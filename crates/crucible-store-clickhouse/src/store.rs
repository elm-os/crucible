//! [`ClickHouseStore`]: the `TelemetryStore` over the ClickHouse HTTP interface (contract 2.6).

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use clickhouse::query::BytesCursor;
use crucible_chfmt::{Block, Compression, EncodedBlock, UuidColumn};
use crucible_env::{
    ClockExt, EngineFamily, Env, InsertError, InsertSettings, Instant, MutationId, MutationStatus,
    QueryError, RowStream, StoreError, StoreFacts, StoreFlavour, StoreHealth, StoreVersion,
    TelemetryStore, TlsRoots,
};
use crucible_plan::{
    BatchProbe, CLASS_FIXED_SETTINGS, ClassLimits, DeletePlan, OutputSchema, QueryPlan, RowBlock,
};
use crucible_records::{
    BatchToken, Lsn, PartitionId, ProjectId, TableId, Timestamp, Uuid, WalEpoch,
};
use crucible_schema::{
    AGENT_MAX_CONCURRENT_QUERIES, DATABASE, Ident, IndexKind, MigrationStep, ServiceIdentity, table,
};
use futures::Stream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::codes::{self, Failure, parse_code};
use crate::connect;
use crate::decode::Decoder;
use crate::render::{self, Binder, Statement};

/// The Rust-side deadline of an insert: its `max_execution_time` plus slack for the round trip.
const INSERT_DEADLINE: Duration =
    Duration::from_secs(InsertSettings::MAX_EXECUTION_TIME_SECS as u64 + 5);
/// `max_execution_time` of the store's own short reads (probe, catalogue lookups), in seconds.
pub(crate) const LOOKUP_SECS: u64 = 30;
/// `max_execution_time` of the health check and of the reads every identity runs in the visibility check: the
/// smallest ceiling among the identities' settings profiles.
const SHORT_SECS: u64 = 5;
/// The Rust-side deadline of a read is its `max_execution_time` plus this.
const DEADLINE_SLACK_SECS: u64 = crucible_plan::DEADLINE_SLACK_SECS as u64;
/// The Rust-side deadline of `KILL QUERY`.
const KILL_DEADLINE: Duration = Duration::from_secs(LOOKUP_SECS);
/// The Rust-side deadline of a statement that changes data or schema. On expiry the outcome is unknown and the
/// idempotent call is repeated.
pub(crate) const CHANGE_DEADLINE: Duration = Duration::from_secs(300);
/// The first wait of a read of `crucible_read_agent` that the server refused because the identity ran as many
/// statements as its profile allows; each later wait doubles, up to [`AGENT_RETRY_MAX`].
const AGENT_RETRY_FIRST: Duration = Duration::from_millis(50);
/// The longest wait between two sends of a refused read of `crucible_read_agent`.
const AGENT_RETRY_MAX: Duration = Duration::from_secs(1);

/// How long the client waits for each statement of a migration step: [`CHANGE_DEADLINE`], or longer when the
/// step sets its own `max_execution_time`, so that the client never gives up on a statement the server still
/// runs (the copy of the hourly step runs for minutes on a large rollup) and the server's limit ends it first.
fn migration_deadline(step: &MigrationStep) -> Duration {
    step.settings
        .iter()
        .find(|(name, _)| *name == "max_execution_time")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.saturating_add(DEADLINE_SLACK_SECS)))
        .map_or(CHANGE_DEADLINE, |deadline| deadline.max(CHANGE_DEADLINE))
}

/// Every query-level setting this crate sends, apart from the per-insert settings of
/// [`InsertSettings::NAMES`]. Startup detection checks that the server knows each one (contract 5.8).
pub const QUERY_SETTINGS: [&str; 24] = [
    "max_execution_time",
    "max_memory_usage",
    "max_rows_to_read",
    "max_bytes_to_read",
    "max_threads",
    "timeout_overflow_mode",
    "read_overflow_mode",
    "cancel_http_readonly_queries_on_client_close",
    "use_statistics",
    "max_rows_in_set",
    "set_overflow_mode",
    "use_with_fill_by_sorting_prefix",
    "lightweight_deletes_sync",
    "lightweight_delete_mode",
    "mutations_sync",
    "alter_sync",
    "distributed_ddl_entry_format_version",
    "distributed_ddl_output_mode",
    "distributed_ddl_task_timeout",
    // The reads of every replica in a cluster, and the replicated changes that a replica that is down has not run.
    "skip_unavailable_shards",
    "replication_wait_for_inactive_replica_timeout",
    // The copy of the hourly schema step (`crucible_schema::schema_mcp_tool_rollup_hourly`).
    "max_bytes_before_external_group_by",
    "min_insert_block_size_rows",
    "min_insert_block_size_bytes",
];

/// Every table-level setting the schema's DDL sets.
pub const TABLE_SETTINGS: [&str; 5] = [
    "index_granularity",
    "index_granularity_bytes",
    "non_replicated_deduplication_window",
    "replicated_deduplication_window",
    "replicated_deduplication_window_seconds",
];

/// The text-search functions the renderer uses.
pub const TEXT_SEARCH_FUNCTIONS: [&str; 2] = ["hasAllTokens", "hasAnyTokens"];

/// The settings of a lightweight delete: it returns once the rows are masked on every replica, so they are hidden
/// from reads at once, and it never takes the lightweight-update path (contract 6).
const DELETE_SETTINGS: [(&str, &str); 2] = [
    ("lightweight_deletes_sync", "2"),
    ("lightweight_delete_mode", "alter_update"),
];

/// On a cluster, a replicated change waits for every active replica and not for one that is down, which runs it
/// when it returns: the server answers [`UNFINISHED`] at once instead of after two minutes.
const INACTIVE_REPLICAS: (&str, &str) = ("replication_wait_for_inactive_replica_timeout", "0");

/// `UNFINISHED`: a replicated change that a replica that is down has not run yet. The change is queued for it.
const UNFINISHED: u32 = 341;

/// The settings every read carries: the limits of its class and the fixed settings (contracts 8.4 and 8.5).
fn class_settings(limits: ClassLimits) -> Vec<(&'static str, String)> {
    let mut settings = vec![
        (
            "max_execution_time",
            limits.max_execution_time_secs.to_string(),
        ),
        ("max_memory_usage", limits.max_memory_usage.to_string()),
        ("max_threads", limits.max_threads.to_string()),
    ];
    if let Some(rows) = limits.max_rows_to_read {
        settings.push(("max_rows_to_read", rows.to_string()));
    }
    if let Some(bytes) = limits.max_bytes_to_read {
        settings.push(("max_bytes_to_read", bytes.to_string()));
    }
    settings.extend(
        CLASS_FIXED_SETTINGS
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned())),
    );
    settings
}

/// The user and password of one identity.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The user name.
    pub user: String,
    /// The password.
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// What the store needs to know. Everything comes from configuration; nothing is read from the environment.
#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// The HTTP endpoint, `http://host:port` or `https://host:port`.
    pub url: String,
    /// A separate endpoint for the two read identities (`clickhouse.read_url`).
    pub read_url: Option<String>,
    /// `crucible_admin`, supplied by the operator.
    pub admin: Credentials,
    /// `crucible_ingest`.
    pub ingest: Credentials,
    /// `crucible_read_ui`.
    pub read_ui: Credentials,
    /// `crucible_read_agent`.
    pub read_agent: Credentials,
    /// `crucible_maint`.
    pub maint: Credentials,
    /// The configured flavour (`clickhouse.flavor`); `None` is `auto`.
    pub flavour: Option<StoreFlavour>,
    /// The cluster of a replicated deployment (`clickhouse.cluster`), one shard with several replicas, whose
    /// replicas the store reads together where a call must hold on every one of them. `None` for one server.
    pub cluster: Option<Ident>,
    /// The trust anchors for an `https` endpoint.
    pub tls_roots: TlsRoots,
    /// How long opening a connection may take.
    pub connect_timeout: Duration,
    /// How long an idle connection is kept for the next statement (`clickhouse.idle_timeout`), for a balancer in
    /// front of the store that drops idle connections sooner than the server. `None`: detection derives it from
    /// the server's `keep_alive_timeout`.
    pub idle_timeout: Option<Duration>,
}

/// One client per service identity, in the order of `slot`, over one connection pool.
struct Clients {
    /// How long the pool keeps an idle connection.
    idle: Duration,
    by_identity: [clickhouse::Client; 5],
    /// The read endpoint without credentials: workspace identities are attached per statement.
    workspace: clickhouse::Client,
}

impl Clients {
    fn new(env: &Env, config: &StoreConfig, idle: Duration) -> Self {
        let base = connect::client(env, config.tls_roots.clone(), config.connect_timeout, idle);
        let read_url = config.read_url.as_deref().unwrap_or(&config.url);
        let client = |url: &str, credentials: &Credentials| {
            base.clone()
                .with_url(url)
                .with_user(credentials.user.as_str())
                .with_password(credentials.password.as_str())
        };
        Self {
            idle,
            workspace: base.clone().with_url(read_url),
            // In the order of `slot`.
            by_identity: [
                client(&config.url, &config.admin),
                client(&config.url, &config.ingest),
                client(read_url, &config.read_ui),
                client(read_url, &config.read_agent),
                client(&config.url, &config.maint),
            ],
        }
    }
}

struct Inner {
    env: Env,
    config: StoreConfig,
    /// Replaced when detection learns the server's keep-alive timeout, because a pool keeps the idle timeout it
    /// was built with.
    clients: Mutex<Arc<Clients>>,
    flavour: Mutex<Option<StoreFlavour>>,
    /// The statements `crucible_read_agent` may run at a time, which its settings profile caps at
    /// [`AGENT_MAX_CONCURRENT_QUERIES`]: a read of that identity waits here for a slot, so that the server never
    /// refuses it for running too many, however many reads a request sends together.
    agent_slots: Arc<Semaphore>,
}

/// The production `TelemetryStore`: one client per service identity over one connection pool that runs on the
/// `Env` network.
#[derive(Clone)]
pub struct ClickHouseStore {
    inner: Arc<Inner>,
}

impl fmt::Debug for ClickHouseStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouseStore")
            .field("configured_flavour", &self.inner.config.flavour)
            .finish_non_exhaustive()
    }
}

const fn slot(identity: ServiceIdentity) -> usize {
    match identity {
        ServiceIdentity::Admin => 0,
        ServiceIdentity::Ingest => 1,
        ServiceIdentity::ReadUi => 2,
        ServiceIdentity::ReadAgent => 3,
        ServiceIdentity::Maint => 4,
    }
}

/// A tab-separated answer: one `Vec` of fields per line.
fn lines(text: &str) -> Vec<Vec<&str>> {
    text.lines()
        .map(|line| line.split('\t').collect())
        .collect()
}

fn unexpected(what: &str, text: &str) -> StoreError {
    StoreError::Internal(format!("unexpected answer for {what}: {text:?}"))
}

fn count(what: &str, text: &str) -> Result<u64, StoreError> {
    text.trim().parse().map_err(|_| unexpected(what, text))
}

/// The tab-separated numbers of a one-row answer.
fn numbers<const N: usize>(what: &str, text: &str) -> Result<[u64; N], StoreError> {
    let fields: Vec<u64> = text
        .trim()
        .split('\t')
        .map(|field| field.parse().map_err(|_| unexpected(what, text)))
        .collect::<Result<_, _>>()?;
    fields.try_into().map_err(|_| unexpected(what, text))
}

/// The answer of the existence probe on a cluster of `replicas` replicas, of which `answered` answered and
/// `holding` hold the block. The block is present once a majority holds it, as an acknowledged insert with
/// `insert_quorum = auto` leaves it, and absent when every replica answered and none holds it. Between the two
/// the probe cannot tell: a block on fewer replicas waits for its quorum, and a quorum that fails drops the block
/// from the replica that holds it, while a replica that did not answer may hold a block that lands later. The
/// shipper probes again.
fn probe_answer(holding: u64, answered: u64, replicas: u64) -> Result<bool, StoreError> {
    if replicas == 0 {
        return Err(StoreError::Unavailable(
            "the server defines no replica in the configured cluster in remote_servers".to_owned(),
        ));
    }
    if holding > replicas / 2 {
        Ok(true)
    } else if holding == 0 && answered >= replicas {
        Ok(false)
    } else {
        Err(StoreError::Unavailable(format!(
            "{holding} of the cluster's {replicas} replicas hold the block and {answered} answered: the probe \
             answers once a majority holds the block, or once every replica answers without it"
        )))
    }
}

/// One replica's row of a mutation in `system.mutations`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ReplicaMutation<'a> {
    listed: bool,
    done: bool,
    killed: bool,
    reason: &'a str,
}

/// The state of a mutation on a cluster of `replicas` replicas from the rows of those that answered. It is done
/// when every replica has done it, or when none lists it any more; one that did not answer, or that does not list
/// it while another does, has not run it yet.
fn mutation_answer(replicas: u64, rows: &[ReplicaMutation<'_>]) -> MutationStatus {
    if let Some(row) = rows
        .iter()
        .find(|r| r.listed && !r.done && (r.killed || !r.reason.is_empty()))
    {
        return MutationStatus::Failed {
            code: parse_code(row.reason).unwrap_or(0),
            reason: if row.reason.is_empty() {
                "the mutation was killed".to_owned()
            } else {
                row.reason.to_owned()
            },
        };
    }
    let all_answered = rows.len() as u64 >= replicas;
    let gone = rows.iter().all(|r| !r.listed);
    let done = rows.iter().all(|r| r.listed && r.done);
    if all_answered && (gone || done) {
        MutationStatus::Done
    } else {
        MutationStatus::Pending
    }
}

/// In a read of a replica's system tables, whether the replica has the table (`database` and `name` are the
/// placeholders of their bound values). A replica that lacks the table lists no part and no mutation of it, so it
/// counts as one that did not answer.
fn table_present(database: &str, name: &str) -> String {
    format!("(SELECT count() FROM system.tables WHERE database = {database} AND name = {name})")
}

/// `clusterAllReplicas('<cluster>', view(<select>))`: `select` on every replica, one answer row each.
fn on_every_replica(cluster: &Ident, select: &str) -> String {
    format!("clusterAllReplicas('{cluster}', view({select}))")
}

/// What the startup checks of a replicated deployment read through the replica the store talks to, as
/// `crucible_admin`, so that they run before the first migration creates the service identities.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClusterFacts {
    /// Whether the server is a ClickHouse Cloud service (its `cloud_mode` setting).
    pub cloud: bool,
    /// The shards of the configured cluster in the server's `remote_servers`; 0 when no cluster has its name.
    pub shards: u64,
    /// Its replicas as `host:port`, in the order of `remote_servers`.
    pub replicas: Vec<String>,
    /// The macros of the replication path, of `shard` and `replica`, that the server does not define.
    pub missing_macros: Vec<String>,
    /// Why the server holds no Keeper session, when it holds none.
    pub keeper: Option<String>,
    /// The server setting `distributed_ddl_use_initial_user_and_roles`, which `ON CLUSTER` text-index DDL needs.
    pub ddl_initial_user_and_roles: bool,
    /// The tables of database `crucible` whose engine is not replicated, by name.
    pub unreplicated: Vec<String>,
    /// Why a statement sent to every replica failed, when it failed; a replica that is down does not fail it.
    pub fan_out: Option<String>,
    /// The replicas that did not answer a statement sent to every replica, as `host:port`.
    pub silent: Vec<String>,
}

fn engine_family(engine: &str) -> EngineFamily {
    if engine.starts_with("Shared") {
        EngineFamily::Shared
    } else if engine.starts_with("Replicated") {
        EngineFamily::Replicated
    } else {
        EngineFamily::MergeTree
    }
}

fn version(text: &str) -> Option<StoreVersion> {
    let mut parts = text.trim().split('.');
    Some(StoreVersion {
        major: parts.next()?.parse().ok()?,
        minor: parts.next()?.parse().ok()?,
    })
}

/// The server's `keep_alive_timeout` from its row in `system.server_settings`: whole seconds.
fn keep_alive(text: &str) -> Option<Duration> {
    text.trim().parse().ok().map(Duration::from_secs)
}

/// A one-row block for `dedup_selftest`.
fn selftest_block(id: Uuid, epoch: WalEpoch, now: Timestamp) -> Result<EncodedBlock, StoreError> {
    let internal = |e: crucible_chfmt::EncodeError| StoreError::Internal(e.to_string());
    let def = table(TableId::DedupSelftest);
    let mut ids = UuidColumn::with_capacity(1);
    ids.push(id.to_bytes());
    let mut epochs = UuidColumn::with_capacity(1);
    epochs.push(epoch.to_bytes());
    let batch_seq = vec![Lsn::FIRST.get()];
    let created_at = vec![now.micros()];
    let mut block = Block::new(1).map_err(internal)?;
    for column in def.columns {
        let data: &dyn crucible_chfmt::Column = match column.name {
            "id" => &ids,
            "wal_epoch" => &epochs,
            "batch_seq" => &batch_seq,
            "created_at" => &created_at,
            other => {
                return Err(StoreError::Internal(format!(
                    "dedup_selftest has an unknown column {other}"
                )));
            }
        };
        block
            .push(column.name, &column.ty.to_string(), data)
            .map_err(internal)?;
    }
    block.encode(Compression::Lz4).map_err(internal)
}

/// Issues `KILL QUERY` when a read is abandoned: on drop while the statement may still run.
struct KillOnDrop {
    inner: Arc<Inner>,
    query_id: Uuid,
    armed: bool,
}

impl KillOnDrop {
    fn fire(&mut self) {
        if std::mem::take(&mut self.armed) {
            let store = ClickHouseStore {
                inner: Arc::clone(&self.inner),
            };
            let query_id = self.query_id;
            // Detached: the kill outlives the stream that asked for it.
            drop(self.inner.env.spawn("clickhouse-kill", async move {
                if let Err(error) = store.kill(query_id).await {
                    tracing::warn!(%query_id, %error, "kill of an abandoned query failed");
                }
            }));
        }
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.fire();
    }
}

/// The state of one streaming read.
pub(crate) struct Reading {
    cursor: Option<BytesCursor>,
    /// The statement and its output format until the first poll, to send it once more if its request could not
    /// be sent.
    resend: Option<(clickhouse::query::Query, String)>,
    /// Whether the server has sent a byte of its answer.
    answered: bool,
    deadline: Instant,
    kill: Option<KillOnDrop>,
    /// Until the first poll: the slots the statement waits for before it is sent, for an identity whose
    /// concurrent statements the server caps.
    slots: Option<Arc<Semaphore>>,
    /// The slot the statement holds until its answer ends.
    slot: Option<OwnedSemaphorePermit>,
    /// For an identity whose concurrent statements the server caps: the statement and its output format, to send
    /// it again while the server refuses it for running as many as the cap, which other nodes' statements of the
    /// identity can fill.
    refused_resend: Option<(clickhouse::query::Query, String)>,
    inner: Arc<Inner>,
}

impl Reading {
    /// The next chunk of the answer; `None` at its end. An error ends the read.
    pub(crate) async fn next(&mut self) -> Option<Result<Bytes, QueryError>> {
        Some(self.next_chunk().await?.map_err(Failure::into_query))
    }

    /// [`Reading::next`] with the failure before it is mapped to an error type.
    pub(crate) async fn next_chunk(&mut self) -> Option<Result<Bytes, Failure>> {
        self.cursor.as_ref()?;
        let clock = Arc::clone(&self.inner.env.clock);
        // Nothing is sent until the statement holds a slot; the wait counts against its deadline.
        if let Some(slots) = self.slots.take() {
            match clock.timeout_at(self.deadline, slots.acquire_owned()).await {
                Ok(Ok(slot)) => self.slot = Some(slot),
                // The semaphore is never closed, and a statement that found no slot by its deadline was never
                // sent.
                Ok(Err(_)) | Err(_) => {
                    self.cursor = None;
                    return Some(Err(Failure::Timeout));
                }
            }
        }
        let cursor = self.cursor.as_mut()?;
        // The request is sent by the first poll, so from here on the statement may be running.
        if let Some(kill) = &mut self.kill {
            kill.armed = true;
        }
        let first = clock.timeout_at(self.deadline, cursor.next()).await;
        let mut outcome = match (first, self.resend.take()) {
            // No byte of the request was sent, so the read changed nothing: it is sent once more, on a new
            // connection.
            (Ok(Err(error)), Some((query, format))) if codes::not_sent(&error) => {
                match query.fetch_bytes(format) {
                    Ok(fresh) => {
                        *cursor = fresh;
                        clock.timeout_at(self.deadline, cursor.next()).await
                    }
                    Err(error) => Ok(Err(error)),
                }
            }
            (first, _) => first,
        };
        // The server refused the statement because the identity runs as many statements as its profile allows,
        // which other nodes' statements of the identity can fill: nothing ran, so it is sent again after a wait,
        // within its deadline, as a statement waits for a slot of this node's own.
        let mut wait = AGENT_RETRY_FIRST;
        while !self.answered
            && let Ok(Err(error)) = &outcome
            && codes::too_many_queries(error)
            && let Some((query, format)) = &self.refused_resend
        {
            if let Some(kill) = &mut self.kill {
                kill.armed = false;
            }
            if clock.now().saturating_add(wait) >= self.deadline {
                self.cursor = None;
                self.slot = None;
                return Some(Err(Failure::Timeout));
            }
            clock.sleep(wait).await;
            wait = wait.saturating_mul(2).min(AGENT_RETRY_MAX);
            outcome = match query.clone().fetch_bytes(format) {
                Ok(fresh) => {
                    *cursor = fresh;
                    if let Some(kill) = &mut self.kill {
                        kill.armed = true;
                    }
                    clock.timeout_at(self.deadline, cursor.next()).await
                }
                Err(error) => Ok(Err(error)),
            };
        }
        let finished = !matches!(outcome, Ok(Ok(Some(_))));
        if finished {
            // Dropping the cursor closes the connection, which cancels the statement on the server
            // (contract 8.5).
            self.cursor = None;
            self.slot = None;
        }
        match outcome {
            Ok(Ok(Some(chunk))) => {
                self.answered = true;
                Some(Ok(chunk))
            }
            Ok(Ok(None)) => {
                self.disarm();
                None
            }
            Ok(Err(error)) => {
                self.disarm();
                Some(Err(Failure::from_client(error)))
            }
            Err(_elapsed) => {
                self.abandon();
                // A server that runs the statement ends it at the class limit, before the deadline, with its
                // own exception; one that sent nothing by then is not answering at all.
                Some(Err(if self.answered {
                    Failure::Timeout
                } else {
                    Failure::NoAnswer
                }))
            }
        }
    }

    /// The statement has ended on the server.
    fn disarm(&mut self) {
        if let Some(kill) = &mut self.kill {
            kill.armed = false;
        }
    }

    /// Stops reading a statement that may still run.
    pub(crate) fn abandon(&mut self) {
        self.cursor = None;
        if let Some(kill) = &mut self.kill {
            kill.fire();
        }
    }
}

impl ClickHouseStore {
    /// A store over `env`. No connection is opened until the first call.
    pub fn new(env: Env, config: StoreConfig) -> Self {
        let idle = config
            .idle_timeout
            .unwrap_or(connect::FALLBACK_IDLE_TIMEOUT);
        let clients = Mutex::new(Arc::new(Clients::new(&env, &config, idle)));
        Self {
            inner: Arc::new(Inner {
                env,
                flavour: Mutex::new(config.flavour),
                config,
                clients,
                agent_slots: Arc::new(Semaphore::new(AGENT_MAX_CONCURRENT_QUERIES as usize)),
            }),
        }
    }

    fn clients(&self) -> Arc<Clients> {
        Arc::clone(
            &self
                .inner
                .clients
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// How long the connection pool keeps an idle connection: the operator's `clickhouse.idle_timeout`, or,
    /// once [`TelemetryStore::detect`] has read it, a margin below the server's `keep_alive_timeout`.
    pub fn idle_timeout(&self) -> Duration {
        self.clients().idle
    }

    /// A query as `identity` with an explicit query id, its bound values and its settings.
    fn query(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
        settings: &[(&str, String)],
    ) -> (Uuid, clickhouse::query::Query) {
        let query_id = self.inner.env.new_id();
        let mut query = self.clients().by_identity[slot(identity)]
            .query_raw(&statement.sql)
            .with_setting("query_id", query_id.to_string());
        for (n, param) in statement.params.iter().enumerate() {
            query = query.with_setting(format!("param_p{n}"), param.value.as_str());
        }
        for (name, value) in settings {
            query = query.with_setting(*name, value.as_str());
        }
        for (name, value) in &statement.settings {
            query = query.with_setting(*name, value.as_str());
        }
        (query_id, query)
    }

    /// Runs a statement that returns nothing.
    pub(crate) async fn command(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
        settings: &[(&str, String)],
        deadline: Duration,
    ) -> Result<(), Failure> {
        let (_, query) = self.query(identity, statement, settings);
        match self.inner.env.timeout(deadline, query.execute()).await {
            Ok(result) => result.map_err(Failure::from_client),
            Err(_elapsed) => Err(Failure::Timeout),
        }
    }

    /// Runs a short read and returns its tab-separated answer.
    pub(crate) async fn text(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
        secs: u64,
    ) -> Result<String, Failure> {
        let deadline = Duration::from_secs(secs + DEADLINE_SLACK_SECS);
        let settings = [("max_execution_time", secs.to_string())];
        let (_, query) = self.query(identity, statement, &settings);
        let read = |query: clickhouse::query::Query| async move {
            let mut cursor = query.fetch_bytes("TabSeparated")?;
            cursor.collect().await
        };
        let attempts = async {
            match read(query.clone()).await.map_err(Failure::from_client) {
                // No byte of the request was sent, so the read changed nothing: it is sent once more, on a new
                // connection.
                Err(Failure::NotSent(_)) => read(query).await.map_err(Failure::from_client),
                other => other,
            }
        };
        match self.inner.env.timeout(deadline, attempts).await {
            Ok(Ok(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Ok(Err(failure)) => Err(failure),
            Err(_elapsed) => Err(Failure::Timeout),
        }
    }

    async fn lookup(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
    ) -> Result<String, StoreError> {
        self.text(identity, statement, LOOKUP_SECS)
            .await
            .map_err(Failure::into_store)
    }

    /// Sends one `INSERT ... FORMAT Native` whose body is the pre-compressed block, verbatim.
    pub(crate) async fn insert(
        &self,
        sql: String,
        body: Bytes,
        settings: &[(&str, String)],
    ) -> Result<(), Failure> {
        let query_id = self.inner.env.new_id();
        let mut insert = self.clients().by_identity[slot(ServiceIdentity::Ingest)]
            .insert_formatted_with(sql)
            .with_setting("query_id", query_id.to_string())
            // The body is already in the server's framed compression, so the client compresses nothing.
            .with_setting("decompress", "1");
        for (name, value) in settings {
            insert = insert.with_setting(*name, value.as_str());
        }
        let send = async {
            insert.send(body).await?;
            insert.end().await
        };
        match self.inner.env.timeout(INSERT_DEADLINE, send).await {
            Ok(result) => result.map_err(Failure::from_client),
            Err(_elapsed) => Err(Failure::Timeout),
        }
    }

    /// Starts a streaming read. With `kill_on_drop`, abandoning it issues `KILL QUERY`.
    pub(crate) fn read(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
        settings: &[(&str, String)],
        format: &str,
        deadline: Duration,
        kill_on_drop: bool,
    ) -> Result<(Uuid, Reading), QueryError> {
        let (query_id, query) = self.query(identity, statement, settings);
        let mut reading = self
            .reading(query, query_id, format, deadline, kill_on_drop)
            .map_err(Failure::into_query)?;
        if identity == ServiceIdentity::ReadAgent {
            reading.slots = Some(Arc::clone(&self.inner.agent_slots));
            reading.refused_resend.clone_from(&reading.resend);
        }
        Ok((query_id, reading))
    }

    /// The streaming read of a prepared query that carries `query_id`.
    pub(crate) fn reading(
        &self,
        query: clickhouse::query::Query,
        query_id: Uuid,
        format: &str,
        deadline: Duration,
        kill_on_drop: bool,
    ) -> Result<Reading, Failure> {
        let resend = (query.clone(), format.to_owned());
        let cursor = query.fetch_bytes(format).map_err(Failure::from_client)?;
        Ok(Reading {
            cursor: Some(cursor),
            resend: Some(resend),
            answered: false,
            deadline: self.inner.env.clock.now().saturating_add(deadline),
            kill: kill_on_drop.then(|| KillOnDrop {
                inner: Arc::clone(&self.inner),
                query_id,
                armed: false,
            }),
            slots: None,
            slot: None,
            refused_resend: None,
            inner: Arc::clone(&self.inner),
        })
    }

    /// The read endpoint's client without credentials, for the SQL workspace's per-project identities.
    pub(crate) fn workspace_client(&self) -> clickhouse::Client {
        self.clients().workspace.clone()
    }

    /// The environment the store runs over.
    pub(crate) fn env(&self) -> &Env {
        &self.inner.env
    }

    /// The flavour: the configured one, or the detected one once [`TelemetryStore::detect`] has run.
    async fn flavour(&self) -> Result<StoreFlavour, StoreError> {
        let cached = *self
            .inner
            .flavour
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match cached {
            Some(flavour) => Ok(flavour),
            None => Ok(self.detect_catalogue().await?.flavour),
        }
    }

    /// The value of one row of `system.settings` or `system.server_settings`; empty when it is not listed.
    async fn setting_value(&self, system_table: &str, name: &str) -> Result<String, StoreError> {
        self.setting_value_as(ServiceIdentity::Maint, system_table, name)
            .await
    }

    /// [`ClickHouseStore::setting_value`] as `identity`.
    async fn setting_value_as(
        &self,
        identity: ServiceIdentity,
        system_table: &str,
        name: &str,
    ) -> Result<String, StoreError> {
        let mut b = Binder::default();
        let name = b.string(name);
        let statement = b.finish(format!(
            "SELECT value FROM system.{system_table} WHERE name = {name}"
        ));
        self.lookup(identity, &statement).await
    }

    /// The cluster whose replicas the calls that must hold on every replica read, when the store is clustered.
    async fn replicated(&self) -> Result<Option<&Ident>, StoreError> {
        match self.flavour().await? {
            StoreFlavour::Clustered => {
                self.inner.config.cluster.as_ref().map(Some).ok_or_else(|| {
                    StoreError::Internal(
                        "a replicated store needs the name of its cluster (clickhouse.cluster)"
                            .to_owned(),
                    )
                })
            }
            StoreFlavour::SingleNode | StoreFlavour::Cloud => Ok(None),
        }
    }

    /// A one-row answer read as `identity`, sent to every replica in the cluster.
    async fn every_replica_lookup(
        &self,
        identity: ServiceIdentity,
        statement: Statement,
    ) -> Result<String, StoreError> {
        let mut statement = statement;
        statement
            .settings
            .push(("skip_unavailable_shards", "1".to_owned()));
        self.lookup(identity, &statement).await
    }

    /// The replicas of `cluster` as `host:port`, in the order of `remote_servers`, with their shard numbers.
    async fn cluster_replicas(&self, cluster: &Ident) -> Result<Vec<(u64, String)>, StoreError> {
        let mut b = Binder::default();
        let name = b.string(cluster.as_str());
        let answer = self
            .lookup(
                ServiceIdentity::Admin,
                &b.finish(format!(
                    "SELECT shard_num, host_name, port FROM system.clusters WHERE cluster = {name} ORDER BY \
                     shard_num, replica_num"
                )),
            )
            .await?;
        lines(&answer)
            .into_iter()
            .filter(|line| line != &[""])
            .map(|line| match line.as_slice() {
                [shard, host, port] => shard
                    .parse()
                    .map(|shard| (shard, format!("{host}:{port}")))
                    .map_err(|_| unexpected("system.clusters", &answer)),
                _ => Err(unexpected("system.clusters", &answer)),
            })
            .collect()
    }

    /// The replicas of `cluster` that do not answer a statement sent to every replica, as `host:port`.
    async fn silent_replicas(
        &self,
        cluster: &Ident,
        replicas: &[(u64, String)],
    ) -> Result<Vec<String>, StoreError> {
        let statement = Binder::default().finish(format!(
            "SELECT _shard_num FROM {}",
            on_every_replica(cluster, "SELECT 1")
        ));
        let answer = self
            .every_replica_lookup(ServiceIdentity::Admin, statement)
            .await?;
        let answered: Vec<&str> = answer.lines().collect();
        Ok(replicas
            .iter()
            .enumerate()
            .filter(|(n, _)| !answered.contains(&(n + 1).to_string().as_str()))
            .map(|(_, (_, host))| host.clone())
            .collect())
    }

    /// What the startup checks of a replicated deployment need from the server (contract 5.8), read as
    /// `crucible_admin` through the replica the store talks to: whether it is ClickHouse Cloud, the cluster's
    /// shards and replicas, the macros of the replication path, the Keeper session, and whether every replica
    /// answers a statement sent to all of them.
    pub async fn cluster_facts(&self, cluster: &Ident) -> Result<ClusterFacts, StoreError> {
        let admin = ServiceIdentity::Admin;
        let cloud = self
            .setting_value_as(admin, "settings", "cloud_mode")
            .await?
            .trim()
            == "1";
        if cloud {
            // A Cloud service is refused whatever else it says.
            return Ok(ClusterFacts {
                cloud,
                shards: 0,
                replicas: Vec::new(),
                missing_macros: Vec::new(),
                keeper: None,
                ddl_initial_user_and_roles: false,
                unreplicated: Vec::new(),
                fan_out: None,
                silent: Vec::new(),
            });
        }
        let ddl_initial_user_and_roles = self
            .setting_value_as(
                admin,
                "server_settings",
                "distributed_ddl_use_initial_user_and_roles",
            )
            .await?
            .trim()
            == "1";
        let mut b = Binder::default();
        let database = b.string(DATABASE);
        let replicated = b.string("Replicated");
        let unreplicated = self
            .lookup(
                admin,
                &b.finish(format!(
                    "SELECT name FROM system.tables WHERE database = {database} AND NOT startsWith(engine, \
                     {replicated}) ORDER BY name"
                )),
            )
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        let replicas = self.cluster_replicas(cluster).await?;
        let mut shards: Vec<u64> = replicas.iter().map(|(shard, _)| *shard).collect();
        shards.dedup();
        let mut b = Binder::default();
        let wanted = b.strings(&["shard", "replica"]);
        let defined = self
            .lookup(
                admin,
                &b.finish(format!(
                    "SELECT macro FROM system.macros WHERE has({wanted}, macro)"
                )),
            )
            .await?;
        let defined: Vec<&str> = defined.lines().collect();
        let missing_macros = ["shard", "replica"]
            .into_iter()
            .filter(|name| !defined.contains(name))
            .map(str::to_owned)
            .collect();
        let keeper = self
            .text(
                admin,
                &Binder::default()
                    .finish("SELECT count() FROM system.zookeeper WHERE path = '/'".to_owned()),
                LOOKUP_SECS,
            )
            .await
            .err()
            .map(|failure| failure.into_store().to_string());
        let (fan_out, silent) = if shards.len() == 1 {
            match self.silent_replicas(cluster, &replicas).await {
                Ok(silent) => (None, silent),
                Err(error) => (Some(error.to_string()), Vec::new()),
            }
        } else {
            (None, Vec::new())
        };
        Ok(ClusterFacts {
            cloud,
            shards: shards.len() as u64,
            replicas: replicas.into_iter().map(|(_, host)| host).collect(),
            missing_macros,
            keeper,
            ddl_initial_user_and_roles,
            unreplicated,
            fan_out,
            silent,
        })
    }

    /// The names of `names` that `system.<system_table>` does not list.
    async fn missing(&self, system_table: &str, names: &[&str]) -> Result<Vec<String>, StoreError> {
        let mut b = Binder::default();
        let list = b.strings(names);
        let statement = b.finish(format!(
            "SELECT name FROM system.{system_table} WHERE has({list}, name)"
        ));
        let found = self.lookup(ServiceIdentity::Maint, &statement).await?;
        let found: Vec<&str> = found.lines().collect();
        Ok(names
            .iter()
            .filter(|name| !found.contains(name))
            .map(|name| (*name).to_owned())
            .collect())
    }

    /// The part of detection that reads the server's catalogue as `crucible_maint`.
    async fn detect_catalogue(&self) -> Result<StoreFacts, StoreError> {
        let maint = ServiceIdentity::Maint;
        let plain = |sql: &str| Binder::default().finish(sql.to_owned());

        let answer = self.lookup(maint, &plain("SELECT version()")).await?;
        let version = version(&answer).ok_or_else(|| unexpected("version()", &answer))?;

        let mut b = Binder::default();
        let text_index = b.string("TYPE text(");
        let database = b.string(DATABASE);
        let answer = self
            .lookup(
                maint,
                &b.finish(format!(
                    "SELECT name, engine, position(create_table_query, {text_index}) > 0 FROM system.tables WHERE \
                     database = {database}"
                )),
            )
            .await?;
        let mut engines = Vec::new();
        let mut text_indexes = Vec::new();
        for line in lines(&answer) {
            let [name, engine, has_text_index] = line.as_slice() else {
                return Err(unexpected("system.tables", &answer));
            };
            if let Some(id) = TableId::from_name(name) {
                engines.push((id, engine_family(engine)));
                let wants_text_index = table(id)
                    .indexes
                    .iter()
                    .any(|i| matches!(i.kind, IndexKind::Text { .. }));
                if wants_text_index {
                    text_indexes.push(*has_text_index == "1");
                }
            }
        }
        engines.sort_unstable_by_key(|(id, _)| id.id());

        let mut session: Vec<&str> = InsertSettings::NAMES.to_vec();
        session.extend(QUERY_SETTINGS);
        let mut missing_settings = self.missing("settings", &session).await?;
        missing_settings.extend(self.missing("merge_tree_settings", &TABLE_SETTINGS).await?);
        missing_settings.sort_unstable();
        // `max_execution_time` is both a per-insert setting and a query setting.
        missing_settings.dedup();

        let text_search_functions = self
            .missing("functions", &TEXT_SEARCH_FUNCTIONS)
            .await?
            .is_empty();
        // Once the tables exist their text indexes are the proof. Before the first migration the functions,
        // which only serve that index type, stand in for it.
        let text_index = if text_indexes.is_empty() {
            text_search_functions
        } else {
            text_indexes.iter().all(|present| *present)
        };

        let flavour = match self.inner.config.flavour {
            Some(flavour) => flavour,
            None => {
                let cloud = self.setting_value("settings", "cloud_mode").await?;
                if cloud.trim() == "1" || engines.iter().any(|(_, f)| *f == EngineFamily::Shared) {
                    StoreFlavour::Cloud
                } else if self.inner.config.cluster.is_some()
                    || engines.iter().any(|(_, f)| *f == EngineFamily::Replicated)
                {
                    StoreFlavour::Clustered
                } else {
                    StoreFlavour::SingleNode
                }
            }
        };
        *self
            .inner
            .flavour
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(flavour);

        let ddl_initial_user_and_roles = if flavour == StoreFlavour::Clustered {
            let value = self
                .setting_value(
                    "server_settings",
                    "distributed_ddl_use_initial_user_and_roles",
                )
                .await?;
            Some(value.trim() == "1")
        } else {
            None
        };

        Ok(StoreFacts {
            version,
            flavour,
            engines,
            missing_settings,
            text_search_functions,
            text_index,
            ddl_initial_user_and_roles,
        })
    }

    /// Keeps idle connections for as long as the server does, less a margin: reads the server's
    /// `keep_alive_timeout` as `crucible_maint` and, when the pool's idle timeout differs, rebuilds the clients,
    /// because a pool keeps the timeout it was built with. An operator's `clickhouse.idle_timeout` is never
    /// replaced, and a value that cannot be read keeps the fallback.
    async fn follow_keep_alive(&self) {
        if self.inner.config.idle_timeout.is_some() {
            return;
        }
        let keep_alive = match self
            .setting_value("server_settings", "keep_alive_timeout")
            .await
        {
            Ok(answer) => {
                keep_alive(&answer).ok_or_else(|| format!("the server answered {answer:?}"))
            }
            Err(error) => Err(error.to_string()),
        };
        let idle = match keep_alive {
            Ok(keep_alive) => connect::idle_timeout(keep_alive),
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "the server's keep_alive_timeout could not be read, so idle connections to ClickHouse are \
                     dropped after {:?}; clickhouse.idle_timeout sets how long they are kept",
                    connect::FALLBACK_IDLE_TIMEOUT
                );
                connect::FALLBACK_IDLE_TIMEOUT
            }
        };
        let mut clients = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if clients.idle != idle {
            *clients = Arc::new(Clients::new(&self.inner.env, &self.inner.config, idle));
            tracing::info!(
                ?idle,
                "idle connections to ClickHouse are kept a margin below the server's keep_alive_timeout"
            );
        }
    }

    /// Inserts one row into `dedup_selftest` as `crucible_ingest`, `times` times with one token, and returns
    /// its id.
    async fn insert_selftest_row(&self, times: usize) -> Result<Uuid, StoreError> {
        let env = &self.inner.env;
        let id = env.new_id();
        let epoch = WalEpoch::from_uuid(env.new_id());
        let token = BatchToken::new(
            epoch,
            Lsn::FIRST,
            Lsn::FIRST,
            TableId::DedupSelftest,
            PartitionId::UNPARTITIONED,
        )
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        let block = selftest_block(id, epoch, env.clock.wall_now())?;
        let settings = InsertSettings::new(token, self.flavour().await?);
        for _ in 0..times {
            self.insert_block(block.clone(), settings)
                .await
                .map_err(|error| match error {
                    InsertError::Retry { message, .. }
                    | InsertError::UnknownOutcome { message, .. } => {
                        StoreError::Unavailable(message)
                    }
                    InsertError::FatalConfig { code, message }
                    | InsertError::Poison { code, message } => {
                        StoreError::Rejected { code, message }
                    }
                })?;
        }
        Ok(id)
    }

    /// How many `dedup_selftest` rows with this id `identity` sees.
    async fn selftest_rows(&self, identity: ServiceIdentity, id: Uuid) -> Result<u64, StoreError> {
        let mut b = Binder::default();
        let id = b.uuid(id);
        let name = TableId::DedupSelftest.name();
        let statement = b.finish(format!(
            "SELECT count() FROM {DATABASE}.{name} WHERE id = {id}"
        ));
        let answer = self
            .text(identity, &statement, SHORT_SECS)
            .await
            .map_err(Failure::into_store)?;
        count("the self-test row count", &answer)
    }

    /// Confirms that every service identity sees a known row through its row policy (contracts 5.8 and 7.3).
    async fn check_visibility(&self) -> Result<(), StoreError> {
        let id = self.insert_selftest_row(1).await?;
        for identity in ServiceIdentity::ALL {
            let seen = self.selftest_rows(identity, id).await?;
            if seen != 1 {
                return Err(StoreError::SelfTest(format!(
                    "{} sees {seen} of 1 known row: its service row policy is missing",
                    identity.name()
                )));
            }
        }
        Ok(())
    }

    /// The newest mutation of one partition of a table. The store's mutations all name their partition, and the
    /// mutations of one partition finish in order, so when this one is done every earlier one is, including the
    /// one the caller just started. A mutation of another partition says nothing about it.
    async fn latest_mutation(
        &self,
        id: TableId,
        partition: PartitionId,
    ) -> Result<MutationId, StoreError> {
        let mut b = Binder::default();
        let database = b.string(DATABASE);
        let name = b.string(id.name());
        // The form the server prints for both a lightweight delete and a mask application.
        let in_partition = b.string(&format!("IN PARTITION ID '{partition}'"));
        let digits = b.string("[0-9]+");
        let answer = self
            .lookup(
                ServiceIdentity::Maint,
                &b.finish(format!(
                    "SELECT mutation_id FROM system.mutations WHERE database = {database} AND table = {name}                      AND position(command, {in_partition}) > 0 ORDER BY toUInt64OrZero(extract(mutation_id,                      {digits})) DESC LIMIT 1"
                )),
            )
            .await?;
        Ok(MutationId::new(id, answer.trim().to_owned()))
    }

    async fn change(
        &self,
        statement: &Statement,
        settings: &[(&str, String)],
    ) -> Result<(), StoreError> {
        self.command(ServiceIdentity::Maint, statement, settings, CHANGE_DEADLINE)
            .await
            .map_err(Failure::into_store)
    }

    /// `alter_sync`: on a replicated target a partition change waits for every replica.
    async fn alter_sync(&self) -> Result<String, StoreError> {
        Ok(match self.flavour().await? {
            StoreFlavour::SingleNode => "1",
            StoreFlavour::Clustered | StoreFlavour::Cloud => "2",
        }
        .to_owned())
    }
}

/// A streaming read being decoded.
struct Decoding {
    reading: Reading,
    decoder: Decoder,
    schema: OutputSchema,
    ready: VecDeque<RowBlock>,
    done: bool,
}

impl Decoding {
    /// The blocks of one chunk, each checked against the plan's output schema.
    fn decode(&mut self, chunk: &[u8]) -> Result<(), String> {
        let blocks = self.decoder.push(chunk).map_err(|e| e.to_string())?;
        for block in &blocks {
            self.schema.check_block(block).map_err(|e| e.to_string())?;
        }
        self.ready.extend(blocks);
        Ok(())
    }
}

/// Decodes the chunks of a read into validated row blocks. The first error is terminal.
fn rows(
    reading: Reading,
    schema: OutputSchema,
) -> impl Stream<Item = Result<RowBlock, QueryError>> + Send + 'static {
    let state = Decoding {
        reading,
        decoder: Decoder::new(&schema),
        schema,
        ready: VecDeque::new(),
        done: false,
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(block) = s.ready.pop_front() {
                return Some((Ok(block), s));
            }
            if s.done {
                return None;
            }
            match s.reading.next().await {
                Some(Ok(chunk)) => {
                    if let Err(message) = s.decode(&chunk) {
                        // A result that does not follow the plan's schema is a defect: stop reading it.
                        s.done = true;
                        s.ready.clear();
                        s.reading.abandon();
                        return Some((Err(QueryError::Internal(message)), s));
                    }
                }
                Some(Err(error)) => {
                    s.done = true;
                    return Some((Err(error), s));
                }
                None => {
                    s.done = true;
                    if let Err(error) = s.decoder.finish() {
                        return Some((Err(QueryError::Internal(error.to_string())), s));
                    }
                }
            }
        }
    })
}

#[async_trait]
impl TelemetryStore for ClickHouseStore {
    async fn insert_block(
        &self,
        block: EncodedBlock,
        settings: InsertSettings,
    ) -> Result<(), InsertError> {
        if block.rows() == 0 {
            // Empty blocks are never sent: a zero-row block is a header only and creates no part.
            return Err(InsertError::Poison {
                code: 27,
                message: "an empty block is never sent".to_owned(),
            });
        }
        self.insert(
            render::insert(settings.table()),
            block.compressed().clone(),
            &settings.pairs(),
        )
        .await
        .map_err(Failure::into_insert)
    }

    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError> {
        let cluster = if plan.every_replica() {
            self.replicated()
                .await
                .map_err(|error| QueryError::Unavailable(error.to_string()))?
        } else {
            None
        };
        let statement = match cluster {
            Some(cluster) => render::plan_on_replicas(&plan, cluster),
            None => render::plan(&plan),
        }
        .map_err(|error| QueryError::Internal(error.to_string()))?;
        let limits = plan.limits();
        let settings = class_settings(limits);
        let (query_id, reading) = self.read(
            plan.identity(),
            &statement,
            &settings,
            "RowBinaryWithNamesAndTypes",
            Duration::from_secs(u64::from(limits.max_execution_time_secs) + DEADLINE_SLACK_SECS),
            true,
        )?;
        Ok(RowStream::new(query_id, rows(reading, plan.output())))
    }

    async fn probe(&self, probe: BatchProbe) -> Result<bool, StoreError> {
        if let Some(cluster) = self.replicated().await? {
            let answer = self
                .lookup(
                    ServiceIdentity::Ingest,
                    &render::probe_replicas(&probe, cluster),
                )
                .await?;
            let [holding, answered, replicas] = numbers("the probe of every replica", &answer)?;
            return probe_answer(holding, answered, replicas);
        }
        let answer = self
            .lookup(ServiceIdentity::Ingest, &render::probe(&probe))
            .await?;
        Ok(count("the probe", &answer)? > 0)
    }

    async fn delete(&self, plan: DeletePlan) -> Result<MutationId, StoreError> {
        let statement =
            render::delete(&plan).map_err(|error| StoreError::Internal(error.to_string()))?;
        let mut settings: Vec<(&str, String)> = DELETE_SETTINGS
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect();
        let replicated = self.replicated().await?.is_some();
        if replicated {
            settings.push((INACTIVE_REPLICAS.0, INACTIVE_REPLICAS.1.to_owned()));
        }
        match self.change(&statement, &settings).await {
            Ok(()) => {}
            // A replica that is down runs the delete when it returns; the mutation's status says when every replica
            // has.
            Err(StoreError::Rejected {
                code: UNFINISHED, ..
            }) if replicated => {}
            Err(error) => return Err(error),
        }
        self.latest_mutation(plan.table(), plan.partition()).await
    }

    async fn drop_partition(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<(), StoreError> {
        let mut b = Binder::default();
        let partition = b.string(&partition.to_string());
        let statement = b.finish(format!(
            "ALTER TABLE {DATABASE}.{} DROP PARTITION ID {partition}",
            table.name()
        ));
        let mut settings = vec![("alter_sync", self.alter_sync().await?)];
        let replicated = self.replicated().await?.is_some();
        if replicated {
            settings.push((INACTIVE_REPLICAS.0, INACTIVE_REPLICAS.1.to_owned()));
        }
        match self.change(&statement, &settings).await {
            // The drop is whole once every replica has run it: until a replica that is down returns and drops the
            // partition too, the drop is sent again.
            Err(StoreError::Rejected {
                code: UNFINISHED,
                message,
            }) if replicated => Err(StoreError::Unavailable(message)),
            other => other,
        }
    }

    async fn apply_deleted_mask(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<MutationId, StoreError> {
        let mut b = Binder::default();
        let id = b.string(&partition.to_string());
        let statement = b.finish(format!(
            "ALTER TABLE {DATABASE}.{} APPLY DELETED MASK IN PARTITION ID {id}",
            table.name()
        ));
        // The mutation runs in the background; `mutation_status` follows it.
        let settings = [("mutations_sync", "0".to_owned())];
        self.change(&statement, &settings).await?;
        self.latest_mutation(table, partition).await
    }

    async fn mutation_status(&self, id: &MutationId) -> Result<MutationStatus, StoreError> {
        if id.name().is_empty() {
            return Ok(MutationStatus::Done);
        }
        if let Some(cluster) = self.replicated().await? {
            let mut b = Binder::default();
            let database = b.string(DATABASE);
            let name = b.string(id.table().name());
            let mutation = b.string(id.name());
            let cluster_name = b.string(cluster.as_str());
            let present = table_present(&database, &name);
            let rows = on_every_replica(
                cluster,
                &format!(
                    "SELECT count() AS listed, countIf(is_done) AS done, countIf(is_killed) AS killed, \
                     max(latest_fail_reason) AS reason, {present} AS present FROM system.mutations WHERE database \
                     = {database} AND table = {name} AND mutation_id = {mutation}"
                ),
            );
            let answer = self
                .every_replica_lookup(
                    ServiceIdentity::Maint,
                    b.finish(format!(
                        "SELECT listed, done, killed, reason, (SELECT count() FROM system.clusters WHERE cluster = \
                         {cluster_name}) FROM {rows} WHERE present > 0"
                    )),
                )
                .await?;
            let mut replicas = 0;
            let mut rows = Vec::new();
            for line in lines(&answer) {
                let [listed, done, killed, reason, total] = line.as_slice() else {
                    return Err(unexpected("system.mutations of every replica", &answer));
                };
                replicas = total
                    .parse()
                    .map_err(|_| unexpected("system.clusters", &answer))?;
                rows.push(ReplicaMutation {
                    listed: *listed != "0",
                    done: *done != "0",
                    killed: *killed != "0",
                    reason,
                });
            }
            // A cluster none of whose replicas answered counts its replicas in no row.
            if rows.is_empty() {
                return Ok(MutationStatus::Pending);
            }
            return Ok(mutation_answer(replicas, &rows));
        }
        let mut b = Binder::default();
        let database = b.string(DATABASE);
        let name = b.string(id.table().name());
        let mutation = b.string(id.name());
        let answer = self
            .lookup(
                ServiceIdentity::Maint,
                &b.finish(format!(
                    "SELECT is_done, is_killed, latest_fail_reason FROM system.mutations WHERE database = \
                     {database} AND table = {name} AND mutation_id = {mutation}"
                )),
            )
            .await?;
        // A mutation the server no longer lists has finished, or its partition was dropped.
        let Some(line) = lines(&answer).into_iter().next() else {
            return Ok(MutationStatus::Done);
        };
        Ok(match line.as_slice() {
            ["1", ..] => MutationStatus::Done,
            ["0", "0", ""] => MutationStatus::Pending,
            ["0", _, reason] => MutationStatus::Failed {
                code: parse_code(reason).unwrap_or(0),
                reason: if reason.is_empty() {
                    "the mutation was killed".to_owned()
                } else {
                    (*reason).to_owned()
                },
            },
            _ => return Err(unexpected("system.mutations", &answer)),
        })
    }

    async fn partition_projects(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        if !crucible_schema::table(table).is_scoped() {
            return Err(StoreError::Internal(format!(
                "{} has no project column",
                table.name()
            )));
        }
        let mut b = Binder::default();
        let partition = b.string(&partition.to_string());
        let nil = b.uuid(Uuid::NIL);
        let answer = self
            .lookup(
                ServiceIdentity::Maint,
                &b.finish(format!(
                    "SELECT DISTINCT project_id FROM {DATABASE}.{} WHERE _partition_id = {partition} AND \
                     project_id != {nil}",
                    table.name()
                )),
            )
            .await?;
        let mut projects = answer
            .lines()
            .map(|line| ProjectId::parse(line).map_err(|_| unexpected("project ids", &answer)))
            .collect::<Result<Vec<_>, _>>()?;
        projects.sort_unstable();
        Ok(projects)
    }

    async fn masked_parts(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<u64, StoreError> {
        let mut b = Binder::default();
        let database = b.string(DATABASE);
        let name = b.string(table.name());
        let partition = b.string(&partition.to_string());
        if let Some(cluster) = self.replicated().await? {
            let cluster_name = b.string(cluster.as_str());
            let present = table_present(&database, &name);
            let parts = on_every_replica(
                cluster,
                &format!(
                    "SELECT count() AS n, {present} AS present FROM system.parts WHERE database = {database} AND \
                     table = {name} AND partition_id = {partition} AND active AND has_lightweight_delete"
                ),
            );
            let answer = self
                .every_replica_lookup(
                    ServiceIdentity::Maint,
                    b.finish(format!(
                        "SELECT sum(n), countIf(present > 0), (SELECT count() FROM system.clusters WHERE cluster = \
                         {cluster_name}) FROM {parts}"
                    )),
                )
                .await?;
            let [masked, answered, replicas] =
                numbers("the masked parts of every replica", &answer)?;
            if answered < replicas {
                return Err(StoreError::Unavailable(format!(
                    "{answered} of the cluster's {replicas} replicas answered with the table, and one that did not \
                     may still hold masked parts"
                )));
            }
            return Ok(masked);
        }
        let answer = self
            .lookup(
                ServiceIdentity::Maint,
                &b.finish(format!(
                    "SELECT count() FROM system.parts WHERE database = {database} AND table = {name} AND \
                     partition_id = {partition} AND active AND has_lightweight_delete"
                )),
            )
            .await?;
        count("the masked part count", &answer)
    }

    async fn apply_migration(&self, step: &MigrationStep) -> Result<(), StoreError> {
        // Every replica runs a step's ON CLUSTER statements, so none may be down when it starts.
        if let Some(cluster) = &self.inner.config.cluster {
            let replicas = self.cluster_replicas(cluster).await?;
            let silent = self.silent_replicas(cluster, &replicas).await?;
            if !silent.is_empty() {
                return Err(StoreError::Unavailable(format!(
                    "{} of cluster {cluster} did not answer, and every replica runs the migration's ON CLUSTER \
                     statements: start it, then migrate again",
                    silent.join(", ")
                )));
            }
        }
        let settings: Vec<(&str, String)> = step
            .settings
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect();
        let deadline = migration_deadline(step);
        for sql in &step.statements {
            let statement = Binder::default().finish(sql.clone());
            self.command(ServiceIdentity::Admin, &statement, &settings, deadline)
                .await
                .map_err(Failure::into_store)?;
        }
        Ok(())
    }

    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        let facts = self.detect_catalogue().await?;
        self.follow_keep_alive().await;
        // The check needs the table and the identities, so it starts with the first migration.
        let migrated = facts
            .engines
            .iter()
            .any(|(id, _)| *id == TableId::DedupSelftest);
        if migrated {
            self.check_visibility().await?;
        }
        Ok(facts)
    }

    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        let id = self.insert_selftest_row(2).await?;
        match self.selftest_rows(ServiceIdentity::Ingest, id).await? {
            1 => Ok(()),
            rows => Err(StoreError::SelfTest(format!(
                "one block inserted twice with one token left {rows} rows, not 1"
            ))),
        }
    }

    async fn kill(&self, query_id: Uuid) -> Result<(), StoreError> {
        let mut b = Binder::default();
        let id = b.string(&query_id.to_string());
        let statement = b.finish(format!("KILL QUERY WHERE query_id = {id} ASYNC"));
        self.command(ServiceIdentity::Maint, &statement, &[], KILL_DEADLINE)
            .await
            .map_err(Failure::into_store)
    }

    async fn health(&self) -> StoreHealth {
        let statement = Binder::default().finish("SELECT 1".to_owned());
        match self
            .text(ServiceIdentity::Maint, &statement, SHORT_SECS)
            .await
        {
            Ok(_) => StoreHealth::Healthy,
            Err(failure) => StoreHealth::Unavailable {
                reason: failure.into_store().to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_plan::QueryClass;
    use crucible_schema::{Ident, Variant};

    use super::*;

    #[test]
    fn detection_checks_every_setting_the_store_sends() {
        let mut sent: Vec<&str> = Vec::new();
        for class in QueryClass::ALL {
            sent.extend(class_settings(class.limits()).iter().map(|(name, _)| *name));
        }
        sent.extend(DELETE_SETTINGS.map(|(name, _)| name));
        // The reads of every replica in a cluster and its replicated changes.
        sent.extend(["skip_unavailable_shards", INACTIVE_REPLICAS.0]);
        // The byte limit a plan may carry, the semi-join id cap, the per-series fill, the mask application and
        // the partition drop.
        sent.extend([
            "max_bytes_to_read",
            "max_rows_in_set",
            "set_overflow_mode",
            "use_with_fill_by_sorting_prefix",
            "mutations_sync",
            "alter_sync",
        ]);
        let cluster = Variant::Clustered {
            cluster: Ident::new("crucible").unwrap(),
        };
        for variant in [Variant::SingleNode, cluster, Variant::Cloud] {
            sent.extend(variant.ddl_settings().iter().map(|(name, _)| *name));
            let admin = Ident::new("crucible_admin").unwrap();
            let hourly = crucible_schema::schema_mcp_tool_rollup_hourly(&variant, &admin);
            sent.extend(hourly.settings.iter().map(|(name, _)| *name));
        }
        for name in sent {
            // `wait_end_of_query` is a parameter of the HTTP interface, not a setting.
            assert!(
                QUERY_SETTINGS.contains(&name)
                    || InsertSettings::NAMES.contains(&name)
                    || name == "wait_end_of_query",
                "{name} is sent and not checked by detect"
            );
        }
    }

    /// The client waits for a migration statement as long as the step lets the server run it, and never less
    /// than the default.
    #[test]
    fn a_migration_waits_for_the_limit_its_step_sets() {
        let admin = Ident::new("crucible_admin").unwrap();
        let hourly = crucible_schema::schema_mcp_tool_rollup_hourly(&Variant::SingleNode, &admin);
        assert_eq!(
            migration_deadline(&hourly),
            Duration::from_secs(3_600 + DEADLINE_SLACK_SECS)
        );
        let base =
            crucible_schema::schema_base(&Variant::SingleNode, crucible_schema::Granularity::Day);
        assert_eq!(migration_deadline(&base), CHANGE_DEADLINE);
    }

    /// Every read keeps column statistics out of its planning: under ingest, ClickHouse 26.8 loads the statistics
    /// of every part of the table for each read that plans with them (`docs/testing/perf.md`).
    #[test]
    fn every_read_leaves_column_statistics_unused() {
        for class in QueryClass::ALL {
            assert!(
                class_settings(class.limits()).contains(&("use_statistics", "0".to_owned())),
                "{class:?} reads use column statistics"
            );
        }
    }

    #[test]
    fn versions_and_engines_are_read_from_the_server_answers() {
        assert_eq!(
            version(
                "26.3.66.4
"
            ),
            Some(StoreVersion {
                major: 26,
                minor: 3
            })
        );
        assert_eq!(version("26"), None);
        assert_eq!(engine_family("SharedMergeTree"), EngineFamily::Shared);
        assert_eq!(
            engine_family("ReplicatedAggregatingMergeTree"),
            EngineFamily::Replicated
        );
        assert_eq!(
            engine_family("AggregatingMergeTree"),
            EngineFamily::MergeTree
        );
    }

    #[test]
    fn the_keep_alive_timeout_is_read_in_whole_seconds() {
        assert_eq!(keep_alive("10\n"), Some(Duration::from_secs(10)));
        assert_eq!(keep_alive("30"), Some(Duration::from_secs(30)));
        // The setting is not listed, or the answer is not a number of seconds.
        for unusable in ["", "\n", "ten", "-1", "1.5"] {
            assert_eq!(keep_alive(unusable), None, "{unusable:?}");
        }
    }

    #[test]
    fn the_selftest_block_follows_the_table() {
        let block = selftest_block(
            Uuid::from_bytes([1; 16]),
            WalEpoch::from_bytes([2; 16]),
            Timestamp::from_micros(7),
        )
        .unwrap();
        assert_eq!(block.rows(), 1);
    }

    proptest::proptest! {
        /// The probe of a cluster reads present only when a majority holds the block, as an acknowledged insert
        /// with `insert_quorum = auto` leaves it, and absent only when every replica answered without it; every
        /// other answer is no answer, so the shipper probes again rather than resend a block that may land.
        #[test]
        fn the_probe_of_a_cluster_answers_only_what_every_replica_settles(
            replicas in 1u64..8,
            answered in 0u64..8,
            holding in 0u64..8,
        ) {
            let answered = answered.min(replicas);
            let holding = holding.min(answered);
            match probe_answer(holding, answered, replicas) {
                Ok(true) => proptest::prop_assert!(2 * holding > replicas),
                Ok(false) => proptest::prop_assert!(holding == 0 && answered == replicas),
                Err(StoreError::Unavailable(_)) => proptest::prop_assert!(
                    2 * holding <= replicas && (holding > 0 || answered < replicas)
                ),
                Err(other) => proptest::prop_assert!(false, "{other:?}"),
            }
        }
    }

    #[test]
    fn the_probe_of_a_cluster_with_no_replica_is_no_answer() {
        assert!(matches!(
            probe_answer(0, 0, 0),
            Err(StoreError::Unavailable(_))
        ));
        // Two replicas: one holds the block while its quorum waits.
        assert!(probe_answer(1, 2, 2).is_err());
        assert!(probe_answer(2, 2, 2).unwrap());
        assert!(!probe_answer(0, 2, 2).unwrap());
        // Three replicas, one down: a majority still answers present, never absent.
        assert!(probe_answer(2, 2, 3).unwrap());
        assert!(probe_answer(0, 2, 3).is_err());
    }

    /// A mutation of a cluster is done once every replica has done it or none lists it any more; a replica that
    /// did not answer, or that does not list it yet, has not run it.
    #[test]
    fn a_mutation_of_a_cluster_is_done_on_every_replica_or_pending() {
        let row = |listed, done| ReplicaMutation {
            listed,
            done,
            killed: false,
            reason: "",
        };
        assert_eq!(
            mutation_answer(2, &[row(true, true), row(true, true)]),
            MutationStatus::Done
        );
        assert_eq!(
            mutation_answer(2, &[row(false, false), row(false, false)]),
            MutationStatus::Done
        );
        for pending in [
            vec![row(true, true), row(true, false)],
            vec![row(true, true), row(false, false)],
            vec![row(true, true)],
            vec![],
        ] {
            assert_eq!(
                mutation_answer(2, &pending),
                MutationStatus::Pending,
                "{pending:?}"
            );
        }
        let failed = ReplicaMutation {
            reason: "Code: 241. DB::Exception: Memory limit exceeded",
            ..row(true, false)
        };
        assert_eq!(
            mutation_answer(2, &[row(true, true), failed]),
            MutationStatus::Failed {
                code: 241,
                reason: failed.reason.to_owned()
            }
        );
    }

    #[test]
    fn one_row_answers_read_as_numbers() {
        assert_eq!(numbers::<3>("x", "1\t2\t3\n").unwrap(), [1, 2, 3]);
        assert!(numbers::<3>("x", "1\t2\n").is_err());
        assert!(numbers::<2>("x", "1\tz\n").is_err());
    }

    #[test]
    fn credentials_do_not_print_their_password() {
        let credentials = Credentials {
            user: "crucible_ingest".to_owned(),
            password: "hunter2".to_owned(),
        };
        assert!(!format!("{credentials:?}").contains("hunter2"));
    }
}

/// Reads of `crucible_read_agent` that the server refuses because the identity runs as many statements as its
/// profile allows, as it does when the reads of several nodes together fill its cap (contract 7.1), against a fake
/// of the server's HTTP interface on the simulated network.
#[cfg(test)]
mod refused_agent_reads {
    use std::convert::Infallible;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicU32, Ordering};

    use crucible_env::{Clock as _, Listener, Net, NetError, Stream};
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper_util::rt::TokioIo;

    use super::*;

    /// The server's answer to a statement it refuses for its user's statement count.
    const REFUSAL: &str = "Code: 202. DB::Exception: Too many simultaneous queries for user crucible_read_agent. \
                           Maximum: 16. (TOO_MANY_SIMULTANEOUS_QUERIES) (version 26.8.1.1)";

    /// A server that refuses its first `refusals` statements that way and answers `1` to the others.
    #[derive(Debug)]
    struct FakeServer {
        env: Env,
        refusals: u32,
        statements: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Net for FakeServer {
        async fn listen(&self, _: SocketAddr) -> Result<Box<dyn Listener>, NetError> {
            Err(NetError::Io(std::io::Error::other(
                "the fake network does not listen",
            )))
        }

        async fn connect(&self, _: SocketAddr, _: Instant) -> Result<Stream, NetError> {
            let (client, server) = tokio::io::duplex(1 << 16);
            let statements = Arc::clone(&self.statements);
            let refusals = self.refusals;
            self.env.spawn("fake-clickhouse", async move {
                let service = hyper::service::service_fn(move |_: http::Request<Incoming>| {
                    let refused = statements.fetch_add(1, Ordering::Relaxed) < refusals;
                    let answer = if refused {
                        http::Response::builder()
                            .status(500)
                            .header("X-ClickHouse-Exception-Code", "202")
                            .body(Full::new(Bytes::from_static(REFUSAL.as_bytes())))
                    } else {
                        http::Response::builder().body(Full::new(Bytes::from_static(b"1\n")))
                    };
                    async move { Ok::<_, Infallible>(answer.unwrap()) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(server), service)
                    .await;
            });
            Ok(Stream::new(client))
        }

        async fn resolve(&self, _: &str) -> Result<Vec<IpAddr>, NetError> {
            Ok(vec![IpAddr::V4(Ipv4Addr::new(10, 9, 0, 9))])
        }

        async fn tls_connect(
            &self,
            stream: Stream,
            _: &str,
            _: &TlsRoots,
        ) -> Result<Stream, NetError> {
            Ok(stream)
        }
    }

    fn credentials(user: &str) -> Credentials {
        Credentials {
            user: user.to_owned(),
            password: "fake".to_owned(),
        }
    }

    /// Reads `SELECT 1` as `identity` from a server that refuses its first `refusals` statements, within
    /// `deadline`: the answer, the statements the server saw and how long the read took.
    fn read_from(
        refusals: u32,
        identity: ServiceIdentity,
        deadline: Duration,
    ) -> (Result<Bytes, QueryError>, u32, Duration) {
        crucible_sim::run(7, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let statements = Arc::new(AtomicU32::new(0));
            let mut env = host.env.clone();
            env.net = Arc::new(FakeServer {
                env: host.env.clone(),
                refusals,
                statements: Arc::clone(&statements),
            });
            let store = ClickHouseStore::new(
                env,
                StoreConfig {
                    url: "http://clickhouse.test:8123".to_owned(),
                    read_url: None,
                    admin: credentials("crucible_admin"),
                    ingest: credentials("crucible_ingest"),
                    read_ui: credentials("crucible_read_ui"),
                    read_agent: credentials("crucible_read_agent"),
                    maint: credentials("crucible_maint"),
                    flavour: Some(StoreFlavour::SingleNode),
                    cluster: None,
                    tls_roots: TlsRoots::WebPki,
                    connect_timeout: Duration::from_secs(5),
                    idle_timeout: Some(Duration::from_secs(5)),
                },
            );
            let statement = Binder::default().finish("SELECT 1".to_owned());
            let started = host.clock.now();
            let (_, mut reading) = store
                .read(identity, &statement, &[], "TabSeparated", deadline, false)
                .unwrap();
            let answer = reading.next().await.unwrap();
            let took = host.clock.now().saturating_duration_since(started);
            (answer, statements.load(Ordering::Relaxed), took)
        })
    }

    #[test]
    fn a_refused_agent_read_waits_and_is_sent_again_until_the_server_runs_it() {
        let (answer, statements, took) =
            read_from(4, ServiceIdentity::ReadAgent, Duration::from_secs(10));
        assert_eq!(answer.unwrap(), Bytes::from_static(b"1\n"));
        assert_eq!(statements, 5);
        // The waits double from 50 ms: 50, 100, 200 and 400 ms.
        assert!(took >= Duration::from_millis(750), "{took:?}");
    }

    #[test]
    fn a_refusal_that_outlasts_the_deadline_is_a_timeout() {
        let (answer, statements, took) =
            read_from(u32::MAX, ServiceIdentity::ReadAgent, Duration::from_secs(3));
        assert_eq!(answer, Err(QueryError::Timeout));
        assert!(statements > 3, "{statements}");
        assert!(took <= Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn a_refused_read_of_another_identity_is_not_sent_again() {
        let (answer, statements, _) =
            read_from(1, ServiceIdentity::ReadUi, Duration::from_secs(10));
        assert!(
            matches!(answer, Err(QueryError::Unavailable(_))),
            "{answer:?}"
        );
        assert_eq!(statements, 1);
    }
}
