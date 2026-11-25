//! The Postgres side of the binary: the control store from the configuration, and the migrations ledger of
//! contract 6.6.
//!
//! With Postgres configured, `crucible migrate` and `serve --migrate` apply the Postgres migrations and then
//! the ClickHouse schema and access steps that the ledger (`schema_migrations`) does not hold yet, or holds with
//! other content. The ClickHouse steps run under the `migrations` lease (contract 11.7), renewed every 5 s while a
//! step applies, and each is recorded with the checksum of what was applied in a transaction fenced by the lease's
//! token, so two nodes that migrate at once never interleave, even over a step that runs longer than the lease
//! lives, and a step whose content changed applies once. A server started without `--migrate` waits until the
//! ledger records every step ([`ensure_migrated`]). Without Postgres there is no ledger and every step is applied
//! each time, which their idempotence allows.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crucible_control::{
    ControlStore, LEASE_RENEW_EVERY, LEASE_SAFETY_MARGIN, LEASE_TTL, Lease, LeaseName, LedgerEntry,
    MigrationKind, TxError,
};
use crucible_control_pg::{PgConfig, PgControlStore};
use crucible_env::{ClockExt as _, Env, Instant};
use crucible_schema::{LEDGER_ORDER, MigrationStep, StepKind, without_secrets};
use sha2::{Digest, Sha256};

/// How long a migration waits for another node's `migrations` lease before it gives up.
const LEASE_WAIT: Duration = Duration::from_secs(60);
/// The pause between attempts to take the lease, and between attempts to record a step.
const RETRY_EVERY: Duration = Duration::from_secs(1);
/// How often recording a step is tried again after a retryable failure.
const RECORD_ATTEMPTS: u32 = 5;

/// A step being applied.
pub(crate) type Applying<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Why the service identities refuse this node's master key, or `None` when they accept it.
pub(crate) type Refusal<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// What applies a step to ClickHouse.
pub(crate) trait ApplyStep: Sync {
    /// Applies the step. It is idempotent, so a step that failed part-way is applied again.
    fn apply<'a>(&'a self, step: &'a MigrationStep) -> Applying<'a>;

    /// Why the service identities refuse this node's master key, or `None` when they accept it. A step that the
    /// ledger records with other content applies again only under a key they accept: a start with another master
    /// key applies nothing that a start with the right key recorded (contract 6.6).
    fn refusal(&self) -> Refusal<'_>;
}

/// The steps that changed since the ledger recorded them and were not applied again, because the service identities
/// refuse this node's master key. They apply at the next migration with the key the deployment was migrated with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Deferred {
    /// The steps, in order.
    pub(crate) steps: Vec<&'static str>,
    /// Why the service identities refuse the key.
    pub(crate) reason: String,
}

impl std::fmt::Display for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (noun, them, they) = if self.steps.len() == 1 {
            ("step", "it", "it is")
        } else {
            ("steps", "them", "they are")
        };
        write!(
            f,
            "ClickHouse migration {noun} {} changed since the migrations ledger recorded {them}, and the service \
             identities refuse this server's master key ({}), so {they} not applied. Run `crucible migrate` again \
             with the master key this deployment was migrated with (master_key_file, --master-key-file or \
             CRUCIBLE_MASTER_KEY); the other steps are applied",
            self.steps.join(", "),
            self.reason
        )
    }
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

    /// Applies the Postgres migrations, then the steps of `steps` that the ledger does not hold. Answers the
    /// changed steps that wait for another master key.
    pub(crate) async fn migrate(
        &self,
        env: &Env,
        steps: &[MigrationStep],
        apply: &dyn ApplyStep,
    ) -> anyhow::Result<Option<Deferred>> {
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

/// The ledger checksum of a step: SHA-256 over its statements with the password digests left out. The digests are
/// derived from the master key, so a start with another key finds every step a start with the right key recorded
/// and applies none of them again (contract 6.6).
pub(crate) fn checksum(step: &MigrationStep) -> String {
    digest(step.statements.iter().map(|s| without_secrets(s)))
}

/// The checksum that ledger rows written before the digests were left out hold: over the whole statements. Such a
/// row still matches under the master key it was recorded with.
fn checksum_with_digests(step: &MigrationStep) -> String {
    digest(step.statements.iter().map(|s| Cow::Borrowed(s.as_str())))
}

fn digest<'a>(statements: impl Iterator<Item = Cow<'a, str>>) -> String {
    let mut hasher = Sha256::new();
    for statement in statements {
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
            && applied
                .iter()
                .any(|row| row.id == entry.id && row.checksum == entry.checksum)
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

/// Applies `step` while renewing `lease` every 5 s (contract 11.7), so that a step that runs longer than the
/// lease lives, such as the copy of step 0005, keeps it, and a node that starts meanwhile waits instead of
/// applying the step beside it. `asked` is when the last renewal was sent. The step ends with an error when the
/// lease is lost: Postgres says so, or the lease's local window ends before a renewal got through. ClickHouse
/// may still finish the statement in flight; the step is idempotent, so the next run applies it again.
async fn apply_holding(
    env: &Env,
    store: &dyn ControlStore,
    lease: &mut Lease,
    mut asked: Instant,
    apply: &dyn ApplyStep,
    step: &MigrationStep,
) -> anyhow::Result<()> {
    let lost = || {
        anyhow::anyhow!(
            "the migrations lease was lost while step {} ran, so it is not recorded. Run the migration \
             again: the step is idempotent",
            step.id
        )
    };
    let window = LEASE_TTL.saturating_sub(LEASE_SAFETY_MARGIN);
    let mut next = asked.saturating_add(LEASE_RENEW_EVERY);
    let mut applying = apply.apply(step);
    loop {
        tokio::select! {
            biased;
            applied = &mut applying => return applied,
            () = env.clock.sleep_until(next) => {}
        }
        let until = asked.saturating_add(window);
        let sent = env.clock.now();
        if sent >= until {
            return Err(lost());
        }
        match env
            .clock
            .timeout_at(until, store.leases().renew(lease, LEASE_TTL))
            .await
        {
            Ok(Ok(Some(renewed))) => {
                *lease = renewed;
                asked = sent;
                next = sent.saturating_add(LEASE_RENEW_EVERY);
            }
            Ok(Ok(None)) | Err(_) => return Err(lost()),
            // Nothing was renewed, or the outcome is unknown: the last renewal still bounds this node.
            Ok(Err(error)) => {
                tracing::debug!(
                    %error,
                    step = step.id,
                    "renewing the migrations lease failed; trying again"
                );
                next = env.clock.now().saturating_add(RETRY_EVERY);
            }
        }
    }
}

/// Refuses unless the ledger records every step of this version (`crucible_schema::LEDGER_ORDER`). A server
/// started without `--migrate` checks it before it takes ingest, so its shipper never writes before another node or
/// `crucible migrate` has applied the steps it writes for, such as the copy of the minute rollup that step 0005
/// makes before the shipper fills the hourly rollup (contract 6.6).
pub(crate) async fn ensure_migrated(store: &dyn ControlStore) -> anyhow::Result<()> {
    let recorded =
        store.migrations().applied().await.context(
            "reading the migrations ledger in Postgres, which `crucible migrate` creates",
        )?;
    let missing: Vec<&str> = LEDGER_ORDER
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !recorded.iter().any(|row| row.id == *id))
        .collect();
    anyhow::ensure!(
        missing.is_empty(),
        "this version needs ClickHouse migration {} {}, which the migrations ledger does not record: run \
         `crucible migrate`, or start one server with --migrate",
        if missing.len() == 1 { "step" } else { "steps" },
        missing.join(", ")
    );
    Ok(())
}

/// Applies the steps the ledger does not hold, in order, under the `migrations` lease, and records each with the
/// checksum of what was applied. A step that the ledger holds with other content applies again too, unless the
/// service identities refuse this node's master key ([`ApplyStep::refusal`]); such steps are answered.
pub(crate) async fn apply_steps(
    env: &Env,
    store: &dyn ControlStore,
    holder: &str,
    steps: &[MigrationStep],
    apply: &dyn ApplyStep,
) -> anyhow::Result<Option<Deferred>> {
    if steps.is_empty() {
        return Ok(None);
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
        // Asked once, and only when a recorded step changed.
        let mut refusal = None;
        let mut deferred = Vec::new();
        for step in steps {
            let checksum = checksum(step);
            match ledger.get(step.id) {
                Some(recorded)
                    if *recorded == checksum || *recorded == checksum_with_digests(step) =>
                {
                    tracing::debug!(step = step.id, "migration step already recorded");
                    continue;
                }
                Some(_) => {
                    if refusal.is_none() {
                        refusal = Some(apply.refusal().await);
                    }
                    if let Some(Some(reason)) = &refusal {
                        tracing::warn!(
                            step = step.id,
                            reason,
                            "this step changed since the ledger recorded it, and the service identities refuse \
                             this node's master key, so it is not applied again: it applies at the next start \
                             with the master key this deployment was migrated with"
                        );
                        deferred.push(step.id);
                        continue;
                    }
                    tracing::warn!(
                        step = step.id,
                        "the ledger records this step with other content (for example from an earlier version, \
                         or with another sql.statements_per_hour); applying it again"
                    );
                }
                None => {}
            }
            let asked = env.clock.now();
            lease = store
                .leases()
                .renew(&lease, LEASE_TTL)
                .await
                .context("renewing the migrations lease")?
                .context("the migrations lease was lost; run the migration again")?;
            apply_holding(env, store, &mut lease, asked, apply, step).await?;
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
        Ok(match refusal {
            Some(Some(reason)) if !deferred.is_empty() => Some(Deferred {
                steps: deferred,
                reason,
            }),
            _ => None,
        })
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crucible_control::Repos as _;
    use crucible_schema::SqlProfile;
    use crucible_sim::pg_model::{PgFaults, PgModel};
    use crucible_store_clickhouse::Credentials;

    use super::*;
    use crate::store::{ClickHouse, MasterKey};

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

    /// A stand-in for ClickHouse: counts the steps, and can refuse access steps or run past the lease.
    #[derive(Default)]
    struct Fake {
        applied: AtomicUsize,
        /// The ids of the steps applied, in order.
        log: Mutex<Vec<&'static str>>,
        refuse_access: bool,
        /// The service identities refuse this node's master key.
        refuse_key: bool,
        /// Run for three lease lifetimes on this clock while another node tries every second to take the lease.
        outlive: Option<(Arc<dyn crucible_env::Clock>, PgModel)>,
        /// Step the database clock past the lease, let another node take it, and finish at once.
        steal: Option<PgModel>,
        /// Make Postgres unavailable, then run for two lease lifetimes on this clock.
        cut_off: Option<(Arc<dyn crucible_env::Clock>, PgModel)>,
    }

    impl Fake {
        fn applied(&self) -> usize {
            self.applied.load(Ordering::SeqCst)
        }

        /// The ids of the steps applied so far, in order.
        fn steps(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().clone()
        }
    }

    impl ApplyStep for Fake {
        fn apply<'a>(&'a self, step: &'a MigrationStep) -> Applying<'a> {
            Box::pin(async move {
                anyhow::ensure!(
                    !(self.refuse_access && step.kind == StepKind::Access),
                    "clickhouse said no"
                );
                if let Some((clock, other)) = &self.outlive {
                    for _ in 0..3 * LEASE_TTL.as_secs() {
                        clock.sleep(Duration::from_secs(1)).await;
                        let taken = other
                            .leases()
                            .acquire(LeaseName::Migrations, "node-other", LEASE_TTL)
                            .await?;
                        assert!(
                            taken.is_none(),
                            "another node took the lease of a running step"
                        );
                    }
                }
                if let Some(thief) = &self.steal {
                    let past = LEASE_TTL + Duration::from_secs(1);
                    thief.step_db_clock(i64::try_from(past.as_micros())?);
                    let taken = thief
                        .leases()
                        .acquire(LeaseName::Migrations, "node-other", LEASE_TTL)
                        .await?;
                    assert!(taken.is_some());
                }
                if let Some((clock, model)) = &self.cut_off {
                    model.set_faults(PgFaults {
                        unavailable_ppm: 1_000_000,
                        ..PgFaults::default()
                    });
                    clock.sleep(2 * LEASE_TTL).await;
                }
                self.applied.fetch_add(1, Ordering::SeqCst);
                self.log.lock().unwrap().push(step.id);
                Ok(())
            })
        }

        fn refusal(&self) -> Refusal<'_> {
            let refused = self
                .refuse_key
                .then(|| "AUTHENTICATION_FAILED for crucible_maint".to_owned());
            Box::pin(async move { refused })
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
            // A step whose content changed is applied again, once: its row takes the new checksum.
            let mut changed = steps();
            changed[1].statements = vec!["CREATE USER c".to_owned()];
            for holder in ["node-a", "node-b"] {
                apply_steps(&env, &model, holder, &changed, &apply)
                    .await
                    .unwrap();
                assert_eq!(apply.applied(), 3);
            }
            let rows = model.migrations().applied().await.unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| (row.id.as_str(), row.checksum.clone()))
                    .collect::<Vec<_>>(),
                [
                    ("0001_schema_base", checksum(&changed[0])),
                    ("0002_access_service", checksum(&changed[1])),
                ]
            );
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
    fn a_step_that_runs_longer_than_the_lease_keeps_it() {
        sim(PgFaults::default(), async |env, model| {
            // Each step runs for 45 s. The lease is renewed every 5 s meanwhile, so the other node never takes
            // it, and both steps are recorded.
            let slow = Fake {
                outlive: Some((Arc::clone(&env.clock), model.clone())),
                ..Fake::default()
            };
            apply_steps(&env, &model, "node-a", &steps(), &slow)
                .await
                .unwrap();
            assert_eq!(slow.applied(), 2);
            assert_eq!(ledger(&model).await.len(), 2);
        });
    }

    #[test]
    fn a_step_whose_lease_another_node_took_is_not_recorded_under_a_stale_token() {
        sim(PgFaults::default(), async |env, model| {
            let stolen = Fake {
                steal: Some(model.clone()),
                ..Fake::default()
            };
            let error = apply_steps(&env, &model, "node-a", &steps(), &stolen)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("the migrations lease was lost while step 0001_schema_base ran"),
                "{error}"
            );
            assert!(ledger(&model).await.is_empty());
        });
    }

    #[test]
    fn a_step_that_cannot_renew_the_lease_ends_before_the_lease_does() {
        sim(PgFaults::default(), async |env, model| {
            // Postgres is unavailable while the step runs, so no renewal gets through: the step ends when the
            // lease's local window does, 2 s before the lease expires and another node could take it.
            let cut = Fake {
                cut_off: Some((Arc::clone(&env.clock), model.clone())),
                ..Fake::default()
            };
            let before = env.clock.now();
            let error = apply_steps(&env, &model, "node-a", &steps(), &cut)
                .await
                .unwrap_err();
            let ran = env.clock.now().saturating_duration_since(before);
            assert!(
                error
                    .to_string()
                    .contains("the migrations lease was lost while step 0001_schema_base ran"),
                "{error}"
            );
            assert!(ran < LEASE_TTL, "{ran:?}");
            model.set_faults(PgFaults::default());
            assert_eq!(cut.applied(), 0);
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

    /// The ledger checksum leaves the password digests out: a step recorded under one master key is found under
    /// another and applied once only. A row from before holds the checksum over the whole statements, and still
    /// matches under the key it was recorded with.
    #[test]
    fn steps_are_recorded_without_their_password_digests() {
        let user = |digest: char| {
            step(
                "0002_access_service",
                StepKind::Access,
                &format!(
                    "CREATE USER IF NOT EXISTS b IDENTIFIED WITH sha256_hash BY '{}' SETTINGS PROFILE 'p'",
                    digest.to_string().repeat(64)
                ),
            )
        };
        sim(PgFaults::default(), async move |env, model| {
            let apply = Fake::default();
            apply_steps(&env, &model, "node-a", &[user('a')], &apply)
                .await
                .unwrap();
            apply_steps(&env, &model, "node-b", &[user('b')], &apply)
                .await
                .unwrap();
            assert_eq!(apply.applied(), 1);
            let before = LedgerEntry {
                id: "0003_access_workspace".to_owned(),
                kind: MigrationKind::ClickhouseAccess,
                applied_at: env.clock.wall_now(),
                checksum: checksum_with_digests(&user('a')),
            };
            let mut workspace = user('a');
            workspace.id = "0003_access_workspace";
            assert!(model.migrations().apply(&before).await.unwrap());
            apply_steps(&env, &model, "node-a", &[workspace], &apply)
                .await
                .unwrap();
            assert_eq!(apply.applied(), 1);
        });
    }

    /// What a test's log subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        /// What was written since the last call.
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Captures this thread's log, at the server's level, until the guard drops. The simulation runs on this
    /// thread.
    fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
        let captured = Captured::default();
        let writer = captured.clone();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish(),
        );
        (captured, guard)
    }

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const OTHER_KEY: &str = "0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad";

    /// This version's steps as `--migrate` renders them under `key` in the default configuration: the admin user
    /// `crucible_admin`, `sql.statements_per_hour = 1800` and the SQL workspace profile's defaults.
    fn release_steps(key: &str) -> [MigrationStep; 5] {
        ClickHouse {
            url: "http://localhost:8123".to_owned(),
            admin: Credentials {
                user: "crucible_admin".to_owned(),
                password: "admin-secret".to_owned(),
            },
            master_key: MasterKey::parse(key).unwrap(),
            sql_statements_per_hour: 1_800,
            idle_timeout: None,
            cluster: None,
        }
        .steps(SqlProfile::default())
        .unwrap()
    }

    /// The ledger that crucible 0.1.1 records in the configuration of [`release_steps`]: its four steps with the
    /// checksums that its own code computes (`steps` and `checksum` of the binary, `profile_caps` of
    /// `crucible-plan`, run at the tag `v0.1.1`).
    const LEDGER_0_1_1: [(&str, MigrationKind, &str); 4] = [
        (
            "0001_schema_base",
            MigrationKind::ClickhouseSchema,
            "6c945aaa8b837ff32b9d45bd6c6e706dcc14cfe457a2fb7788c5b643b0942d14",
        ),
        (
            "0002_access_service",
            MigrationKind::ClickhouseAccess,
            "62be73963edae234c6adc63d17bbe6dbad0aa0d6c1791d28e8e6694e1c39ff23",
        ),
        (
            "0003_access_workspace",
            MigrationKind::ClickhouseAccess,
            "80517a4776b6d84eef8c5228348adda358038baff26c203f0615f15e704d7994",
        ),
        (
            "0004_span_usage_aggregate",
            MigrationKind::ClickhouseSchema,
            "353adc83aeb9edd0e807ecf25e11c76a9bfcd2e11a4400966202b9de0405ee56",
        ),
    ];

    /// The steps a start applies on a ledger that 0.1.1 recorded.
    const UPGRADE: [&str; 4] = [
        "0001_schema_base",
        "0002_access_service",
        "0003_access_workspace",
        "0005_mcp_tool_rollup_hourly",
    ];

    async fn record_0_1_1(env: &Env, model: &PgModel) {
        for (id, kind, checksum) in LEDGER_0_1_1 {
            let entry = LedgerEntry {
                id: id.to_owned(),
                kind,
                applied_at: env.clock.wall_now(),
                checksum: checksum.to_owned(),
            };
            assert!(model.migrations().apply(&entry).await.unwrap());
        }
    }

    /// Whether a start's log tells of a step applied, or of one recorded with other content.
    fn applied_something(log: &str) -> bool {
        log.contains("migration step applied") || log.contains("with other content")
    }

    /// A server upgraded from 0.1.1 finds steps 0001 to 0003 recorded with other content, because they render from
    /// the table list and it gained `mcp_tool_rollup_hourly`. Its first start applies them again, with a warning,
    /// and step 0005. The second start and every later one find each step recorded with the content it applied,
    /// so they apply nothing and warn of nothing (contract 6.6).
    #[test]
    fn a_ledger_from_0_1_1_applies_each_changed_step_once() {
        let (log, _guard) = capture();
        sim(PgFaults::default(), async move |env, model| {
            record_0_1_1(&env, &model).await;
            let steps = release_steps(KEY);
            for ((id, _, recorded), step) in LEDGER_0_1_1.into_iter().zip(&steps) {
                assert_eq!(step.id, id);
                // Step 0004 is as 0.1.1 shipped it.
                assert_eq!(
                    checksum(step) == recorded,
                    id == "0004_span_usage_aggregate",
                    "{id}"
                );
            }
            let apply = Fake::default();
            apply_steps(&env, &model, "node-a", &steps, &apply)
                .await
                .unwrap();
            assert_eq!(apply.steps(), UPGRADE);
            let first = log.take();
            assert_eq!(first.matches("with other content").count(), 3, "{first}");
            let recorded: Vec<(String, String)> = model
                .migrations()
                .applied()
                .await
                .unwrap()
                .into_iter()
                .map(|row| (row.id, row.checksum))
                .collect();
            let applied: Vec<(String, String)> = steps
                .iter()
                .map(|step| (step.id.to_owned(), checksum(step)))
                .collect();
            assert_eq!(recorded, applied);
            for holder in ["node-b", "node-a"] {
                apply_steps(&env, &model, holder, &steps, &apply)
                    .await
                    .unwrap();
                assert_eq!(apply.steps(), UPGRADE);
                let later = log.take();
                assert!(!applied_something(&later), "{later}");
            }
        });
    }

    /// The service identities refuse every master key but the one the deployment was migrated with.
    fn other_key() -> Fake {
        Fake {
            refuse_key: true,
            ..Fake::default()
        }
    }

    /// A start with another master key finds every step recorded, also on a ledger that an upgrade from 0.1.1
    /// brought up to date, so it sends no statement: no identity's password changes (contract 6.6, #114).
    #[test]
    fn a_start_with_another_master_key_applies_nothing() {
        let (log, _guard) = capture();
        for upgraded in [false, true] {
            let log = log.clone();
            sim(PgFaults::default(), async move |env, model| {
                if upgraded {
                    record_0_1_1(&env, &model).await;
                }
                let right = Fake::default();
                apply_steps(&env, &model, "node-a", &release_steps(KEY), &right)
                    .await
                    .unwrap();
                let _ = log.take();
                let wrong = other_key();
                apply_steps(&env, &model, "node-b", &release_steps(OTHER_KEY), &wrong)
                    .await
                    .unwrap();
                assert!(wrong.steps().is_empty(), "upgraded: {upgraded}");
                let other = log.take();
                assert!(!applied_something(&other), "{other}");
            });
        }
    }

    /// The first start after an upgrade from 0.1.1 uses another master key. The service identities refuse it, so
    /// the steps that 0.1.1 recorded with other content are not applied again under it (access step A2 would give
    /// the probe identities that key's passwords); only step 0005, which is new and holds no password, applies.
    /// The next start, with the right key, applies the changed steps once (contract 6.6, #114).
    #[test]
    fn an_upgrade_started_with_another_master_key_applies_no_changed_step() {
        let (log, _guard) = capture();
        sim(PgFaults::default(), async move |env, model| {
            record_0_1_1(&env, &model).await;
            let wrong = other_key();
            let deferred = apply_steps(&env, &model, "node-a", &release_steps(OTHER_KEY), &wrong)
                .await
                .unwrap()
                .expect("the changed steps wait for the right key");
            assert_eq!(wrong.steps(), ["0005_mcp_tool_rollup_hourly"]);
            // `crucible migrate` fails with this, so it says which steps wait, why, and what to run.
            assert_eq!(deferred.steps, UPGRADE[..3]);
            let said = deferred.to_string();
            for needed in [
                "steps 0001_schema_base, 0002_access_service, 0003_access_workspace",
                "refuse this server's master key (AUTHENTICATION_FAILED for crucible_maint)",
                "`crucible migrate` again with the master key this deployment was migrated with",
            ] {
                assert!(said.contains(needed), "{needed:?} is missing from: {said}");
            }
            let refused = log.take();
            assert_eq!(
                refused
                    .matches("refuse this node's master key, so it is not applied again")
                    .count(),
                3,
                "{refused}"
            );
            assert!(!refused.contains("with other content"), "{refused}");
            let steps = release_steps(KEY);
            let right = Fake::default();
            let deferred = apply_steps(&env, &model, "node-b", &steps, &right)
                .await
                .unwrap();
            assert_eq!(deferred, None);
            assert_eq!(right.steps(), UPGRADE[..3]);
            let recorded: Vec<(String, String)> = model
                .migrations()
                .applied()
                .await
                .unwrap()
                .into_iter()
                .map(|row| (row.id, row.checksum))
                .collect();
            let applied: Vec<(String, String)> = steps
                .iter()
                .map(|step| (step.id.to_owned(), checksum(step)))
                .collect();
            assert_eq!(recorded, applied);
            let later = Fake::default();
            apply_steps(&env, &model, "node-a", &steps, &later)
                .await
                .unwrap();
            assert!(later.steps().is_empty());
        });
    }

    /// A server started without `--migrate` opens ingest only once the ledger records every step of its version,
    /// so its shipper never writes before step 0005 has copied the minute rollup. Until then the reason names the
    /// missing step and the two ways to apply it (contract 1.7).
    #[test]
    fn a_server_without_migrate_waits_until_every_step_is_recorded() {
        sim(PgFaults::default(), async |env, model| {
            record_0_1_1(&env, &model).await;
            let error = format!("{:#}", ensure_migrated(&model).await.unwrap_err());
            assert!(
                error.contains("step 0005_mcp_tool_rollup_hourly")
                    && error.contains("run `crucible migrate`")
                    && error.contains("--migrate"),
                "{error}"
            );
            apply_steps(
                &env,
                &model,
                "node-a",
                &release_steps(KEY),
                &Fake::default(),
            )
            .await
            .unwrap();
            ensure_migrated(&model).await.unwrap();
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
