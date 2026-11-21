//! The scheduler of evaluations (contract 11.7): a timer on every node that writes the row of each rule's
//! latest due tick into the queue `alert_evaluations`.
//!
//! - The row's deduplication key holds the rule, its interval and the tick, so the nodes of a deployment
//!   write one row per tick between them: the others get a unique violation, which is the expected answer.
//! - Ticks are told by the database clock, so nodes whose wall clocks differ schedule the same ticks.
//! - Only the latest due tick is written. After downtime the ticks in between are never evaluated; the tick
//!   that is counts them as skipped.
//! - A rule of a deleted project gets no tick (contract 13.4).
//! - Each pass also deletes the done rows of both queues that nothing can repeat any more.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crucible_control::{Queue, TxError};
use crucible_jobs::{Cadence, Job, JobResult, NodeCtx};
use crucible_records::{AlertRuleId, ProjectId};

use crate::evaluate::evaluation_job;
use crate::schedule::current_seq;

/// How often a node looks for due ticks.
pub const SCAN_EVERY: Duration = Duration::from_secs(10);
/// How long a done evaluation row is kept. Its key cannot repeat once the rule's last tick has passed it.
pub const KEEP_EVALUATIONS: Duration = Duration::from_secs(3_600);
/// How long a done outbox row is kept, as a record of what was sent.
pub const KEEP_NOTIFICATIONS: Duration = Duration::from_secs(86_400);

/// The ticks this node has already written, so that a pass writes each tick once instead of once per scan.
type Written = Arc<Mutex<BTreeMap<AlertRuleId, (u32, u64)>>>;

async fn scan(ctx: NodeCtx, written: Written) -> JobResult {
    let Some(store) = ctx.control else {
        return Ok(());
    };
    let now = store.db_now().await?;
    let rules = store.alert_rules().list_enabled().await?;
    // A deleted project's rules get no tick from the moment of deletion; its purge removes them later.
    let projects: BTreeSet<ProjectId> = store
        .projects()
        .list()
        .await?
        .into_iter()
        .map(|project| project.id)
        .collect();
    let mut live = BTreeMap::new();
    for rule in rules
        .iter()
        .filter(|rule| projects.contains(&rule.project_id))
    {
        let Some(seq) = current_seq(rule.id, rule.interval_secs, now) else {
            continue;
        };
        let tick = (rule.interval_secs, seq);
        live.insert(rule.id, tick);
        let known = written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&rule.id)
            .copied();
        if known == Some(tick) || rule.last_eval_seq.is_some_and(|last| last >= seq) {
            continue;
        }
        let job = evaluation_job(ctx.env.new_id(), rule.id, rule.interval_secs, seq);
        match store.claims().enqueue(Queue::AlertEvaluations, &job).await {
            // Another node, or this one before a restart, wrote the tick.
            Ok(()) | Err(TxError::UniqueViolation(_)) => {
                written
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(rule.id, tick);
            }
            Err(error) => return Err(error.into()),
        }
    }
    // Forget the rules that are gone or off, so the map does not grow.
    written
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|rule, tick| live.get(rule) == Some(tick));
    let claims = store.claims();
    let evaluations = claims
        .prune_done(Queue::AlertEvaluations, KEEP_EVALUATIONS)
        .await?;
    let notifications = claims
        .prune_done(Queue::NotificationOutbox, KEEP_NOTIFICATIONS)
        .await?;
    if evaluations + notifications > 0 {
        tracing::debug!(evaluations, notifications, "pruned done queue rows");
    }
    Ok(())
}

/// The scheduler as a job of the registry.
pub fn job() -> Job {
    let written = Written::default();
    Job::per_node("alert-scheduler", Cadence::every(SCAN_EVERY), move |ctx| {
        scan(ctx, Arc::clone(&written))
    })
}
