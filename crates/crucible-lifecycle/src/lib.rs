//! Retention and deletion (contract 13).
//!
//! - **Retention executor** ([`leader_jobs`], `lifecycle-retention`): hourly under the `lifecycle` lease. A
//!   partition whose every project has expired is dropped; otherwise the rows of each expired project are
//!   deleted with `DELETE ... IN PARTITION` (13.2).
//! - **Mask scheduler** (`lifecycle-mask`): once per 6 h cycle, one `APPLY DELETED MASK` per partition over
//!   its finished deletes (13.2, step 6).
//! - **Deletion executor** (`lifecycle-deletion`): every 30 s. It acts on a request only after every live
//!   node's checkpoint has passed the request's tombstone, and then deletes across the reach list (13.3 and
//!   13.4). A project deletion, which deleting a project makes, reaches every table of the project and then
//!   its control-plane rows. It also issues, polls and verifies the actions of all three jobs.
//! - **Node timer** ([`node_job`], `lifecycle-node`): every 5 s on every node. Heartbeat, pending-deletion
//!   set, tombstone and acknowledgement, checkpoint report, retention snapshot (13.3, steps 2 and 4).
//! - **[`Service`]**: what the API calls: retention settings and deletion requests.
//!
//! A deletion by user reaches the MCP tables by `user_id` and by `user_subject_hash`: the executor hashes the
//! user under the request's issuer, or under every issuer the project's operations carry, with the project's
//! salt through [`Subjects`], which the node provides with the telemetry store ([`LeaderStores`]).
//!
//! Every action on ClickHouse is a row of `lifecycle_actions` that is recorded as issued under the lease's
//! fencing token before it is sent (contract 11.7), and every such action is idempotent. All time comes from
//! the `Env` clock and the database clock, so the jobs run unchanged in simulation
//! (`crucible_sim::lifecycle_sim`).

mod actions;
mod config;
mod deletion;
mod detail;
mod mask;
mod node;
mod partitions;
mod pending;
mod reach;
mod retention;
mod service;
mod wal_log;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crucible_control::LeaseName;
use crucible_env::TelemetryStore;
use crucible_jobs::{Cadence, Job, JobResult, LeaderCtx};
use crucible_records::ProjectId;
use futures::future::BoxFuture;

pub use config::{LifecycleConfig, Mutant};
/// Where export files are when the deployment has an object store: the type of [`LeaderStores::exports`].
pub use crucible_export::ExportObjects;
pub use deletion::{PlaceStatus, Progress};
pub use node::{AckError, LogError, NodeLog, NodeState, node_job};
pub use partitions::{partition_window, partitions_between, partitions_of_days};
pub use pending::{KEEP_AFTER_APPLIED, PendingDeletion, PendingDeletions, RetentionSnapshot};
pub use reach::{
    CONTROL_PLANE, OTHER_PLACES, Reach, final_reach, identity_reach, page_reach, project_reach,
    retention_tables,
};
pub use retention::{Decision, decide};
pub use service::{
    Accepted, MAX_DELETION_PAGE, MAX_RETENTION_DAYS, MAX_SELECTOR_BYTES, NewDeletion, Retention,
    Service, ServiceError, check_retention, check_selector,
};
pub use wal_log::{WalLog, tombstone};

use crate::actions::Shared;

/// The keyed hashes of a user's subject (contract 9.3), which a deletion by user matches on the MCP tables
/// besides `user_id` (contract 13.3). The node implements it over the project salts of the control store.
pub trait Subjects: fmt::Debug + Send + Sync {
    /// `user_subject_hash` of `subject` under each of `issuers`, in their order, with the project's salt.
    /// `Ok(None)` when the project has no salt: then no row of it carries a hash. `Err` says why the salt
    /// cannot be read now, and how to fix that.
    fn hashes<'a>(
        &'a self,
        project_id: ProjectId,
        issuers: &'a [String],
        subject: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<[u8; 16]>>, String>>;
}

/// What the leader jobs act on: the telemetry store, the project salts that a deletion by user hashes its
/// subject with, and the object store that holds export files when the deployment has one.
#[derive(Clone, Debug)]
pub struct LeaderStores {
    /// The telemetry store.
    pub telemetry: Arc<dyn TelemetryStore>,
    /// The project salts.
    pub subjects: Arc<dyn Subjects>,
    /// Where export files are, when the deployment has an object store; `None` keeps them on the nodes' disks.
    pub exports: Option<crucible_export::ExportObjects>,
}

/// Where the leader jobs find their stores. A node opens its telemetry store after its jobs start, and may
/// run without one: while the source answers `None`, a run does nothing.
pub type LeaderSource = Arc<dyn Fn() -> Option<LeaderStores> + Send + Sync>;

/// The three leader jobs of the `lifecycle` lease over a telemetry store: the retention executor, the mask
/// scheduler and the deletion executor. They run one at a time, on the node that holds the lease.
///
/// The retention executor and the mask scheduler wake more often than their period and decide from the
/// lease's cursor whether their hour or cycle is due, so a pass that could not finish is taken up again soon
/// and a change of leader repeats nothing.
pub fn leader_jobs(source: &LeaderSource, config: LifecycleConfig) -> [Job; 3] {
    [
        leader(
            "lifecycle-retention",
            config.retention_every / 4,
            source,
            config,
            |ctx, shared| Box::pin(retention::run(ctx, shared)),
        ),
        leader(
            "lifecycle-mask",
            config.mask_cycle / 12,
            source,
            config,
            |ctx, shared| Box::pin(mask::run(ctx, shared)),
        ),
        leader(
            "lifecycle-deletion",
            config.deletion_every,
            source,
            config,
            |ctx, shared| Box::pin(deletion::run(ctx, shared)),
        ),
    ]
}

type Run = fn(LeaderCtx, Arc<Shared>) -> BoxFuture<'static, JobResult>;

fn leader(
    name: &'static str,
    every: Duration,
    source: &LeaderSource,
    config: LifecycleConfig,
    run: Run,
) -> Job {
    let source = Arc::clone(source);
    Job::leader(
        name,
        LeaseName::Lifecycle,
        Cadence::every(every),
        move |ctx: LeaderCtx| -> BoxFuture<'static, JobResult> {
            match source() {
                Some(stores) => run(
                    ctx,
                    Arc::new(Shared {
                        telemetry: stores.telemetry,
                        subjects: stores.subjects,
                        exports: stores.exports,
                        config,
                    }),
                ),
                None => Box::pin(async { Ok(()) }),
            }
        },
    )
}
