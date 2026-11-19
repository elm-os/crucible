//! The background jobs of this node (contract 11.7): the registry of `crucible-jobs`, and its start.
//!
//! With Postgres configured, every node runs the registry: leader jobs under their lease rows, queue
//! consumers on their claims, per-node timers. Without Postgres only the per-node timers run. The jobs live
//! in the node's shutdown scope: on shutdown a leader lets its run in flight end and releases its lease.

use std::sync::Arc;

use crucible_control::ControlStore;
use crucible_env::Env;
use crucible_export::ExportRuntime;
use crucible_jobs::{Job, Registry, Running, housekeeping};

use crate::control::Control;

/// Every job of the binary. A track adds its job with one line here (docs/architecture/adding-a-job.md). The
/// export jobs need a control plane, whose rows are the exports: without one, `exports` is `None`. `alerts`
/// are the jobs of alerting, which need what `crate::alerts::build` wires: the scheduler of evaluations, their
/// consumer and the consumer of the notification outbox.
fn registry(exports: Option<&ExportRuntime>, alerts: Vec<Job>) -> Registry {
    let mut registry = Registry::new();
    registry.register(housekeeping::job());
    if let Some(exports) = exports {
        registry.register(crucible_export::job(exports.clone()));
        registry.register(crucible_export::sweep_job());
    }
    for job in alerts {
        registry.register(job);
    }
    registry
}

/// Starts the jobs. The holder name of this process in the lease rows is new at every start. The export job
/// reads through `exports`, which gets its query service once the telemetry store is up.
pub(crate) fn start(
    env: &Env,
    control: Option<&Control>,
    exports: &ExportRuntime,
    alerts: Vec<Job>,
) -> Running {
    let store = control.map(|control| Arc::new(control.store.clone()) as Arc<dyn ControlStore>);
    let holder = format!("node-{}", env.new_id());
    registry(control.map(|_| exports), alerts).start(env, store, &holder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_holds_the_housekeeping_job() {
        assert_eq!(registry(None, Vec::new()).names(), ["housekeeping"]);
        let exports = ExportRuntime::new();
        assert_eq!(
            registry(Some(&exports), Vec::new()).names(),
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
            registry(None, jobs).names(),
            ["housekeeping", "alert-scheduler", "alert-evaluations"]
        );
    }

    #[test]
    fn without_postgres_no_job_task_starts() {
        crucible_sim::run(1, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            assert_eq!(
                start(&host.env, None, &ExportRuntime::new(), Vec::new()).tasks(),
                0
            );
        });
    }
}
