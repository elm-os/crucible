//! The ClickHouse side of `crucible serve`: where the credentials come from, the migrations that `--migrate`
//! applies, and the startup checks of contract 5.8.
//!
//! - The operator supplies `crucible_admin` (contract 7.1). The passwords of the four generated service
//!   identities are derived from the master key (contract 11.2) by HKDF-SHA256 with the identity name and a
//!   generation counter, so nothing is stored. The info string is
//!   `crucible/clickhouse-identity/v1/<name>/<generation>` and the password is the 32 output bytes as lowercase
//!   hex.
//! - `--migrate` and `crucible migrate` apply schema step `0001_schema_base`, access step
//!   `0002_access_service` and schema step `0004_span_usage_aggregate` for a single-node server
//!   ([`apply_migrations`]). All are idempotent. With Postgres configured they are recorded in the ledger of
//!   contract 6.6 and applied once; without it there is no ledger and all are applied each time.
//! - [`bring_up`] is everything that must hold before ingest is accepted.

use std::time::Duration;

use anyhow::Context as _;
use crucible_env::{
    Env, StoreError, StoreFacts, StoreFlavour, StoreVersion, TelemetryStore, TlsRoots,
};
use crucible_plan::profile_caps;
use crucible_schema::{
    AccessParams, Granularity, Ident, MigrationStep, PasswordHash, ServiceIdentity, TABLES,
    Variant, access_service, schema_base, schema_span_usage_aggregate,
};

use crate::control::{ApplyStep, Applying, Control};
use crucible_store_clickhouse::{ClickHouseStore, Credentials, StoreConfig};
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
        let info = format!(
            "crucible/clickhouse-identity/v1/{}/{generation}",
            identity.name()
        );
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

/// What `crucible serve` needs to reach ClickHouse.
#[derive(Clone, Debug)]
pub(crate) struct ClickHouse {
    pub(crate) url: String,
    pub(crate) admin: Credentials,
    pub(crate) master_key: MasterKey,
    /// `clickhouse.idle_timeout`; `None` lets detection derive it from the server.
    pub(crate) idle_timeout: Option<Duration>,
}

impl ClickHouse {
    /// The settings of the `[clickhouse]` section.
    pub(crate) fn new(config: &crucible_config::ClickHouse) -> anyhow::Result<Self> {
        Ok(Self {
            url: config.url.clone(),
            admin: Credentials {
                user: config.user.clone(),
                password: config.password.expose().to_owned(),
            },
            master_key: MasterKey::parse(config.master_key.expose())?,
            idle_timeout: config.idle_timeout,
        })
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

    /// The steps `--migrate` applies, in ledger order, for a single-node server.
    fn steps(&self) -> anyhow::Result<[MigrationStep; 3]> {
        let variant = Variant::SingleNode;
        Ok([
            schema_base(&variant, Granularity::Day),
            access_service(&variant, &self.access_params()?),
            schema_span_usage_aggregate(&variant),
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
}

/// What `--migrate` asks of [`bring_up`]: apply the migrations first, under the ledger of the control plane
/// when Postgres is configured.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Migrate<'a>(pub(crate) Option<&'a Control>);

/// Applies the ClickHouse schema and access steps. With a control plane, the Postgres migrations run first and
/// the ledger of contract 6.6 (`schema_migrations`) decides which steps are still to apply. Without one there
/// is no ledger: every step is applied each time, which their idempotence allows.
pub(crate) async fn apply_migrations(
    env: &Env,
    config: &ClickHouse,
    store: &ClickHouseStore,
    control: Option<&Control>,
) -> anyhow::Result<()> {
    let steps = config.steps()?;
    match control {
        Some(control) => control.migrate(env, &steps, store).await,
        None => {
            for step in &steps {
                store.apply(step).await?;
                tracing::info!(step = step.id, "migration step applied");
            }
            Ok(())
        }
    }
}

/// Why a server cannot take ingest although detection ran (contract 5.8); `None` when it can.
fn refusal(facts: &StoreFacts) -> Option<String> {
    let StoreVersion { major, minor } = facts.version;
    if facts.version < StoreVersion::MINIMUM {
        return Some(format!(
            "ClickHouse {major}.{minor} is older than the minimum, 26.3"
        ));
    }
    if facts.flavour != StoreFlavour::SingleNode {
        return Some(format!(
            "this development preview supports a single-node ClickHouse only, and the server is {:?}",
            facts.flavour
        ));
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

/// Everything that must hold before a node accepts ingest: the migrations when asked for, then startup
/// detection and the two self-tests of contract 5.8. On `Err` ingest stays closed and the text says why.
pub(crate) async fn bring_up(
    env: &Env,
    config: &ClickHouse,
    store: &ClickHouseStore,
    migrate: Option<&Migrate<'_>>,
) -> anyhow::Result<StoreFacts> {
    if let Some(Migrate(control)) = migrate {
        apply_migrations(env, config, store, *control).await?;
    }
    // `detect` includes the check that every service identity sees a known row.
    let facts = store.detect().await.map_err(|error| {
        let hint = match &error {
            // AUTHENTICATION_FAILED for a generated identity.
            StoreError::Rejected { code: 516, .. } => {
                " (the service users are missing, or were created under a different master key;                  see --migrate and --master-key-file)"
            }
            _ => "",
        };
        anyhow::anyhow!("startup detection{hint}: {error}")
    })?;
    if let Some(reason) = refusal(&facts) {
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
            idle_timeout: None,
        };
        let steps = config.steps().unwrap();
        assert_eq!(
            steps.each_ref().map(|step| step.id),
            [
                crucible_schema::SCHEMA_BASE,
                crucible_schema::ACCESS_SERVICE,
                crucible_schema::SCHEMA_SPAN_USAGE_AGGREGATE
            ]
        );
        let sql = steps[1].statements.join("\n");
        // Only digests reach a statement: neither a derived password nor the admin's.
        for identity in ServiceIdentity::GENERATED {
            assert!(!sql.contains(&config.generated(identity).password));
        }
        assert!(!sql.contains("admin-secret"));
        assert!(format!("{config:?}").contains("MasterKey(..)"));
        assert!(!format!("{config:?}").contains("admin-secret"));
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
        assert_eq!(refusal(&fits), None);
        let refused = |change: fn(&mut StoreFacts)| {
            let mut facts = fits.clone();
            change(&mut facts);
            refusal(&facts).unwrap()
        };
        assert!(refused(|f| f.version.minor = 2).contains("older than the minimum"));
        assert!(refused(|f| f.flavour = StoreFlavour::Clustered).contains("single-node"));
        assert!(refused(|f| f.engines.clear()).contains("--migrate"));
        assert!(refused(|f| f.missing_settings.push("x".to_owned())).contains("settings x"));
        assert!(refused(|f| f.text_index = false).contains("text index"));
    }
}
