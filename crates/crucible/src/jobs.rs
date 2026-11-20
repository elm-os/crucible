//! The background jobs of this node (contract 11.7): the registry of `crucible-jobs`, and its start.
//!
//! With Postgres configured, every node runs the registry: leader jobs under their lease rows, queue
//! consumers on their claims, per-node timers. They start once the control plane is ready. Without Postgres
//! only the per-node timers run. The jobs live in the node's shutdown scope: on shutdown a leader lets its run
//! in flight end and releases its lease.

use std::sync::Arc;
use std::time::Duration;

use crucible_auth::DbKeys;
use crucible_control::ControlStore;
use crucible_env::{Env, JoinHandle};
use crucible_export::ExportRuntime;
use crucible_jobs::{Job, Registry, housekeeping};

use crate::lifecycle::Lifecycle;

/// How often the start of the jobs looks whether the control plane is ready.
const READY_POLL: Duration = Duration::from_millis(250);

/// Every job of the binary. A track adds its job with one line here (docs/architecture/adding-a-job.md). The
/// export jobs need a control plane, whose rows are the exports: without one, `exports` is `None`, and so
/// is `lifecycle`, which also needs the WAL of the node. `alerts` are the jobs of alerting, which need what
/// `crate::alerts::build` wires: the scheduler of evaluations, their consumer and the consumer of the
/// notification outbox.
fn registry(
    exports: Option<&ExportRuntime>,
    alerts: Vec<Job>,
    lifecycle: Option<&Lifecycle>,
) -> Registry {
    let mut registry = Registry::new();
    registry.register(housekeeping::job());
    if let Some(exports) = exports {
        registry.register(crucible_export::job(exports.clone()));
        registry.register(crucible_export::sweep_job());
    }
    for job in alerts {
        registry.register(job);
    }
    for job in lifecycle.into_iter().flat_map(Lifecycle::jobs) {
        registry.register(job);
    }
    registry
}

/// Starts the jobs once the control plane is ready, in a task that ends when they all have stopped after the
/// shutdown signal. With Postgres, `control` is the store and the key snapshot: the snapshot loads only once
/// Postgres answers and its migrations have run (with `--migrate`, the key feed starts after them), so no first run
/// meets a table that does not exist yet. Without Postgres only the per-node timers start, at once. The holder name
/// of this process in the lease rows is new at every start. The export job reads through `exports`, which gets its
/// query service once the telemetry store is up.
pub(crate) fn start(
    env: &Env,
    control: Option<(Arc<dyn ControlStore>, Arc<DbKeys>)>,
    exports: &ExportRuntime,
    alerts: Vec<Job>,
    lifecycle: Option<&Lifecycle>,
) -> JoinHandle<()> {
    let registry = registry(control.as_ref().map(|_| exports), alerts, lifecycle);
    let holder = format!("node-{}", env.new_id());
    let task_env = env.clone();
    env.spawn("jobs", async move {
        let env = task_env;
        let mut shutdown = env.shutdown();
        let store = match control {
            Some((store, keys)) => {
                while !keys.is_loaded() {
                    tokio::select! {
                        biased;
                        () = &mut shutdown => return,
                        () = env.clock.sleep(READY_POLL) => {}
                    }
                }
                Some(store)
            }
            None => None,
        };
        let mut running = registry.start(&env, store, &holder);
        if running.tasks() > 0 {
            tracing::info!(tasks = running.tasks(), "the background jobs are running");
        }
        running.join().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_holds_the_housekeeping_job() {
        assert_eq!(registry(None, Vec::new(), None).names(), ["housekeeping"]);
        let exports = ExportRuntime::new();
        assert_eq!(
            registry(Some(&exports), Vec::new(), None).names(),
            ["housekeeping", "exports", "exports-sweep"]
        );
    }

    #[derive(Debug)]
    struct NoSeries;

    #[async_trait::async_trait]
    impl crucible_alerts::SeriesSource for NoSeries {
        async fn read(
            &self,
            _: &crucible_control::Project,
            _: &crucible_alerts::RuleSpec,
            _: crucible_records::TimeWindow,
        ) -> Result<crucible_alerts::Observed, crucible_alerts::ReadFailure> {
            Ok(crucible_alerts::Observed::default())
        }
    }

    #[test]
    fn the_registry_holds_the_jobs_of_alerting() {
        let evaluator = crucible_alerts::Evaluator::new(Arc::new(NoSeries));
        let jobs = Vec::from(crucible_alerts::jobs(evaluator));
        assert_eq!(
            registry(None, jobs, None).names(),
            ["housekeeping", "alert-scheduler", "alert-evaluations"]
        );
    }

    #[test]
    fn with_a_control_plane_the_registry_also_holds_the_lifecycle_jobs() {
        crucible_sim::run(1, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let (wal, _) = crucible_wal::Wal::open(&host.env, crucible_wal::WalConfig::default())
                .await
                .unwrap();
            let control = Arc::new(crucible_sim::pg_model::PgModel::new(
                host.clock.clone(),
                sim.rng("pg").as_ref(),
                crucible_sim::pg_model::PgFaults::default(),
            ));
            let lifecycle = Lifecycle::new(&host.env, control, wal);
            assert_eq!(
                registry(None, Vec::new(), Some(&lifecycle)).names(),
                [
                    "housekeeping",
                    "lifecycle-node",
                    "lifecycle-retention",
                    "lifecycle-mask",
                    "lifecycle-deletion"
                ]
            );
        });
    }

    /// A first start applies the Postgres migrations while the node starts. A job that ran before they had would
    /// meet a table that does not exist yet and log a warning, so the jobs start once the control plane is ready:
    /// the migrations have run and the key snapshot has loaded.
    #[test]
    fn the_jobs_start_once_the_control_plane_is_ready() {
        use crucible_auth::{LookupKey, OutageMode};
        use crucible_control::{LeaseName, Repos as _};
        use crucible_sim::pg_model::{PgFaults, PgModel};

        crucible_sim::run(3, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let env = host.env.clone();
            let model = PgModel::new(
                host.clock.clone(),
                sim.rng("pg").as_ref(),
                PgFaults::default(),
            );
            let keys = Arc::new(DbKeys::new(
                &env,
                LookupKey::new([5; 32]),
                OutageMode::default(),
            ));
            let store: Arc<dyn ControlStore> = Arc::new(model.clone());
            let _jobs = start(
                &env,
                Some((store, Arc::clone(&keys))),
                &ExportRuntime::new(),
                Vec::new(),
                None,
            );
            // The housekeeping job takes its lease as soon as it runs.
            let housekeeping = async || {
                model
                    .leases()
                    .get(LeaseName::Housekeeping)
                    .await
                    .unwrap()
                    .is_some()
            };
            env.clock.sleep(Duration::from_secs(30)).await;
            assert!(
                !housekeeping().await,
                "a job ran before the control plane was ready"
            );
            keys.load(&model.key_feed().snapshot().await.unwrap());
            env.clock.sleep(Duration::from_secs(1)).await;
            assert!(
                housekeeping().await,
                "the jobs did not start once the key snapshot had loaded"
            );
        });
    }

    #[test]
    fn without_postgres_no_job_task_starts() {
        crucible_sim::run(1, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let mut jobs = start(&host.env, None, &ExportRuntime::new(), Vec::new(), None);
            // A job task runs until shutdown; with none started, the task that holds them ends at once.
            assert_eq!(
                host.env
                    .timeout(Duration::from_millis(1), jobs.join())
                    .await,
                Ok(Ok(()))
            );
        });
    }
}
