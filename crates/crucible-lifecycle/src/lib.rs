//! Retention and deletion (contract 13).
//!
//! - **Retention executor** ([`leader_jobs`], `lifecycle-retention`): hourly under the `lifecycle` lease. A
//!   partition whose every project has expired is dropped; otherwise the rows of each expired project are
//!   deleted with `DELETE ... IN PARTITION` (13.2).
//! - **Mask scheduler** (`lifecycle-mask`): once per 6 h cycle, one `APPLY DELETED MASK` per partition over
//!   its finished deletes (13.2, step 6).
//! - **Deletion executor** (`lifecycle-deletion`): every 30 s. It acts on a request only after every live
//!   node's checkpoint has passed the request's tombstone, and then deletes across the reach list (13.3 and
//!   13.4). It also issues, polls and verifies the actions of all three jobs.
//! - **Node timer** ([`node_job`], `lifecycle-node`): every 5 s on every node. Heartbeat, pending-deletion
//!   set, tombstone and acknowledgement, checkpoint report, retention snapshot (13.3, steps 2 and 4).
//! - **[`Service`]**: what the API calls: retention settings and deletion requests.
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

use std::sync::Arc;
use std::time::Duration;

use crucible_control::LeaseName;
use crucible_env::TelemetryStore;
use crucible_jobs::{Cadence, Job, JobResult, LeaderCtx};
use futures::future::BoxFuture;

pub use config::{LifecycleConfig, Mutant};
pub use deletion::{PlaceStatus, Progress};
pub use node::{AckError, LogError, NodeLog, NodeState, node_job};
pub use partitions::{partition_window, partitions_between, partitions_of_days};
pub use pending::{KEEP_AFTER_APPLIED, PendingDeletion, PendingDeletions, RetentionSnapshot};
pub use reach::{OTHER_PLACES, Reach, final_reach, page_reach, retention_tables};
pub use retention::{Decision, decide};
pub use service::{
    Accepted, MAX_DELETION_PAGE, MAX_RETENTION_DAYS, MAX_SELECTOR_BYTES, NewDeletion, Retention,
    Service, ServiceError, check_retention, check_selector,
};
pub use wal_log::{WalLog, tombstone};

use crate::actions::Shared;

/// Where the leader jobs find the telemetry store. A node opens its store after its jobs start, and may run
/// without one: while the source answers `None`, a run does nothing.
pub type TelemetrySource = Arc<dyn Fn() -> Option<Arc<dyn TelemetryStore>> + Send + Sync>;

/// The three leader jobs of the `lifecycle` lease over a telemetry store: the retention executor, the mask
/// scheduler and the deletion executor. They run one at a time, on the node that holds the lease.
///
/// The retention executor and the mask scheduler wake more often than their period and decide from the
/// lease's cursor whether their hour or cycle is due, so a pass that could not finish is taken up again soon
/// and a change of leader repeats nothing.
pub fn leader_jobs(telemetry: &TelemetrySource, config: LifecycleConfig) -> [Job; 3] {
    [
        leader(
            "lifecycle-retention",
            config.retention_every / 4,
            telemetry,
            config,
            |ctx, shared| Box::pin(retention::run(ctx, shared)),
        ),
        leader(
            "lifecycle-mask",
            config.mask_cycle / 12,
            telemetry,
            config,
            |ctx, shared| Box::pin(mask::run(ctx, shared)),
        ),
        leader(
            "lifecycle-deletion",
            config.deletion_every,
            telemetry,
            config,
            |ctx, shared| Box::pin(deletion::run(ctx, shared)),
        ),
    ]
}

type Run = fn(LeaderCtx, Arc<Shared>) -> BoxFuture<'static, JobResult>;

fn leader(
    name: &'static str,
    every: Duration,
    telemetry: &TelemetrySource,
    config: LifecycleConfig,
    run: Run,
) -> Job {
    let telemetry = Arc::clone(telemetry);
    Job::leader(
        name,
        LeaseName::Lifecycle,
        Cadence::every(every),
        move |ctx: LeaderCtx| -> BoxFuture<'static, JobResult> {
            match telemetry() {
                Some(telemetry) => run(ctx, Arc::new(Shared { telemetry, config })),
                None => Box::pin(async { Ok(()) }),
            }
        },
    )
}
