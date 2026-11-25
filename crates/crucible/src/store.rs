//! The ClickHouse side of `crucible serve`: where the credentials come from, the migrations that `--migrate`
//! applies, and the startup checks of contract 5.8.
//!
//! - The operator supplies `crucible_admin` (contract 7.1). The passwords of the four generated service
//!   identities are derived from the master key (contract 11.2) by HKDF-SHA256 with the identity name and a
//!   generation counter, so nothing is stored. The info string is
//!   `crucible/clickhouse-identity/v1/<name>/<generation>` and the password is the 32 output bytes as lowercase
//!   hex.
//! - `--migrate` and `crucible migrate` apply schema step `0001_schema_base`, the access steps
//!   `0002_access_service` and `0003_access_workspace` (the SQL workspace's role, profile, quota, static
//!   policies, views and probe identities) and schema steps `0004_span_usage_aggregate` and
//!   `0005_mcp_tool_rollup_hourly` for a single-node server ([`apply_migrations`]). All are idempotent. With
//!   Postgres configured they are recorded in the ledger of contract 6.6 and applied once, and once more after
//!   their content changes, and the probe identities are provisioned again at every run with the passwords of the
//!   current master key; without Postgres there is no ledger and all are applied each time.
//! - [`bring_up`] is everything that must hold before ingest is accepted. With Postgres, a server started without
//!   `--migrate` waits until the ledger records every step.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crucible_env::{
    EngineFamily, Env, SqlWorkspaceStore as _, StoreError, StoreFacts, StoreFlavour, StoreHealth,
    StoreVersion, TelemetryStore, TlsRoots,
};
use crucible_plan::profile_caps;
use crucible_schema::{
    AccessParams, Granularity, Ident, MigrationStep, PROBE_PROJECTS, PasswordHash,
    RetentionSettings, ServiceIdentity, SqlProfile, TABLES, Variant, WorkspaceParams,
    access_service, access_workspace, schema_base, schema_mcp_tool_rollup_hourly,
    schema_span_usage_aggregate, workspace_user,
};

use crate::control::{ApplyStep, Applying, Control, Deferred, Refusal};
use crucible_store_clickhouse::{
    ClickHouseSqlStore, ClickHouseStore, ClusterFacts, Credentials, SqlPasswords, SqlStoreConfig,
    StoreConfig,
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

/// The generation of the derived passwords. Rotation (`crucible admin rotate-keys`) will raise it.
const IDENTITY_GENERATION: u32 = 0;

/// How long opening a connection to ClickHouse may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A 32-byte master key (contract 11.2).
#[derive(Clone)]
pub(crate) struct MasterKey([u8; 32]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

impl MasterKey {
    /// Parses 64 hexadecimal characters; surrounding whitespace is ignored. The error never quotes the input.
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        anyhow::ensure!(
            text.len() == 64 && text.is_ascii(),
            "the master key must be 64 hexadecimal characters (32 bytes)"
        );
        let mut key = [0u8; 32];
        for (byte, pair) in key.iter_mut().zip(text.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).unwrap_or_default();
            *byte = u8::from_str_radix(pair, 16)
                .ok()
                .context("the master key must be 64 hexadecimal characters (32 bytes)")?;
        }
        Ok(Self(key))
    }

    /// The password of a generated service identity (contract 7.1).
    fn identity_password(&self, identity: ServiceIdentity, generation: u32) -> String {
        self.user_password(identity.name(), generation)
    }

    /// The password of a derived ClickHouse user, a service identity or a project's workspace identity
    /// (contract 7.1): HKDF-SHA256 of the master key with the user name and the generation.
    fn user_password(&self, user: &str, generation: u32) -> String {
        let info = format!("crucible/clickhouse-identity/v1/{user}/{generation}");
        let mut okm = [0u8; 32];
        // 32 bytes is within what HKDF-SHA256 can give, so `expand` cannot fail.
        let _ = Hkdf::<Sha256>::new(None, &self.0).expand(info.as_bytes(), &mut okm);
        hex(&okm)
    }

    /// The cursor-signing key (contract 12.4) until signing keys live in the control plane: the HKDF-SHA256
    /// subkey with the info string `crucible/cursor-signing/v1/<generation>`.
    pub(crate) fn cursor_key(&self, generation: u32) -> [u8; 32] {
        let info = format!("crucible/cursor-signing/v1/{generation}");
        let mut okm = [0u8; 32];
        // 32 bytes is within what HKDF-SHA256 can give, so `expand` cannot fail.
        let _ = Hkdf::<Sha256>::new(None, &self.0).expand(info.as_bytes(), &mut okm);
        okm
    }

    /// A 32-byte HKDF-SHA256 subkey for `info` (contract 11.2: one subkey per purpose).
    fn subkey(&self, info: &str) -> [u8; 32] {
        let mut okm = [0u8; 32];
        // 32 bytes is within what HKDF-SHA256 can give, so `expand` cannot fail.
        let _ = Hkdf::<Sha256>::new(None, &self.0).expand(info.as_bytes(), &mut okm);
        okm
    }

    /// The subkey that seals secrets at rest: `crucible/seal/v1`.
    pub(crate) fn seal_key(&self) -> [u8; 32] {
        self.subkey("crucible/seal/v1")
    }

    /// The subkey of the API keys' lookup tags (contract 11.3): `crucible/api-key-lookup/v1`.
    pub(crate) fn lookup_key(&self) -> [u8; 32] {
        self.subkey("crucible/api-key-lookup/v1")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The passwords of the SQL workspace's per-project identities: nothing is stored, and a rotation is a
/// generation bump.
#[derive(Debug)]
struct WorkspacePasswords(MasterKey);

impl SqlPasswords for WorkspacePasswords {
    fn password(&self, user: &str, generation: u32) -> String {
        self.0.user_password(user, generation)
    }
}

/// What `crucible serve` needs to reach ClickHouse.
#[derive(Clone, Debug)]
pub(crate) struct ClickHouse {
    pub(crate) url: String,
    pub(crate) admin: Credentials,
    pub(crate) master_key: MasterKey,
    /// `sql.statements_per_hour`: the hourly allowance of each SQL workspace identity.
    pub(crate) sql_statements_per_hour: u64,
    /// `clickhouse.idle_timeout`; `None` lets detection derive it from the server.
    pub(crate) idle_timeout: Option<Duration>,
    /// `clickhouse.cluster`: the cluster of a replicated ClickHouse, one shard with several replicas; `None` for
    /// one server.
    pub(crate) cluster: Option<Ident>,
}

impl ClickHouse {
    /// The settings of the `[clickhouse]` section, with the `[sql]` section's allowance.
    pub(crate) fn new(
        config: &crucible_config::ClickHouse,
        sql: crucible_config::Sql,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            url: config.url.clone(),
            admin: Credentials {
                user: config.user.clone(),
                password: config.password.expose().to_owned(),
            },
            master_key: MasterKey::parse(config.master_key.expose())?,
            sql_statements_per_hour: sql.statements_per_hour,
            idle_timeout: config.idle_timeout,
            cluster: config
                .cluster
                .as_deref()
                .map(Ident::new)
                .transpose()
                .context("clickhouse.cluster is not a plain identifier")?,
        })
    }

    /// The variant every step and every workspace identity renders for: the cluster's when one is configured.
    pub(crate) fn variant(&self) -> Variant {
        match &self.cluster {
            Some(cluster) => Variant::Clustered {
                cluster: cluster.clone(),
            },
            None => Variant::SingleNode,
        }
    }

    fn generated(&self, identity: ServiceIdentity) -> Credentials {
        Credentials {
            user: identity.name().to_owned(),
            password: self
                .master_key
                .identity_password(identity, IDENTITY_GENERATION),
        }
    }

    /// The store, with one client per service identity. No connection is opened yet.
    pub(crate) fn store(&self, env: &Env) -> ClickHouseStore {
        ClickHouseStore::new(
            env.clone(),
            StoreConfig {
                url: self.url.clone(),
                read_url: None,
                admin: self.admin.clone(),
                ingest: self.generated(ServiceIdentity::Ingest),
                read_ui: self.generated(ServiceIdentity::ReadUi),
                read_agent: self.generated(ServiceIdentity::ReadAgent),
                maint: self.generated(ServiceIdentity::Maint),
                // `clickhouse.flavor = auto`: detection reads it from the server.
                flavour: None,
                cluster: self.cluster.clone(),
                tls_roots: TlsRoots::WebPki,
                connect_timeout: CONNECT_TIMEOUT,
                idle_timeout: self.idle_timeout,
            },
        )
    }

    fn access_params(&self) -> anyhow::Result<AccessParams> {
        let hash = |identity| {
            let digest = Sha256::digest(self.generated(identity).password.as_bytes());
            PasswordHash::new(&hex(&digest)).context("hashing an identity password")
        };
        let caps = |identity| profile_caps(identity).context("a read identity has class limits");
        Ok(AccessParams {
            admin_user: Ident::new(&self.admin.user)
                .context("the ClickHouse admin user is not a plain identifier")?,
            ingest: hash(ServiceIdentity::Ingest)?,
            read_ui: hash(ServiceIdentity::ReadUi)?,
            read_agent: hash(ServiceIdentity::ReadAgent)?,
            maint: hash(ServiceIdentity::Maint)?,
            read_ui_caps: caps(ServiceIdentity::ReadUi)?,
            read_agent_caps: caps(ServiceIdentity::ReadAgent)?,
        })
    }

    /// The SQL workspace's store over `store`'s connection pool (contract 2.9).
    pub(crate) fn sql_store(&self, store: &ClickHouseStore) -> anyhow::Result<ClickHouseSqlStore> {
        Ok(ClickHouseSqlStore::new(
            store.clone(),
            SqlStoreConfig {
                passwords: Arc::new(WorkspacePasswords(self.master_key.clone())),
                variant: self.variant(),
                admin_user: Ident::new(&self.admin.user)
                    .context("the ClickHouse admin user is not a plain identifier")?,
                statements_per_hour: self.sql_statements_per_hour,
            },
        ))
    }

    /// The inputs of access step A2: the profile's deployment defaults and the two probe identities at
    /// generation 0.
    fn workspace_params(&self, profile: SqlProfile) -> anyhow::Result<WorkspaceParams> {
        let hash = |project| {
            let password = self.master_key.user_password(&workspace_user(project), 0);
            PasswordHash::new(&hex(&Sha256::digest(password.as_bytes())))
                .context("hashing a probe identity password")
        };
        Ok(WorkspaceParams {
            admin_user: Ident::new(&self.admin.user)
                .context("the ClickHouse admin user is not a plain identifier")?,
            profile,
            statements_per_hour: self.sql_statements_per_hour,
            probe: [hash(PROBE_PROJECTS[0])?, hash(PROBE_PROJECTS[1])?],
        })
    }

    /// The steps `--migrate` applies, in ledger order, for the configured variant. `profile` is the SQL
    /// workspace's settings profile, fitted to the server.
    pub(crate) fn steps(&self, profile: SqlProfile) -> anyhow::Result<[MigrationStep; 5]> {
        let variant = self.variant();
        let admin = Ident::new(&self.admin.user)
            .context("the ClickHouse admin user is not a plain identifier")?;
        Ok([
            schema_base(&variant, Granularity::Day),
            access_service(&variant, &self.access_params()?),
            access_workspace(&variant, &self.workspace_params(profile)?),
            schema_span_usage_aggregate(&variant),
            schema_mcp_tool_rollup_hourly(&variant, &admin),
        ])
    }
}

impl ApplyStep for ClickHouseStore {
    fn apply<'a>(&'a self, step: &'a MigrationStep) -> Applying<'a> {
        Box::pin(async move {
            self.apply_migration(step)
                .await
                .with_context(|| format!("migration step {}", step.id))
        })
    }

    /// `crucible_maint` signs in with this node's master key, or the health check says why not.
    fn refusal(&self) -> Refusal<'_> {
        Box::pin(async move {
            match self.health().await {
                StoreHealth::Healthy => None,
                StoreHealth::Unavailable { reason } => Some(reason),
            }
        })
    }
}

/// Applies the ClickHouse schema and access steps. With a control plane, the Postgres migrations run first and
/// the ledger of contract 6.6 (`schema_migrations`) decides which steps are still to apply, and then the probe
/// identities are provisioned again ([`provision_probes`]). Without one there is no ledger: every step is applied
/// each time, which their idempotence allows. Answers the changed steps that wait for another master key.
pub(crate) async fn apply_migrations(
    env: &Env,
    config: &ClickHouse,
    store: &ClickHouseStore,
    control: Option<&Control>,
) -> anyhow::Result<Option<Deferred>> {
    let silent = config.check_cluster(store).await?;
    let sql = config.sql_store(store)?;
    let profile = sql
        .server_profile(SqlProfile::default())
        .await
        .context("reading which settings the server knows")?;
    let steps = config.steps(profile)?;
    match control {
        Some(control) => {
            let deferred = control.migrate(env, &steps, store).await?;
            if silent.is_empty() {
                provision_probes(store, &sql).await?;
            } else {
                // A step that was still to apply has refused above; the reconciler repairs the probe identities
                // once every replica answers.
                tracing::warn!(
                    replicas = silent.join(", "),
                    "the SQL workspace's probe users are provisioned again once every replica answers"
                );
            }
            Ok(deferred)
        }
        None => {
            for step in &steps {
                store.apply(step).await?;
                tracing::info!(step = step.id, "migration step applied");
            }
            Ok(None)
        }
    }
}

/// Provisions the SQL workspace's two probe identities (contract 7.5) with the passwords of this node's master key.
/// The ledger applies access step A2, which creates them, only once, so a deployment whose probe identities hold
/// another key's passwords (an earlier version applied A2 again at a start with a wrong key) is repaired here, and
/// by the identity reconciler. A node whose key the service identities refuse leaves them as they are.
async fn provision_probes(store: &ClickHouseStore, sql: &ClickHouseSqlStore) -> anyhow::Result<()> {
    if let StoreHealth::Unavailable { reason } = store.health().await {
        tracing::warn!(
            reason,
            "the service identities refuse this master key, so the SQL workspace's probe identities keep their \
             passwords; start with the master key this deployment was migrated with"
        );
        return Ok(());
    }
    for project in PROBE_PROJECTS {
        sql.provision(project, 0, &RetentionSettings::default())
            .await
            .with_context(|| {
                format!("provisioning the SQL workspace's probe identity {project}")
            })?;
    }
    Ok(())
}

/// The refusal of a cluster whose replica lacks the server setting that `ON CLUSTER` text-index DDL needs.
const DDL_USER_REFUSAL: &str = "the server setting distributed_ddl_use_initial_user_and_roles is 0; set it to 1 on \
                                every replica in the cluster, because the ON CLUSTER statements that create the text \
                                indexes need it";

/// Why the configured cluster cannot hold Crucible's tables (contract 5.8), from what the replica that
/// `clickhouse.url` names says before any `ON CLUSTER` statement runs; `None` when it can. A replica that is down
/// is not a refusal: an insert needs a majority of the replicas, and a migration step says which replica it needs.
fn cluster_refusal(cluster: &Ident, facts: &ClusterFacts) -> Option<String> {
    if facts.cloud {
        return Some(CLOUD_REFUSAL.to_owned());
    }
    if facts.shards == 0 {
        return Some(format!(
            "the server's remote_servers defines no cluster named {cluster}; set clickhouse.cluster \
             (--clickhouse-cluster or CRUCIBLE_CLICKHOUSE_CLUSTER) to the name of the cluster this server belongs to, \
             or define {cluster} in remote_servers on every replica"
        ));
    }
    if facts.shards > 1 {
        return Some(format!(
            "cluster {cluster} has {} shards, and Crucible keeps its tables on one shard with several replicas; \
             name a cluster of one shard in clickhouse.cluster",
            facts.shards
        ));
    }
    if !facts.missing_macros.is_empty() {
        return Some(format!(
            "the server defines no macro {}, which places each replica in the replication path; define the macros \
             shard and replica under <macros> on every replica",
            facts.missing_macros.join(" or ")
        ));
    }
    if let Some(reason) = &facts.keeper {
        return Some(format!(
            "the server reaches no ClickHouse Keeper, which replicated tables need ({reason}); configure \
             <zookeeper> on every replica and check that Keeper answers"
        ));
    }
    if !facts.ddl_initial_user_and_roles {
        return Some(DDL_USER_REFUSAL.to_owned());
    }
    if !facts.unreplicated.is_empty() {
        let tables: Vec<&str> = facts.unreplicated.iter().map(String::as_str).collect();
        return Some(unreplicated_refusal(cluster, &tables));
    }
    if let Some(reason) = &facts.fan_out {
        return Some(format!(
            "the replicas in cluster {cluster} do not run each other's queries as the user who sent them ({reason}); \
             give the cluster a <secret> in remote_servers on every replica, and let every replica reach the others"
        ));
    }
    None
}

impl ClickHouse {
    /// Refuses a configured cluster that cannot hold Crucible's tables, and ClickHouse Cloud, before any step
    /// renders `ON CLUSTER`; warns about the replicas that do not answer, and returns them. Nothing happens without
    /// a cluster name.
    async fn check_cluster(&self, store: &ClickHouseStore) -> anyhow::Result<Vec<String>> {
        let Some(cluster) = &self.cluster else {
            return Ok(Vec::new());
        };
        let facts = store
            .cluster_facts(cluster)
            .await
            .with_context(|| format!("reading cluster {cluster} from the server"))?;
        if let Some(reason) = cluster_refusal(cluster, &facts) {
            anyhow::bail!(reason);
        }
        if !facts.silent.is_empty() {
            tracing::warn!(
                cluster = cluster.as_str(),
                replicas = facts.silent.join(", "),
                "replicas in the cluster do not answer: an insert waits for a majority of the replicas, and a \
                 migration step for every one"
            );
        }
        Ok(facts.silent)
    }
}

/// The refusal of a ClickHouse Cloud service, which Crucible does not support.
const CLOUD_REFUSAL: &str = "this server is a ClickHouse Cloud service, which Crucible does not support; point \
                             clickhouse.url (--clickhouse-url or CRUCIBLE_CLICKHOUSE_URL) at a self-hosted ClickHouse: \
                             a single server, or one of the replicas in the cluster that clickhouse.cluster names";

/// The tables in `engines` that are not replicated, by name.
fn unreplicated(engines: &[(crucible_records::TableId, EngineFamily)]) -> Vec<&'static str> {
    engines
        .iter()
        .filter(|(_, family)| *family != EngineFamily::Replicated)
        .map(|(id, _)| id.name())
        .collect()
}

/// The refusal of a cluster name over tables that were created for a single server.
fn unreplicated_refusal(cluster: &Ident, tables: &[&str]) -> String {
    format!(
        "clickhouse.cluster names cluster {cluster}, but the tables {} in database crucible are not replicated: they \
         were created for a single server. Unset clickhouse.cluster to use this server alone, or point \
         clickhouse.url at a replica in a cluster whose database crucible holds no tables yet",
        tables.join(", ")
    )
}

/// Why a server cannot take ingest although detection ran (contract 5.8); `None` when it can. `cluster` is the
/// configured `clickhouse.cluster`.
fn refusal(facts: &StoreFacts, cluster: Option<&Ident>) -> Option<String> {
    let StoreVersion { major, minor } = facts.version;
    if facts.version < StoreVersion::MINIMUM {
        return Some(format!(
            "ClickHouse {major}.{minor} is older than the minimum, 26.3"
        ));
    }
    match (facts.flavour, cluster) {
        (StoreFlavour::Cloud, _) => return Some(CLOUD_REFUSAL.to_owned()),
        (StoreFlavour::Clustered, None) => {
            return Some(
                "the tables in database crucible are replicated, so this server belongs to a cluster; set \
                 clickhouse.cluster (--clickhouse-cluster or CRUCIBLE_CLICKHOUSE_CLUSTER) to the cluster's name in \
                 the servers' remote_servers"
                    .to_owned(),
            );
        }
        (StoreFlavour::Clustered, Some(cluster)) => {
            let tables = unreplicated(&facts.engines);
            if !tables.is_empty() {
                return Some(unreplicated_refusal(cluster, &tables));
            }
            if facts.ddl_initial_user_and_roles == Some(false) {
                return Some(DDL_USER_REFUSAL.to_owned());
            }
        }
        (StoreFlavour::SingleNode, _) => {}
    }
    if facts.engines.len() < TABLES.len() {
        return Some(format!(
            "{} of {} tables exist; start once with --migrate",
            facts.engines.len(),
            TABLES.len()
        ));
    }
    if !facts.missing_settings.is_empty() {
        return Some(format!(
            "the server lacks the settings {}",
            facts.missing_settings.join(", ")
        ));
    }
    if !facts.text_search_functions || !facts.text_index {
        return Some("the server lacks the text index or the text-search functions".to_owned());
    }
    None
}

/// Everything that must hold before a node accepts ingest: the migrations when `migrate` asks for them (under the
/// ledger of `control`, when Postgres is configured), or else a ledger that records every step; then startup
/// detection and the two self-tests of contract 5.8. On `Err` ingest stays closed and the text says why.
pub(crate) async fn bring_up(
    env: &Env,
    config: &ClickHouse,
    store: &ClickHouseStore,
    control: Option<&Control>,
    migrate: bool,
) -> anyhow::Result<StoreFacts> {
    if migrate {
        // A changed step that waits for the right master key was logged when it was passed over; detection below
        // says why the key is refused.
        apply_migrations(env, config, store, control).await?;
    } else {
        let _silent = config.check_cluster(store).await?;
        if let Some(control) = control {
            crate::control::ensure_migrated(&control.store).await?;
        }
    }
    // `detect` includes the check that every service identity sees a known row.
    let facts = store.detect().await.map_err(|error| {
        let hint = match &error {
            // AUTHENTICATION_FAILED for a generated identity.
            StoreError::Rejected { code: 516, .. } => {
                " (the service users are missing, or were created under a different master key; see --migrate \
                 and --master-key-file)"
            }
            _ => "",
        };
        anyhow::anyhow!("startup detection{hint}: {error}")
    })?;
    if let Some(reason) = refusal(&facts, config.cluster.as_ref()) {
        anyhow::bail!(reason);
    }
    store
        .dedup_selftest()
        .await
        .context("the deduplication self-test")?;
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use crucible_env::EngineFamily;

    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    /// A server on a fresh local port that takes every connection, reads the request, writes `answer` and then
    /// holds the connection open without another byte.
    async fn holding_server(env: &Env, answer: &'static [u8]) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut listener = env
            .net
            .listen(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(env.spawn("holding-server", async move {
            let mut held = Vec::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                if !answer.is_empty() {
                    let mut request = [0; 1 << 16];
                    let _ = stream.read(&mut request).await;
                    let _ = stream.write_all(answer).await;
                }
                held.push(stream);
            }
        }));
        addr
    }

    /// A ClickHouse that holds the connection and answers nothing (stopped behind a name that resolves slowly, or
    /// paused) fails a read with `telemetry_store_unavailable` once the read's deadline passes, so trace detail
    /// answers from the recent-span index, as the trace list does (contract 8.6). It once ended with
    /// `query_timeout`, and trace detail answered 504. A read the server began to answer and did not finish stays
    /// `query_timeout`: the server's own limit should have ended it first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_the_store_never_answers_is_unavailable() {
        use crucible_env::QueryError;
        use crucible_plan::{Caller, RetentionClamp, trace_time_bounds};
        use crucible_records::{ProjectId, TimeWindow, Timestamp, TraceId};

        let config = crucible_env_prod::ProdConfig {
            roots: std::collections::BTreeMap::new(),
            wal_backend: crucible_env_prod::WalBackend::Auto,
        };
        let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &config).unwrap();
        let env = &prod.env;
        let project = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
        let keys = crate::keys::parse(
            &format!(
                "[[project_key]]\nproject_id = \"{project}\"\npublic_key = \"cpk_{0}\"\nsecret = \"csk_{0}\"\n",
                "7".repeat(43)
            ),
            env,
        )
        .unwrap();
        let scope = keys
            .project_scope(ProjectId::parse(project).unwrap())
            .unwrap();
        let all = TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE).unwrap();
        let clamp = RetentionClamp::new(Default::default(), env.clock.wall_now());
        let trace = TraceId::from_bytes([1; 16]);
        let plan = trace_time_bounds(scope, all, &clamp, Caller::Session, trace)
            .unwrap()
            .unwrap();
        let store = |addr: std::net::SocketAddr| {
            let user = || Credentials {
                user: "crucible".to_owned(),
                password: "x".to_owned(),
            };
            ClickHouseStore::new(
                env.clone(),
                StoreConfig {
                    url: format!("http://{addr}"),
                    read_url: None,
                    admin: user(),
                    ingest: user(),
                    read_ui: user(),
                    read_agent: user(),
                    maint: user(),
                    flavour: Some(StoreFlavour::SingleNode),
                    cluster: None,
                    tls_roots: TlsRoots::WebPki,
                    connect_timeout: CONNECT_TIMEOUT,
                    idle_timeout: None,
                },
            )
        };

        let silent = holding_server(env, b"").await;
        let read = store(silent)
            .execute(plan.clone())
            .await
            .unwrap()
            .rows()
            .await;
        assert!(matches!(read, Err(QueryError::Unavailable(_))), "{read:?}");

        // The answer begins with the column count of the plan's six outputs, then stops before their names.
        let stalling = holding_server(
            env,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n\x06\r\n",
        )
        .await;
        let read = store(stalling).execute(plan).await.unwrap().rows().await;
        assert_eq!(read, Err(QueryError::Timeout));
    }

    #[test]
    fn master_keys_are_64_hex_characters_and_never_echoed() {
        assert!(MasterKey::parse(&format!("  {KEY}\n")).is_ok());
        for bad in [
            "",
            "abc",
            &KEY[2..],
            &format!("{KEY}00"),
            &KEY.replace('f', "g"),
        ] {
            let error = format!("{:#}", MasterKey::parse(bad).unwrap_err());
            assert!(error.contains("64 hexadecimal"), "{error}");
            assert!(bad.len() < 8 || !error.contains(&bad[..8]), "{error}");
        }
        assert_eq!(
            format!("{:?}", MasterKey::parse(KEY).unwrap()),
            "MasterKey(..)"
        );
    }

    /// RFC 5869, test case 3 (SHA-256, no salt, no info), pins the primitive; the rest pins the info layout.
    #[test]
    fn identity_passwords_are_hkdf_sha256_of_the_master_key() {
        let mut okm = [0u8; 42];
        Hkdf::<Sha256>::new(None, &[0x0b; 22])
            .expand(&[], &mut okm)
            .unwrap();
        assert_eq!(
            hex(&okm),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d\
             9d201395faa4b61a96c8"
        );

        let key = MasterKey::parse(KEY).unwrap();
        let mut expected = [0u8; 32];
        Hkdf::<Sha256>::new(None, &key.0)
            .expand(
                b"crucible/clickhouse-identity/v1/crucible_ingest/0",
                &mut expected,
            )
            .unwrap();
        let ingest = key.identity_password(ServiceIdentity::Ingest, 0);
        assert_eq!(ingest, hex(&expected));
        assert_eq!(ingest.len(), 64);
        // Every identity and every generation has its own password.
        let mut passwords: Vec<String> = ServiceIdentity::GENERATED
            .into_iter()
            .flat_map(|identity| {
                [0, 1].map(|generation| key.identity_password(identity, generation))
            })
            .collect();
        passwords.sort();
        passwords.dedup();
        assert_eq!(passwords.len(), 8);
        let other = MasterKey::parse(&KEY.replace("00", "ff")).unwrap();
        assert_ne!(other.identity_password(ServiceIdentity::Ingest, 0), ingest);
    }

    /// The expected key comes from a separate HKDF-SHA256 (Python's `hmac`), so the info layout is pinned.
    #[test]
    fn the_cursor_key_is_a_subkey_of_its_own() {
        let key = MasterKey::parse(KEY).unwrap();
        assert_eq!(
            hex(&key.cursor_key(0)),
            "1dea993f14702c1e428e5bef7c99f27b38442a1b47505aab32a5a16f383cefd1"
        );
        assert_ne!(key.cursor_key(1), key.cursor_key(0));
        assert_ne!(
            hex(&key.cursor_key(0)),
            key.identity_password(ServiceIdentity::Ingest, 0)
        );
    }

    #[test]
    fn migrate_renders_the_base_schema_then_the_service_access_step() {
        let config = ClickHouse {
            url: "http://localhost:8123".to_owned(),
            admin: Credentials {
                user: "crucible_admin".to_owned(),
                password: "admin-secret".to_owned(),
            },
            master_key: MasterKey::parse(KEY).unwrap(),
            sql_statements_per_hour: 90,
            idle_timeout: None,
            cluster: None,
        };
        let steps = config.steps(SqlProfile::default()).unwrap();
        // `sql.statements_per_hour` reaches the quota of access step A2.
        assert!(steps[2].statements.iter().any(|statement| {
            statement.starts_with("ALTER QUOTA crucible_sql_quota")
                && statement.contains("MAX queries = 90,")
        }));
        assert_eq!(
            steps.each_ref().map(|step| step.id),
            [
                crucible_schema::SCHEMA_BASE,
                crucible_schema::ACCESS_SERVICE,
                crucible_schema::ACCESS_WORKSPACE,
                crucible_schema::SCHEMA_SPAN_USAGE_AGGREGATE,
                crucible_schema::SCHEMA_MCP_TOOL_ROLLUP_HOURLY,
            ]
        );
        // The probe identities' passwords are derived like every other, and only their digests are sent.
        let workspace = steps[2].statements.join(
            "
",
        );
        for project in PROBE_PROJECTS {
            let password = config.master_key.user_password(&workspace_user(project), 0);
            assert!(!workspace.contains(&password));
            assert!(workspace.contains(&hex(&Sha256::digest(password.as_bytes()))));
        }
        let sql = steps[1].statements.join("\n");
        // Only digests reach a statement: neither a derived password nor the admin's.
        for identity in ServiceIdentity::GENERATED {
            assert!(!sql.contains(&config.generated(identity).password));
        }
        assert!(!sql.contains("admin-secret"));
        assert!(format!("{config:?}").contains("MasterKey(..)"));
        assert!(!format!("{config:?}").contains("admin-secret"));
    }

    /// One start with a wrong master key under `--migrate` must find every step a start with the right key
    /// recorded, or it applies access step A2 again and gives the probe identities the wrong key's passwords. The
    /// steps differ only in their password digests, which the ledger checksum leaves out (contract 6.6).
    #[test]
    fn the_ledger_checksum_does_not_depend_on_the_master_key() {
        let config = |key: &str| ClickHouse {
            url: "http://localhost:8123".to_owned(),
            admin: Credentials {
                user: "crucible_admin".to_owned(),
                password: "admin-secret".to_owned(),
            },
            master_key: MasterKey::parse(key).unwrap(),
            sql_statements_per_hour: 1_800,
            idle_timeout: None,
            cluster: None,
        };
        let right = config(KEY).steps(SqlProfile::default()).unwrap();
        let wrong = config(&KEY.replace("00", "ff"))
            .steps(SqlProfile::default())
            .unwrap();
        // The access steps carry the digests of the derived passwords, so they differ under another key.
        assert_ne!(right[1].statements, wrong[1].statements);
        assert_ne!(right[2].statements, wrong[2].statements);
        for (right, wrong) in right.iter().zip(&wrong) {
            assert_eq!(
                crate::control::checksum(right),
                crate::control::checksum(wrong),
                "the ledger checksum of step {} depends on the master key",
                right.id
            );
        }
    }

    #[test]
    fn ingest_is_refused_until_the_server_fits() {
        let fits = StoreFacts {
            version: StoreVersion {
                major: 26,
                minor: 8,
            },
            flavour: StoreFlavour::SingleNode,
            engines: TABLES
                .iter()
                .map(|def| (def.id, EngineFamily::MergeTree))
                .collect(),
            missing_settings: Vec::new(),
            text_search_functions: true,
            text_index: true,
            ddl_initial_user_and_roles: None,
        };
        assert_eq!(refusal(&fits, None), None);
        let refused = |change: fn(&mut StoreFacts)| {
            let mut facts = fits.clone();
            change(&mut facts);
            refusal(&facts, None).unwrap()
        };
        assert!(refused(|f| f.version.minor = 2).contains("older than the minimum"));
        assert!(refused(|f| f.engines.clear()).contains("--migrate"));
        assert!(refused(|f| f.missing_settings.push("x".to_owned())).contains("settings x"));
        assert!(refused(|f| f.text_index = false).contains("text index"));

        // A cluster of replicated tables, with its name configured, fits.
        let cluster = Ident::new("crucible").unwrap();
        let replicated = StoreFacts {
            flavour: StoreFlavour::Clustered,
            engines: TABLES
                .iter()
                .map(|def| (def.id, EngineFamily::Replicated))
                .collect(),
            ddl_initial_user_and_roles: Some(true),
            ..fits.clone()
        };
        assert_eq!(refusal(&replicated, Some(&cluster)), None);
        // Each refusal states what is wrong and the setting that fixes it (contract 1.7).
        let unnamed = refusal(&replicated, None).unwrap();
        assert!(
            unnamed.contains("replicated")
                && unnamed.contains("clickhouse.cluster")
                && unnamed.contains("--clickhouse-cluster")
                && unnamed.contains("CRUCIBLE_CLICKHOUSE_CLUSTER"),
            "{unnamed}"
        );
        let single_tables = refusal(
            &StoreFacts {
                flavour: StoreFlavour::Clustered,
                ..fits.clone()
            },
            Some(&cluster),
        )
        .unwrap();
        assert!(
            single_tables.contains("not replicated")
                && single_tables.contains("clickhouse.cluster"),
            "{single_tables}"
        );
        let ddl = refusal(
            &StoreFacts {
                ddl_initial_user_and_roles: Some(false),
                ..replicated.clone()
            },
            Some(&cluster),
        )
        .unwrap();
        assert!(
            ddl.contains("distributed_ddl_use_initial_user_and_roles"),
            "{ddl}"
        );
        // ClickHouse Cloud stays refused, with or without a cluster name.
        for named in [None, Some(&cluster)] {
            let cloud = refusal(
                &StoreFacts {
                    flavour: StoreFlavour::Cloud,
                    ..fits.clone()
                },
                named,
            )
            .unwrap();
            assert!(
                cloud.contains("ClickHouse Cloud")
                    && cloud.contains("clickhouse.url")
                    && !cloud.contains("preview"),
                "{cloud}"
            );
        }
    }

    /// A configured cluster that cannot hold the tables is refused before any `ON CLUSTER` statement runs, each
    /// refusal naming the fix (contract 1.7); a replica that does not answer is not a refusal.
    #[test]
    fn a_cluster_is_refused_until_it_can_hold_the_tables() {
        let cluster = Ident::new("crucible").unwrap();
        let fits = ClusterFacts {
            cloud: false,
            shards: 1,
            replicas: vec![
                "clickhouse-1:9000".to_owned(),
                "clickhouse-2:9000".to_owned(),
            ],
            missing_macros: Vec::new(),
            keeper: None,
            ddl_initial_user_and_roles: true,
            unreplicated: Vec::new(),
            fan_out: None,
            silent: Vec::new(),
        };
        assert_eq!(cluster_refusal(&cluster, &fits), None);
        let one_down = ClusterFacts {
            silent: vec!["clickhouse-2:9000".to_owned()],
            ..fits.clone()
        };
        assert_eq!(cluster_refusal(&cluster, &one_down), None);
        let refused = |change: fn(&mut ClusterFacts)| {
            let mut facts = fits.clone();
            change(&mut facts);
            cluster_refusal(&cluster, &facts).unwrap()
        };
        let cloud = refused(|f| f.cloud = true);
        assert!(
            cloud.contains("ClickHouse Cloud") && cloud.contains("clickhouse.url"),
            "{cloud}"
        );
        let undefined = refused(|f| f.shards = 0);
        assert!(
            undefined.contains("remote_servers") && undefined.contains("--clickhouse-cluster"),
            "{undefined}"
        );
        assert!(refused(|f| f.shards = 2).contains("one shard"));
        let macros = refused(|f| f.missing_macros = vec!["replica".to_owned()]);
        assert!(
            macros.contains("replica") && macros.contains("<macros>"),
            "{macros}"
        );
        let keeper = refused(|f| f.keeper = Some("Code: 999".to_owned()));
        assert!(
            keeper.contains("Keeper") && keeper.contains("<zookeeper>"),
            "{keeper}"
        );
        assert!(
            refused(|f| f.ddl_initial_user_and_roles = false)
                .contains("distributed_ddl_use_initial_user_and_roles")
        );
        let tables = refused(|f| f.unreplicated = vec!["span_core".to_owned()]);
        assert!(
            tables.contains("span_core") && tables.contains("not replicated"),
            "{tables}"
        );
        let fan_out = refused(|f| f.fan_out = Some("Code: 516".to_owned()));
        assert!(
            fan_out.contains("Code: 516") && fan_out.contains("<secret>"),
            "{fan_out}"
        );
    }

    /// With `clickhouse.cluster` set, every step renders for the cluster: replicated engines, `ON CLUSTER`.
    #[test]
    fn a_cluster_name_renders_every_step_for_the_cluster() {
        let cluster = Ident::new("crucible").unwrap();
        let config = ClickHouse {
            url: "http://clickhouse-1:8123".to_owned(),
            admin: Credentials {
                user: "crucible_admin".to_owned(),
                password: "admin-secret".to_owned(),
            },
            master_key: MasterKey::parse(KEY).unwrap(),
            sql_statements_per_hour: 1_800,
            idle_timeout: None,
            cluster: Some(cluster.clone()),
        };
        let variant = Variant::Clustered { cluster };
        assert_eq!(config.variant(), variant);
        let steps = config.steps(SqlProfile::default()).unwrap();
        assert_eq!(
            steps[0].statements,
            schema_base(&variant, Granularity::Day).statements
        );
        assert_eq!(steps[3], schema_span_usage_aggregate(&variant));
        for step in &steps {
            assert!(
                step.statements
                    .iter()
                    .any(|statement| statement.contains(" ON CLUSTER crucible")),
                "step {} has no ON CLUSTER statement",
                step.id
            );
        }
        let single = ClickHouse {
            cluster: None,
            ..config
        };
        assert_eq!(single.variant(), Variant::SingleNode);
    }
}
