//! The Postgres side of the binary: the control store from the configuration, and the migrations ledger of
//! contract 6.6.
//!
//! With Postgres configured, `crucible migrate` and `serve --migrate` apply the Postgres migrations and then
//! the ClickHouse schema and access steps that the ledger (`schema_migrations`) does not hold yet. The
//! ClickHouse steps run under the `migrations` lease (contract 11.7), and each is recorded in a transaction
//! fenced by the lease's token, so two nodes that migrate at once never interleave. Without Postgres there is
//! no ledger and every step is applied each time, which their idempotence allows.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crucible_control::{
    ControlStore, LEASE_TTL, Lease, LeaseName, LedgerEntry, MigrationKind, TxError,
};
use crucible_control_pg::{PgConfig, PgControlStore};
use crucible_env::Env;
use crucible_schema::{MigrationStep, StepKind};
use sha2::{Digest, Sha256};

/// How long a migration waits for another node's `migrations` lease before it gives up.
const LEASE_WAIT: Duration = Duration::from_secs(60);
/// The pause between attempts to take the lease, and between attempts to record a step.
const RETRY_EVERY: Duration = Duration::from_secs(1);
/// How often recording a step is tried again after a retryable failure.
const RECORD_ATTEMPTS: u32 = 5;

/// A step being applied.
pub(crate) type Applying<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// What applies a step to ClickHouse.
pub(crate) trait ApplyStep: Sync {
    /// Applies the step. It is idempotent, so a step that failed part-way is applied again.
    fn apply<'a>(&'a self, step: &'a MigrationStep) -> Applying<'a>;
}

/// The control plane of this node.
#[derive(Clone, Debug)]
pub(crate) struct Control {
    config: PgConfig,
    pub(crate) store: PgControlStore,
    /// One Postgres migration at a time on this node: `serve --migrate` starts them for the identity tasks and
    /// again before the ClickHouse steps, and two at once would race on the same tables.
    migrating: Arc<tokio::sync::Mutex<()>>,
}

impl Control {
    /// The store over `env`. No connection is opened yet.
    pub(crate) fn new(env: &Env, config: &crucible_config::Postgres) -> anyhow::Result<Self> {
        let config = PgConfig {
            password: config
                .password
                .as_ref()
                .map(|password| password.expose().to_owned()),
            main_pool: config.pool_size,
            ..PgConfig::new(config.url.expose())
        };
        let store = PgControlStore::new(env, &config).context("the Postgres configuration")?;
        Ok(Self {
            config,
            store,
            migrating: Arc::default(),
        })
    }

    /// The store, as the API's dashboard routes hold it.
    pub(crate) fn dashboards(&self) -> Arc<dyn crucible_control::ControlStore> {
        Arc::new(self.store.clone())
    }

    /// Applies the Postgres migrations, then the steps of `steps` that the ledger does not hold.
    pub(crate) async fn migrate(
        &self,
        env: &Env,
        steps: &[MigrationStep],
        apply: &dyn ApplyStep,
    ) -> anyhow::Result<()> {
        self.migrate_postgres(env).await?;
        let holder = format!("migrate-{}", env.new_id());
        apply_steps(env, &self.store, &holder, steps, apply).await
    }

    /// Applies the Postgres migrations and records them in the ledger.
    pub(crate) async fn migrate_postgres(&self, env: &Env) -> anyhow::Result<()> {
        let _one_at_a_time = self.migrating.lock().await;
        let applied = crucible_control_pg::migrate(env, &self.config)
            .await
            .context("applying the Postgres migrations")?;
        for entry in &applied {
            tracing::info!(step = entry.id, "postgres migration applied");
        }
        Ok(())
    }
}

/// The ledger checksum of a step: SHA-256 over its statements.
fn checksum(step: &MigrationStep) -> String {
    let mut hasher = Sha256::new();
    for statement in &step.statements {
        hasher.update(
            u64::try_from(statement.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(statement.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Takes the `migrations` lease, waiting while another node holds it.
async fn acquire(env: &Env, store: &dyn ControlStore, holder: &str) -> anyhow::Result<Lease> {
    let deadline = env.clock.now().saturating_add(LEASE_WAIT);
    loop {
        let attempt = store
            .leases()
            .acquire(LeaseName::Migrations, holder, LEASE_TTL)
            .await;
        match attempt {
            Ok(Some(lease)) => return Ok(lease),
            Ok(None) if env.clock.now() < deadline => {}
            Ok(None) => anyhow::bail!(
                "another node has held the migrations lease for {} s; it is still migrating. Run the \
                 migration again when it has finished",
                LEASE_WAIT.as_secs()
            ),
            // The lease may have been taken for this holder; it expires by itself.
            Err(error) if error.is_retryable() || error == TxError::CommitUnknown => {
                anyhow::ensure!(
                    env.clock.now() < deadline,
                    "taking the migrations lease: {error}"
                );
            }
            Err(error) => return Err(error).context("taking the migrations lease"),
        }
        env.clock.sleep(RETRY_EVERY).await;
    }
}

/// Records `entry` in a transaction fenced by `lease`.
async fn record(
    env: &Env,
    store: &dyn ControlStore,
    lease: &Lease,
    entry: &LedgerEntry,
) -> anyhow::Result<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = async {
            let tx = store.begin().await?;
            tx.fence(lease).await?;
            tx.migrations().apply(entry).await?;
            tx.commit().await
        }
        .await;
        let error = match result {
            Ok(()) => return Ok(()),
            Err(TxError::Fenced) => anyhow::bail!(
                "the migrations lease was lost while step {} ran, so it is not recorded. Run the migration \
                 again: the step is idempotent",
                entry.id
            ),
            Err(error) => error,
        };
        // An unknown commit is settled by reading the ledger back.
        if error == TxError::CommitUnknown
            && let Ok(applied) = store.migrations().applied().await
            && applied.iter().any(|row| row.id == entry.id)
        {
            return Ok(());
        }
        let retry = error.is_retryable() || error == TxError::CommitUnknown;
        if !retry || attempt >= RECORD_ATTEMPTS {
            return Err(error)
                .with_context(|| format!("recording step {} in the ledger", entry.id));
        }
        env.clock.sleep(RETRY_EVERY).await;
    }
}

/// Applies the steps the ledger does not hold, in order, under the `migrations` lease, and records each.
pub(crate) async fn apply_steps(
    env: &Env,
    store: &dyn ControlStore,
    holder: &str,
    steps: &[MigrationStep],
    apply: &dyn ApplyStep,
) -> anyhow::Result<()> {
    if steps.is_empty() {
        return Ok(());
    }
    let mut lease = acquire(env, store, holder).await?;
    let result = async {
        let ledger: BTreeMap<String, String> = store
            .migrations()
            .applied()
            .await
            .context("reading the migrations ledger")?
            .into_iter()
            .map(|entry| (entry.id, entry.checksum))
            .collect();
        for step in steps {
            let checksum = checksum(step);
            match ledger.get(step.id) {
                Some(recorded) if *recorded == checksum => {
                    tracing::debug!(step = step.id, "migration step already recorded");
                    continue;
                }
                Some(_) => tracing::warn!(
                    step = step.id,
                    "the ledger records this step with other content (for example another master key); \
                     applying it again"
                ),
                None => {}
            }
            lease = store
                .leases()
                .renew(&lease, LEASE_TTL)
                .await
                .context("renewing the migrations lease")?
                .context("the migrations lease was lost; run the migration again")?;
            apply.apply(step).await?;
            let entry = LedgerEntry {
                id: step.id.to_owned(),
                kind: match step.kind {
                    StepKind::Schema => MigrationKind::ClickhouseSchema,
                    StepKind::Access => MigrationKind::ClickhouseAccess,
                },
                applied_at: env.clock.wall_now(),
                checksum,
            };
            record(env, store, &lease, &entry).await?;
            tracing::info!(step = step.id, "migration step applied and recorded");
        }
        Ok(())
    }
    .await;
    // Shutdown rule of contract 11.7: a leader releases its lease. A failure here only delays the next
    // migration by the lease's 15 s.
    if let Err(error) = store.leases().release(&lease).await {
        tracing::debug!(%error, "releasing the migrations lease");
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crucible_control::Repos as _;
    use crucible_sim::pg_model::{PgFaults, PgModel};

    use super::*;

    fn step(id: &'static str, kind: StepKind, statement: &str) -> MigrationStep {
        MigrationStep {
            id,
            kind,
            statements: vec![statement.to_owned()],
            settings: &[],
        }
    }

    fn steps() -> [MigrationStep; 2] {
        [
            step("0001_schema_base", StepKind::Schema, "CREATE TABLE a"),
            step("0002_access_service", StepKind::Access, "CREATE USER b"),
        ]
    }

    /// A stand-in for ClickHouse: counts the steps, and can refuse access steps or outlive the lease.
    #[derive(Default)]
    struct Fake {
        applied: AtomicUsize,
        refuse_access: bool,
        /// Sleep past the lease on this clock, then let another node take the lease.
        outlive: Option<(Arc<dyn crucible_env::Clock>, PgModel)>,
    }

    impl Fake {
        fn applied(&self) -> usize {
            self.applied.load(Ordering::SeqCst)
        }
    }

    impl ApplyStep for Fake {
        fn apply<'a>(&'a self, step: &'a MigrationStep) -> Applying<'a> {
            Box::pin(async move {
                anyhow::ensure!(
                    !(self.refuse_access && step.kind == StepKind::Access),
                    "clickhouse said no"
                );
                if let Some((clock, thief)) = &self.outlive {
                    clock.sleep(LEASE_TTL + Duration::from_secs(1)).await;
                    let taken = thief
                        .leases()
                        .acquire(LeaseName::Migrations, "node-other", LEASE_TTL)
                        .await?;
                    assert!(taken.is_some());
                }
                self.applied.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    /// Runs `body` on a simulated host with an empty `PgModel`.
    fn sim<T>(faults: PgFaults, body: impl AsyncFnOnce(Env, PgModel) -> T + 'static) -> T {
        crucible_sim::run(11, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let model = PgModel::new(host.clock.clone(), sim.rng("pg").as_ref(), faults);
            body(host.env.clone(), model).await
        })
    }

    async fn ledger(model: &PgModel) -> Vec<(String, MigrationKind)> {
        let rows = model.migrations().applied().await.unwrap();
        rows.into_iter().map(|row| (row.id, row.kind)).collect()
    }

    #[test]
    fn steps_are_applied_once_and_recorded_by_kind() {
        sim(PgFaults::default(), async |env, model| {
            let apply = Fake::default();
            apply_steps(&env, &model, "node-a", &steps(), &apply)
                .await
                .unwrap();
            assert_eq!(apply.applied(), 2);
            assert_eq!(
                ledger(&model).await,
                [
                    (
                        "0001_schema_base".to_owned(),
                        MigrationKind::ClickhouseSchema
                    ),
                    (
                        "0002_access_service".to_owned(),
                        MigrationKind::ClickhouseAccess
                    ),
                ]
            );
            // A second run finds both recorded and applies nothing; the released lease is free at once.
            apply_steps(&env, &model, "node-b", &steps(), &apply)
                .await
                .unwrap();
            assert_eq!(apply.applied(), 2);
            // A step whose content changed is applied again; the ledger row stays.
            let mut changed = steps();
            changed[1].statements = vec!["CREATE USER c".to_owned()];
            apply_steps(&env, &model, "node-a", &changed, &apply)
                .await
                .unwrap();
            assert_eq!(apply.applied(), 3);
            assert_eq!(ledger(&model).await.len(), 2);
        });
    }

    #[test]
    fn a_failed_step_is_not_recorded_and_the_lease_is_released() {
        sim(PgFaults::default(), async |env, model| {
            let fail = Fake {
                refuse_access: true,
                ..Fake::default()
            };
            let error = apply_steps(&env, &model, "node-a", &steps(), &fail)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "clickhouse said no");
            assert_eq!(ledger(&model).await.len(), 1);
            let lease = model
                .leases()
                .get(LeaseName::Migrations)
                .await
                .unwrap()
                .unwrap();
            assert!(lease.expires_at < env.clock.wall_now(), "{lease:?}");
            // The next run applies only what is missing.
            let apply = Fake::default();
            apply_steps(&env, &model, "node-b", &steps(), &apply)
                .await
                .unwrap();
            assert_eq!(apply.applied(), 1);
        });
    }

    #[test]
    fn a_migration_waits_for_another_nodes_lease() {
        sim(PgFaults::default(), async |env, model| {
            // Another node holds the lease and stops without releasing it: it expires after its 15 s.
            let other = model
                .leases()
                .acquire(LeaseName::Migrations, "node-other", LEASE_TTL)
                .await
                .unwrap()
                .unwrap();
            let before = env.clock.now();
            let apply = Fake::default();
            apply_steps(&env, &model, "node-a", &steps(), &apply)
                .await
                .unwrap();
            let waited = env.clock.now().saturating_duration_since(before);
            assert!(
                waited >= LEASE_TTL && waited < LEASE_TTL + Duration::from_secs(3),
                "{waited:?}"
            );
            assert_eq!(ledger(&model).await.len(), 2);
            // The stopped node's token is stale now.
            let stale = model.leases().set_cursor(&other, "x").await.unwrap_err();
            assert_eq!(stale, TxError::Fenced);
        });
    }

    #[test]
    fn a_step_that_outlives_the_lease_is_not_recorded_under_a_stale_token() {
        sim(PgFaults::default(), async |env, model| {
            // The step takes longer than the lease lives, and another node takes the lease meanwhile.
            let slow = Fake {
                outlive: Some((Arc::clone(&env.clock), model.clone())),
                ..Fake::default()
            };
            let error = apply_steps(&env, &model, "node-a", &steps(), &slow)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("the migrations lease was lost"),
                "{error}"
            );
            assert!(ledger(&model).await.is_empty());
        });
    }

    #[test]
    fn recording_survives_unknown_commits_and_failovers() {
        let faults = PgFaults {
            unavailable_ppm: 150_000,
            commit_unknown_ppm: 200_000,
            spurious_serialization_ppm: 100_000,
            ..PgFaults::default()
        };
        sim(faults, async |env, model| {
            let apply = Fake::default();
            // Faults can exhaust one run's attempts; a rerun continues, and nothing is recorded twice.
            let mut runs = 0;
            while apply_steps(&env, &model, "node-a", &steps(), &apply)
                .await
                .is_err()
            {
                runs += 1;
                assert!(runs < 50, "the migration never got through");
            }
            model.set_faults(PgFaults::default());
            assert_eq!(ledger(&model).await.len(), 2);
        });
    }

    #[test]
    fn the_checksum_depends_on_every_statement() {
        let a = checksum(&steps()[0]);
        assert_eq!(a.len(), 64);
        assert_eq!(a, checksum(&steps()[0]));
        assert_ne!(a, checksum(&steps()[1]));
        let mut split = steps()[0].clone();
        split.statements = vec!["CREATE ".to_owned(), "TABLE a".to_owned()];
        assert_ne!(a, checksum(&split));
    }
}
