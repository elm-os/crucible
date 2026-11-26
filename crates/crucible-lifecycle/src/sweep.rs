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
//! as the drop of a partition that no project can write to, and verifies it. While a project deletion still finds
//! where the project's rows are, the sweep looks at the tables it has passed. A request that has completed stays
//! completed and counts these late purges, with the time of the last. A request that failed is finished here once no
//! table holds a row of its project: its objects and control-plane rows go, and it completes.
//!
//! A project that cannot be looked at now, because a probe fails or the object store refuses, is recorded on its
//! request with one more failed attempt, and the pass goes on with the next project; the next pass looks at it again.
//! A wake sends at most [`MAX_PROBES_PER_RUN`] probes and records how far its pass got, so a pass over many deleted
//! projects is spread over the wakes of its period. A pass counts for the period it began in, so one that runs past
//! the end of that period does not delay the next. A project with no late row costs one probe per table, 17, so at a
//! wake every 30 s an hourly pass covers about 7,000 deleted projects.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crucible_control::{
    ActionKind, ActionState, DeletionRequest, LifecycleAction, duration_micros,
};
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_records::{DeletionState, PartitionId, TableId, Timestamp};

use crate::actions::{self, Place, Shared, planned};
use crate::config::LifecycleConfig;
use crate::deletion::{MAX_PROBES_PER_RUN, Progress, find, finish};
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

/// The start of the error the sweep records when its probes of a project fail. A later look whose probes answer
/// clears an error that starts with it.
const LOOK_FAILED: &str = "looking again for rows of the deleted project";

/// The deleted projects one pass looks at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pass {
    /// Those deleted within [`SWEEP_RECENT`], every period.
    Recent,
    /// Those deleted before, every [`SWEEP_OLD_PERIODS`] periods.
    Old,
}

impl Pass {
    /// How often the pass looks at each of its projects.
    fn every(self, config: &LifecycleConfig) -> Duration {
        match self {
            Self::Recent => config.sweep_every,
            Self::Old => config
                .sweep_every
                .saturating_mul(u32::try_from(SWEEP_OLD_PERIODS).unwrap_or(u32::MAX)),
        }
    }
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
                Ok(()) => {}
                Err(JobError::LeaseLost) => return Err(JobError::LeaseLost),
                // The request says why and the next pass looks again; the other projects are not held back.
                Err(error) => record(&ctx, request, &error, pass.every(config)).await?,
            }
            last = request.deletion_seq;
        }
        // Each job of the lease writes its own fields of the shared cursor, read just before.
        advanced(Schedule::load(&ctx).await?, pass, period, whole, last)
            .store(&ctx)
            .await?;
    }
    Ok(())
}

/// The cursor after a wake in the period `period` that took `pass` as far as the request with the sequence number
/// `last`; `whole` when the pass is complete. A pass begins in the wake that finds none in progress, and a complete
/// pass counts for the period it began in, so a pass that runs past the end of that period does not delay the next.
fn advanced(mut stored: Schedule, pass: Pass, period: u64, whole: bool, last: u64) -> Schedule {
    let (done, after, began) = match pass {
        Pass::Recent => (
            &mut stored.sweep,
            &mut stored.sweep_after,
            &mut stored.sweep_began,
        ),
        Pass::Old => (
            &mut stored.sweep_old,
            &mut stored.sweep_old_after,
            &mut stored.sweep_old_began,
        ),
    };
    if *after == 0 {
        *began = period;
    }
    if whole {
        (*done, *after) = (*began, 0);
    } else {
        *after = last;
    }
    stored
}

/// A period as an error says it: in hours, or in minutes when it is not whole hours.
fn span(every: Duration) -> String {
    let minutes = every.as_secs() / 60;
    if minutes >= 60 && minutes % 60 == 0 {
        format!("{} h", minutes / 60)
    } else {
        format!("{minutes} min")
    }
}

/// Records on a project deletion that the sweep could not look at its project, or not finish it: one more failed
/// attempt and the error, which the list of deletion requests shows. The next pass looks again, within `every`.
async fn record(
    ctx: &LeaderCtx,
    request: &DeletionRequest,
    error: &JobError,
    every: Duration,
) -> JobResult {
    let attempts = request.attempts.saturating_add(1);
    tracing::warn!(
        request = %request.id, project = %request.project_id, %error, attempts,
        "the purge sweep could not look at a deleted project; its next pass looks again"
    );
    let row = DeletionRequest {
        attempts,
        error: Some(format!(
            "{error}; the purge sweep looks again at its next pass, within {}",
            span(every)
        )),
        updated_at: ctx.env.clock.wall_now(),
        ..request.clone()
    };
    ctx.fenced(move |tx| {
        Box::pin(async move {
            tx.lifecycle().update_request(&row).await?;
            Ok(())
        })
    })
    .await
}

/// Looks for rows of the project of one project deletion and plans a purge of each partition that holds some. A
/// pending request is left to the executor, which finds the project's rows itself; of a request still finding them,
/// only the tables it has passed are looked at; and a project that is not deleted is never swept. A failed request
/// whose project no table holds is finished: its project's objects and control-plane rows go, and it completes. On an
/// error the stored request is as it was.
async fn sweep(
    ctx: &LeaderCtx,
    shared: &Shared,
    request: &DeletionRequest,
    period: u64,
    probes: &mut usize,
) -> JobResult {
    if request.state == DeletionState::Pending {
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
    let before = Progress::parse(&request.places);
    let mut progress = before.clone();
    // Discovery plans a table's purges before it adds the table to `purged`.
    let discovering = request.state == DeletionState::Logged && !progress.planned;
    let mut found: Vec<(TableId, PartitionId)> = Vec::new();
    for table in project_reach() {
        if discovering && !progress.purged.iter().any(|name| name == table.name()) {
            continue;
        }
        // The tables of one project are looked at in one wake.
        let mut left = usize::MAX;
        let held = find(shared, table, project, None, &mut left).await;
        *probes = probes.saturating_sub(usize::MAX - left);
        let held = held.map_err(|error| {
            JobError::Failed(format!("{LOOK_FAILED} in {} failed ({error})", table.name()))
        })?;
        found.extend(
            held.partitions
                .into_iter()
                .map(|partition| (table, partition)),
        );
    }
    let mut row = request.clone();
    if row
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with(LOOK_FAILED))
    {
        row.error = None;
    }
    let now = ctx.env.clock.wall_now();
    if found.is_empty() {
        if request.state == DeletionState::Failed {
            // The purge gave up an action, and no table holds a row of the project now: what is left is the rest
            // of the request, which nothing else does.
            finish(ctx, shared, request, &mut progress).await?;
            row.state = DeletionState::Completed;
            row.error = None;
            tracing::info!(
                request = %request.id, project = %project,
                "the purge sweep found no row of the project of a failed project deletion, and finished it"
            );
        }
    } else {
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
        let purges: Vec<LifecycleAction> = found
            .iter()
            .map(|(table, partition)| purge(ctx, shared, request, (*table, *partition), period, now))
            .collect();
        let planned = actions::plan(ctx, purges).await?;
        let late = matches!(
            request.state,
            DeletionState::Completed | DeletionState::Failed
        );
        if planned > 0 && late {
            progress.late = progress
                .late
                .saturating_add(u32::try_from(planned).unwrap_or(u32::MAX));
            progress.late_at = Some(now.micros());
            tracing::info!(
                request = %request.id, project = %project, purges = planned,
                "the purge sweep found rows of a deleted project that landed after its purge, and plans their purge"
            );
        }
    }
    if progress != before {
        row.places = progress.to_text();
    }
    if row == *request {
        return Ok(());
    }
    let row = DeletionRequest {
        updated_at: now,
        ..row
    };
    ctx.fenced(move |tx| {
        Box::pin(async move {
            tx.lifecycle().update_request(&row).await?;
            Ok(())
        })
    })
    .await
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass that runs past the end of the period it began in counts for that period, so the next pass begins in
    /// the period after, not one period later.
    #[test]
    fn a_pass_counts_for_the_period_it_began_in() {
        for pass in [Pass::Recent, Pass::Old] {
            let begun = advanced(Schedule::default(), pass, 100, false, 7);
            let done = advanced(begun, pass, 101, true, 9);
            let (period, after) = match pass {
                Pass::Recent => (done.sweep, done.sweep_after),
                Pass::Old => (done.sweep_old, done.sweep_old_after),
            };
            assert_eq!((period, after), (100, 0), "{pass:?}");
        }
    }

    /// A pass that a wake begins and completes counts for that wake's period, and a pass in progress keeps the period
    /// it began in across wakes.
    #[test]
    fn a_pass_begins_in_the_wake_that_finds_none_in_progress() {
        let whole = advanced(Schedule::default(), Pass::Recent, 50, true, 3);
        assert_eq!((whole.sweep, whole.sweep_after), (50, 0));
        let begun = advanced(whole, Pass::Recent, 51, false, 4);
        let on = advanced(begun, Pass::Recent, 52, false, 8);
        assert_eq!((on.sweep, on.sweep_after, on.sweep_began), (50, 8, 51));
        let done = advanced(on, Pass::Recent, 53, true, 9);
        assert_eq!((done.sweep, done.sweep_after), (51, 0));
        // The earlier pass is left as it was.
        assert_eq!((done.sweep_old, done.sweep_old_after), (0, 0));
    }

    #[test]
    fn an_error_names_the_period_within_which_the_sweep_looks_again() {
        assert_eq!(span(Duration::from_secs(3_600)), "1 h");
        assert_eq!(span(Duration::from_secs(24 * 3_600)), "24 h");
        assert_eq!(span(Duration::from_secs(600)), "10 min");
    }
}
