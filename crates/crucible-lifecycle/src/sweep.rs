//! The purge sweep (contract 13.4): a leader job under the `lifecycle` lease that looks again for rows of every
//! deleted project and purges what it finds.
//!
//! A deleted project's rows can reach the store after its purge: a node takes a frame of the project in the moment
//! before its key snapshot has the revocation and ships it late, a node cut off from Postgres keeps taking the
//! project's keys under `auth.pg_outage_mode = fail_open`, a node that stopped heartbeating ships what it took before
//! the deletion after the executor stopped waiting for it, and a node that was down for weeks ships its WAL when it
//! returns. Stage B keeps the first two kinds, which arrived after the deletion's point in time.
//!
//! Every `lifecycle.sweep_every` (an hour) the sweep looks at each project deleted in the last [`SWEEP_RECENT`], and
//! every [`SWEEP_OLD_PERIODS`] of those periods (a day) at the projects deleted before. For each table of a project,
//! one probe asks whether the table holds a row of it; in a table that does, each partition is probed
//! ([`find`]). Each partition with rows gets a purge, the action a project deletion plans, unless one that has not
//! run yet is there already. The deletion executor sends it at its next run, as the delete of the project's rows or
//! as the drop of a partition that no project can write to, and verifies it. A request that has completed stays
//! completed and counts these late purges, with the time of the last. A wake sends at most
//! [`MAX_PROBES_PER_RUN`] probes and records how far its pass got, so a pass over many deleted projects is spread
//! over the wakes of its period.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crucible_control::{
    ActionKind, ActionState, DeletionRequest, LifecycleAction, duration_micros,
};
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_records::{DeletionState, PartitionId, TableId, Timestamp};

use crate::actions::{self, Place, Shared, planned};
use crate::deletion::{MAX_PROBES_PER_RUN, Progress, find};
use crate::detail::Detail;
use crate::reach::project_reach;
use crate::retention::Schedule;

/// How long after its deletion a project is looked at every `lifecycle.sweep_every`. Late rows come from nodes that
/// were behind, cut off or down, and most catch up within days: a month of hourly looks keeps such a row for at most
/// about an hour in that time.
pub const SWEEP_RECENT: Duration = Duration::from_secs(30 * 86_400);

/// After [`SWEEP_RECENT`], a deleted project is looked at once every this many periods of `lifecycle.sweep_every`:
/// a day. A node that returns after weeks still has its late rows removed within a day, for about one probe per table
/// of each deleted project a day.
pub const SWEEP_OLD_PERIODS: u64 = 24;

/// The deleted projects one pass looks at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pass {
    /// Those deleted within [`SWEEP_RECENT`], every period.
    Recent,
    /// Those deleted before, every [`SWEEP_OLD_PERIODS`] periods.
    Old,
}

/// One wake of the purge sweep.
pub(crate) async fn run(ctx: LeaderCtx, shared: Arc<Shared>) -> JobResult {
    let config = &shared.config;
    if config.skips_sweep() {
        return Ok(());
    }
    let db_now = ctx.store().db_now().await?;
    let period = Schedule::period(db_now, config.sweep_every);
    let schedule = Schedule::load(&ctx).await?;
    let due = |pass| match pass {
        Pass::Recent => schedule.sweep < period,
        Pass::Old => schedule.sweep_old.saturating_add(SWEEP_OLD_PERIODS) <= period,
    };
    if !due(Pass::Recent) && !due(Pass::Old) {
        return Ok(());
    }
    let recent_from = db_now.saturating_add_micros(-duration_micros(SWEEP_RECENT));
    let requests = ctx.store().lifecycle().project_requests().await?;
    let mut probes = MAX_PROBES_PER_RUN;
    for pass in [Pass::Recent, Pass::Old] {
        if !due(pass) {
            continue;
        }
        let after = match pass {
            Pass::Recent => schedule.sweep_after,
            Pass::Old => schedule.sweep_old_after,
        };
        let mut last = after;
        let mut whole = true;
        let of_pass = requests.iter().filter(|request| {
            request.deletion_seq > after
                && (request.requested_at >= recent_from) == (pass == Pass::Recent)
        });
        for request in of_pass {
            if probes == 0 {
                whole = false;
                break;
            }
            match sweep(&ctx, &shared, request, period, &mut probes).await {
                Ok(()) => last = request.deletion_seq,
                Err(JobError::LeaseLost) => return Err(JobError::LeaseLost),
                Err(error) => {
                    tracing::warn!(
                        request = %request.id, project = %request.project_id, %error,
                        "the purge sweep could not look at a deleted project; its next wake looks again"
                    );
                    whole = false;
                    break;
                }
            }
        }
        // Each job of the lease writes its own fields of the shared cursor, read just before.
        let mut stored = Schedule::load(&ctx).await?;
        match (pass, whole) {
            (Pass::Recent, true) => (stored.sweep, stored.sweep_after) = (period, 0),
            (Pass::Recent, false) => stored.sweep_after = last,
            (Pass::Old, true) => (stored.sweep_old, stored.sweep_old_after) = (period, 0),
            (Pass::Old, false) => stored.sweep_old_after = last,
        }
        stored.store(&ctx).await?;
        if !whole {
            return Ok(());
        }
    }
    Ok(())
}

/// Looks for rows of the project of one project deletion and plans a purge of each partition that holds some. A
/// request that is not planned yet is left to the executor, which finds the project's rows itself, and a project
/// that is not deleted is never swept.
async fn sweep(
    ctx: &LeaderCtx,
    shared: &Shared,
    request: &DeletionRequest,
    period: u64,
    probes: &mut usize,
) -> JobResult {
    let progress = Progress::parse(&request.places);
    let planning = request.state == DeletionState::Pending
        || (request.state == DeletionState::Logged && !progress.planned);
    if planning {
        return Ok(());
    }
    let project = request.project_id;
    // Only a deleted project is swept. Deleting a project makes its request, and nothing clears a deletion.
    let deleted = ctx
        .store()
        .projects()
        .get(project)
        .await?
        .is_some_and(|row| row.deleted_at.is_some());
    if !deleted {
        return Ok(());
    }
    let mut found: Vec<(TableId, PartitionId)> = Vec::new();
    for table in project_reach() {
        // The tables of one project are looked at in one wake.
        let mut left = usize::MAX;
        let held = find(shared, table, project, None, &mut left)
            .await
            .map_err(|error| {
                JobError::Failed(format!("the probes of {} failed: {error}", table.name()))
            })?;
        *probes = probes.saturating_sub(usize::MAX - left);
        found.extend(
            held.partitions
                .into_iter()
                .map(|partition| (table, partition)),
        );
    }
    if found.is_empty() {
        return Ok(());
    }
    // A purge of the project that has not run yet removes these rows when it runs.
    let waiting: BTreeSet<(TableId, PartitionId)> = ctx
        .store()
        .lifecycle()
        .request_actions(request.id)
        .await?
        .into_iter()
        .filter(|action| {
            action.state == ActionState::Planned
                || (action.state == ActionState::Issued && action.mutation.is_none())
        })
        .map(|action| (action.table, action.partition))
        .collect();
    found.retain(|pair| !waiting.contains(pair));
    let now = ctx.env.clock.wall_now();
    let purges: Vec<LifecycleAction> = found
        .iter()
        .map(|(table, partition)| purge(ctx, shared, request, (*table, *partition), period, now))
        .collect();
    let planned = actions::plan(ctx, purges).await?;
    let late = matches!(
        request.state,
        DeletionState::Completed | DeletionState::Failed
    );
    if planned == 0 || !late {
        return Ok(());
    }
    let mut progress = progress;
    progress.late = progress
        .late
        .saturating_add(u32::try_from(planned).unwrap_or(u32::MAX));
    progress.late_at = Some(now.micros());
    let row = DeletionRequest {
        places: progress.to_text(),
        updated_at: now,
        ..request.clone()
    };
    ctx.fenced(move |tx| {
        Box::pin(async move {
            tx.lifecycle().update_request(&row).await?;
            Ok(())
        })
    })
    .await?;
    tracing::info!(
        request = %request.id, project = %project, purges = planned,
        "the purge sweep found rows of a deleted project that landed after its purge, and plans their purge"
    );
    Ok(())
}

/// The purge of one partition that the sweep found rows of the project in: keyed by the sweep's period, so a
/// partition that holds rows again in a later period gets a purge of its own.
fn purge(
    ctx: &LeaderCtx,
    shared: &Shared,
    request: &DeletionRequest,
    (table, partition): (TableId, PartitionId),
    period: u64,
    now: Timestamp,
) -> LifecycleAction {
    let (kind, detail, project_id) = if shared.config.sweeps_without_project() {
        (ActionKind::Drop, Detail::Drop, None)
    } else {
        (ActionKind::Delete, Detail::Purge, Some(request.project_id))
    };
    planned(
        ctx.env.new_id(),
        now,
        format!("sweep:{}:{}:{partition}:{period}", request.id, table.name()),
        kind,
        Place {
            table,
            partition,
            project_id,
            request_id: Some(request.id),
        },
        &detail,
    )
}
