//! The background jobs of this node (contract 11.7): the registry of `crucible-jobs`, and its start.
//!
//! With Postgres configured, every node runs the registry: leader jobs under their lease rows, queue
//! consumers on their claims, per-node timers. Without Postgres only the per-node timers run. The jobs live
//! in the node's shutdown scope: on shutdown a leader lets its run in flight end and releases its lease.

use std::sync::Arc;

use crucible_control::ControlStore;
use crucible_env::Env;
use crucible_jobs::{Registry, Running, housekeeping};

use crate::control::Control;

/// Every job of the binary. A track adds its job with one line here (docs/architecture/adding-a-job.md).
fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(housekeeping::job());
    registry
}

/// Starts the jobs. The holder name of this process in the lease rows is new at every start.
pub(crate) fn start(env: &Env, control: Option<&Control>) -> Running {
    let store = control.map(|control| Arc::new(control.store.clone()) as Arc<dyn ControlStore>);
    let holder = format!("node-{}", env.new_id());
    registry().start(env, store, &holder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_holds_the_housekeeping_job() {
        assert_eq!(registry().names(), ["housekeeping"]);
    }

    #[test]
    fn without_postgres_no_job_task_starts() {
        crucible_sim::run(1, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            assert_eq!(start(&host.env, None).tasks(), 0);
        });
    }
}
