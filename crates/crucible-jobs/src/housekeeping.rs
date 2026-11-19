//! The housekeeping job (contract 11.7, lease `housekeeping`): prunes control-plane rows that nothing reads
//! any more.
//!
//! - `key_feed` rows older than the poll overlap window plus the reconcile interval (contract 11.8): every
//!   node has reloaded the full key snapshot since such a row was written.
//! - `sessions` rows whose absolute expiry has passed.

use std::time::Duration;

use crucible_control::{LeaseName, TRANSACTION_CAP, duration_micros};

use crate::cadence::Cadence;
use crate::job::{Job, JobResult};
use crate::leader::LeaderCtx;

/// The overlap window of a key-feed poll (contract 11.8): the transaction cap plus the poll's own bound.
pub const KEY_FEED_OVERLAP: Duration = TRANSACTION_CAP.saturating_add(Duration::from_secs(5));
/// How often every node reloads the full key snapshot (contract 11.8).
pub const KEY_FEED_RECONCILE_EVERY: Duration = Duration::from_secs(60);
/// How long a key-feed row is kept.
pub const KEY_FEED_KEEP: Duration = KEY_FEED_OVERLAP.saturating_add(KEY_FEED_RECONCILE_EVERY);
/// How long after its absolute expiry a session row is kept. Expiry times are written from node clocks, so
/// the leader allows twice the 5 min of skew between two nodes.
pub const SESSION_GRACE: Duration = Duration::from_secs(600);
/// How often the job runs.
pub const EVERY: Duration = Duration::from_secs(60);

/// The housekeeping job.
pub fn job() -> Job {
    Job::leader(
        "housekeeping",
        LeaseName::Housekeeping,
        Cadence::every(EVERY),
        run,
    )
}

/// One pass. Each table is pruned in its own fenced transaction, so a conflict on one does not hold up the
/// other; every delete is idempotent.
async fn run(ctx: LeaderCtx) -> JobResult {
    let feed = ctx
        .fenced(|tx| Box::pin(async move { tx.key_feed().prune(KEY_FEED_KEEP).await }))
        .await;
    let before = ctx
        .env
        .clock
        .wall_now()
        .saturating_add_micros(-duration_micros(SESSION_GRACE));
    let sessions = ctx
        .fenced(|tx| Box::pin(async move { tx.sessions().delete_expired(before).await }))
        .await;
    let (feed, sessions) = (feed?, sessions?);
    if feed + sessions > 0 {
        tracing::debug!(key_feed = feed, sessions, "housekeeping pruned rows");
    }
    Ok(())
}
