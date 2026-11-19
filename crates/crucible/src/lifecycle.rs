//! Retention and deletion on this node (contract 13): the node's state over its WAL, the jobs, and what the
//! API and the read services take from them.
//!
//! The node timer keeps two snapshots of Postgres in memory, refreshed every 5 s: every project's retention
//! with its floors, and the pending-deletion set. Reads are clamped and filtered with them, so a retention
//! change or a deletion request hides its rows at once on the node that accepted it and within one refresh on
//! every other node.

use std::sync::{Arc, OnceLock};

use crucible_api::{ProjectSettings, ProjectSettingsSource};
use crucible_control::ControlStore;
use crucible_env::{Env, TelemetryStore};
use crucible_jobs::Job;
use crucible_lifecycle::{
    LeaderSource, LeaderStores, LifecycleConfig, NodeState, Service, Subjects, WalLog, leader_jobs,
    node_job,
};
use crucible_query::{Hidden, HiddenSource};
use crucible_records::ProjectId;
use crucible_wal::Wal;

/// The lifecycle of this node. It exists when the node has a control plane.
#[derive(Clone, Debug)]
pub(crate) struct Lifecycle {
    node: Arc<NodeState>,
    /// What the API calls.
    pub(crate) service: Service,
    /// The telemetry store and the project salts, once the store is up. The leader jobs do nothing before
    /// that.
    stores: Arc<OnceLock<LeaderStores>>,
}

impl Lifecycle {
    /// The lifecycle of the node whose WAL is `wal`, over the control store.
    pub(crate) fn new(env: &Env, control: Arc<dyn ControlStore>, wal: Wal) -> Self {
        let node = Arc::new(NodeState::new(
            Arc::new(WalLog(wal)),
            env!("CARGO_PKG_VERSION"),
            "",
        ));
        Self {
            service: Service::new(env, control, Arc::clone(&node)),
            node,
            stores: Arc::default(),
        }
    }

    /// The node timer and the three leader jobs of the `lifecycle` lease.
    pub(crate) fn jobs(&self) -> Vec<Job> {
        let config = LifecycleConfig::default();
        let stores = Arc::clone(&self.stores);
        let source: LeaderSource = Arc::new(move || stores.get().cloned());
        let mut jobs = vec![node_job(Arc::clone(&self.node), &config)];
        jobs.extend(leader_jobs(&source, config));
        jobs
    }

    /// Gives the leader jobs the telemetry store, and the project salts that a deletion by user hashes its
    /// subject with. The first stores stay.
    pub(crate) fn store_is_up(
        &self,
        telemetry: Arc<dyn TelemetryStore>,
        subjects: Arc<dyn Subjects>,
    ) {
        let _ = self.stores.set(LeaderStores {
            telemetry,
            subjects,
        });
    }
}

impl ProjectSettingsSource for Lifecycle {
    fn settings(&self, project_id: ProjectId) -> ProjectSettings {
        let (retention, floors) = self.node.retention().get(project_id);
        ProjectSettings {
            retention,
            floors,
            ..ProjectSettings::default()
        }
    }
}

impl HiddenSource for Lifecycle {
    fn hidden(&self, project_id: ProjectId) -> Vec<Hidden> {
        self.node.pending().hidden(project_id)
    }
}
