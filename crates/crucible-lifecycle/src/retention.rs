//! The retention executor (contract 13.2): a leader job under the `lifecycle` lease that, once per hour, finds
//! the partitions that hold expired rows and plans what removes them.
//!
//! For each project with a retention setting and each partitioned table, one typed read finds the oldest row
//! before the table's cut-off. The partitions from there to the cut-off are candidates. For each candidate the
//! store lists the projects with rows in it, and [`decide`] chooses: drop the partition when every one of
//! them has expired there, otherwise delete the rows of each expired project, with the time predicate that a
//! partition straddling the cut-off needs. The plan is a set of action rows keyed by project, table, partition
//! and cut-off day, so a rerun plans nothing twice; the actions are issued by [`crate::actions::advance`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crucible_auth::stored_project_scope;
use crucible_control::{ActionKind, Project, RetentionFloor, duration_micros};
use crucible_env::QueryError;
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_plan::{Pred, RetentionClamp, RetentionFloors, Value, lifecycle_oldest};
use crucible_records::{PartitionId, ProjectId, TableId, TimeWindow, Timestamp};
use crucible_schema::RetentionSettings;
use serde::{Deserialize, Serialize};

use crate::actions::{Place, Shared, plan, planned};
use crate::detail::Detail;
use crate::partitions::{day_of, partition_window, partitions_between};
use crate::reach::retention_tables;

/// The most partitions of one table and project that one run plans. A project that has just shortened a
/// long retention is worked off over several hours.
const MAX_PARTITIONS_PER_RUN: usize = 64;
/// How long a verified retention action is kept before it is pruned.
const KEEP_VERIFIED: Duration = Duration::from_secs(7 * 86_400);

/// What the jobs of the `lifecycle` lease remember between runs: they share the lease's one cursor (contract
/// 11.7). A new leader runs every job once when it takes the lease, so "hourly" and "once per cycle" are
/// decided from this row, not from the cadence.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Schedule {
    /// The retention period (the database clock divided by `retention_every`) that was last planned whole.
    #[serde(default)]
    pub(crate) retention: u64,
    /// The mask cycle (the database clock divided by `lifecycle.mask_cycle`) that was last planned.
    #[serde(default)]
    pub(crate) mask: u64,
    /// The sweep period (the database clock divided by `lifecycle.sweep_every`) whose pass over the projects deleted
    /// recently is complete.
    #[serde(default)]
    pub(crate) sweep: u64,
    /// The `deletion_seq` of the last request that the recent pass in progress has looked at; 0 between passes.
    #[serde(default)]
    pub(crate) sweep_after: u64,
    /// The sweep period whose pass over the projects deleted earlier is complete.
    #[serde(default)]
    pub(crate) sweep_old: u64,
    /// The `deletion_seq` of the last request that the earlier pass in progress has looked at; 0 between passes.
    #[serde(default)]
    pub(crate) sweep_old_after: u64,
}

impl Schedule {
    pub(crate) async fn load(ctx: &LeaderCtx) -> Result<Self, JobError> {
        Ok(ctx
            .cursor()
            .await?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default())
    }

    pub(crate) async fn store(self, ctx: &LeaderCtx) -> JobResult {
        let text = serde_json::to_string(&self).unwrap_or_default();
        ctx.set_cursor(&text).await
    }

    /// Which period of length `every` the instant `now` is in.
    pub(crate) fn period(now: Timestamp, every: Duration) -> u64 {
        let every = duration_micros(every).max(1);
        u64::try_from(now.micros() / every).unwrap_or(0)
    }
}

/// The floors of a project as the plan builders take them.
pub(crate) fn floors(floor: RetentionFloor) -> RetentionFloors {
    RetentionFloors {
        retention: floor.retention,
        payload: floor.payload,
        mcp_content: floor.mcp_content,
    }
}

/// The retention settings of a project row.
pub(crate) fn settings(project: &Project) -> RetentionSettings {
    RetentionSettings {
        retention_days: project.retention_days,
        payload_retention_days: project.payload_retention_days,
        mcp_content_retention_days: project.mcp_content_retention_days,
    }
}

/// What the executor does with one partition (contract 13.2, steps 2 and 3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// No project with rows in the partition has expired rows there.
    Keep,
    /// Every project with rows in the partition has expired for the whole of it: drop the partition.
    Drop,
    /// Delete the rows before each project's cut-off. The projects are those with rows in the partition whose
    /// cut-off lies inside or after it.
    Delete(Vec<(ProjectId, Timestamp)>),
}

/// Decides one partition. `window` is the time the partition covers, `live` the projects with visible rows in
/// it, and `cutoffs` the cut-off of every project that has one for the table. A project that is not in
/// `cutoffs` keeps its data forever, so its presence always forbids the drop.
pub fn decide(
    window: TimeWindow,
    live: &[ProjectId],
    cutoffs: &BTreeMap<ProjectId, Timestamp>,
) -> Decision {
    let mut expired = Vec::new();
    let mut whole = !live.is_empty();
    for project in live {
        match cutoffs.get(project) {
            Some(cutoff) if *cutoff > window.from() => {
                whole &= *cutoff >= window.to();
                expired.push((*project, *cutoff));
            }
            _ => whole = false,
        }
    }
    if whole {
        Decision::Drop
    } else if expired.is_empty() {
        Decision::Keep
    } else {
        Decision::Delete(expired)
    }
}

/// The key of a retention delete: one per project, table, partition and cut-off day (contract 13.2).
fn delete_key(
    project: ProjectId,
    table: TableId,
    partition: PartitionId,
    cutoff: Timestamp,
) -> String {
    format!(
        "retention:{project}:{}:{partition}:{}",
        table.name(),
        day_of(cutoff).unwrap_or(0)
    )
}

/// The key of a partition drop: one per table, partition and retention period, so that a partition which
/// holds expired rows again is dropped again.
fn drop_key(table: TableId, partition: PartitionId, period: u64) -> String {
    format!("drop:{}:{partition}:{period}", table.name())
}

/// The oldest instant of a table's time column in the answer of [`lifecycle_oldest`].
pub(crate) fn oldest(rows: &[crucible_plan::Row]) -> Option<Timestamp> {
    match rows.first()?.values().first()? {
        Value::Time(t) => Some(*t),
        Value::Date(date) => Some(date.start()),
        _ => None,
    }
}

type Candidates = BTreeMap<(TableId, PartitionId), BTreeMap<ProjectId, Timestamp>>;

/// One pass of the retention executor.
pub(crate) async fn run(ctx: LeaderCtx, shared: Arc<Shared>) -> JobResult {
    let config = &shared.config;
    let db_now = ctx.store().db_now().await?;
    let period = Schedule::period(db_now, config.retention_every);
    let mut schedule = Schedule::load(&ctx).await?;
    if schedule.retention >= period {
        return Ok(());
    }
    // Cut-offs are taken behind the database clock, so that no row is removed that a node with a slower
    // clock still serves.
    let now = db_now.saturating_add_micros(-duration_micros(config.clock_margin));
    let projects = ctx.store().projects().list().await?;
    let stored: BTreeMap<ProjectId, RetentionFloor> = ctx
        .store()
        .lifecycle()
        .floors()
        .await?
        .into_iter()
        .collect();

    let mut candidates = Candidates::new();
    let mut whole = true;
    for project in &projects {
        let Ok(scope) = stored_project_scope(project) else {
            continue;
        };
        let floor = stored.get(&project.id).copied().unwrap_or_default();
        let clamp = RetentionClamp::new(settings(project), now).with_floors(floors(floor));
        for table in retention_tables() {
            let Ok(Some(cutoff)) = clamp.cutoff(table) else {
                continue;
            };
            let Ok(window) = TimeWindow::new(Timestamp::EPOCH, cutoff) else {
                continue;
            };
            let Ok(plan) = lifecycle_oldest(scope, table, window, Pred::all()) else {
                continue;
            };
            let rows = match shared.telemetry.execute(plan).await {
                Ok(stream) => stream.rows().await,
                Err(error) => Err(error),
            };
            let rows = match rows {
                Ok(rows) => rows,
                Err(error) => {
                    log_read_failure(project.id, table, &error);
                    whole = false;
                    continue;
                }
            };
            let Some(first) = oldest(&rows) else {
                continue;
            };
            let (partitions, cut) = partitions_between(
                table,
                config.granularity,
                first,
                cutoff,
                MAX_PARTITIONS_PER_RUN,
            );
            whole &= !cut;
            for partition in partitions {
                candidates
                    .entry((table, partition))
                    .or_default()
                    .insert(project.id, cutoff);
            }
        }
    }

    let known: BTreeSet<ProjectId> = projects.iter().map(|project| project.id).collect();
    let mut actions = Vec::new();
    for ((table, partition), cutoffs) in &candidates {
        let Some(window) = partition_window(*table, config.granularity, *partition) else {
            continue;
        };
        let live = match shared
            .telemetry
            .partition_projects(*table, *partition)
            .await
        {
            Ok(live) => live,
            Err(error) => {
                tracing::debug!(table = table.name(), %partition, %error, "the retention executor could not list a partition's projects");
                whole = false;
                continue;
            }
        };
        let place = |project_id| Place {
            table: *table,
            partition: *partition,
            project_id,
            request_id: None,
        };
        // A project the control plane does not list (it was deleted, or its row is yet to be read) is
        // never taken for expired.
        match decide(window, &live, cutoffs) {
            Decision::Keep => {}
            Decision::Drop if live.iter().all(|project| known.contains(project)) => {
                actions.push(planned(
                    ctx.env.new_id(),
                    db_now,
                    drop_key(*table, *partition, period),
                    ActionKind::Drop,
                    place(None),
                    &Detail::Drop,
                ));
            }
            Decision::Drop => {}
            Decision::Delete(expired) => {
                for (project, cutoff) in expired {
                    actions.push(planned(
                        ctx.env.new_id(),
                        db_now,
                        delete_key(project, *table, *partition, cutoff),
                        ActionKind::Delete,
                        place(Some(project)),
                        &Detail::Retention {
                            cutoff: cutoff.micros(),
                        },
                    ));
                }
            }
        }
    }
    let planned = plan(&ctx, actions).await?;
    let before = db_now.saturating_add_micros(-duration_micros(KEEP_VERIFIED));
    let pruned = ctx
        .fenced(move |tx| Box::pin(async move { tx.lifecycle().prune_actions(before).await }))
        .await?;
    if planned + usize::try_from(pruned).unwrap_or(0) > 0 {
        tracing::info!(
            planned,
            pruned,
            "the retention executor planned its actions"
        );
    }
    // A pass that could not read everything, or that cut a list, runs again at the next cadence.
    if whole {
        schedule = Schedule::load(&ctx).await?;
        schedule.retention = period;
        schedule.store(&ctx).await?;
    }
    Ok(())
}

fn log_read_failure(project: ProjectId, table: TableId, error: &QueryError) {
    tracing::debug!(%project, table = table.name(), %error, "the retention executor could not read a table");
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const DAY: i64 = 86_400_000_000;

    fn at(day: i64) -> Timestamp {
        Timestamp::from_micros(day * DAY)
    }

    fn project(n: u8) -> ProjectId {
        ProjectId::from_bytes([n; 16])
    }

    #[test]
    fn a_partition_is_dropped_only_when_every_project_in_it_has_expired() {
        let window = TimeWindow::new(at(10), at(11)).unwrap();
        let cutoffs = BTreeMap::from([(project(1), at(11)), (project(2), at(20))]);
        assert_eq!(
            decide(window, &[project(1), project(2)], &cutoffs),
            Decision::Drop
        );
        assert_eq!(decide(window, &[], &cutoffs), Decision::Keep);
        // Project 3 keeps its data forever: the others are deleted row by row.
        assert_eq!(
            decide(window, &[project(1), project(3)], &cutoffs),
            Decision::Delete(vec![(project(1), at(11))])
        );
        // A month that straddles a cut-off is never dropped.
        let month = TimeWindow::new(at(0), at(30)).unwrap();
        assert_eq!(
            decide(month, &[project(1)], &cutoffs),
            Decision::Delete(vec![(project(1), at(11))])
        );
        // A cut-off at the partition's start expires nothing in it.
        let later = TimeWindow::new(at(11), at(12)).unwrap();
        assert_eq!(decide(later, &[project(1)], &cutoffs), Decision::Keep);
    }

    #[test]
    fn the_schedule_reads_any_cursor_and_counts_periods() {
        assert_eq!(
            serde_json::from_str::<Schedule>("{}").unwrap(),
            Schedule::default()
        );
        let schedule: Schedule = serde_json::from_str(r#"{"retention":7,"mask":2}"#).unwrap();
        assert_eq!((schedule.retention, schedule.mask), (7, 2));
        let hour = Duration::from_secs(3_600);
        assert_eq!(
            Schedule::period(Timestamp::from_micros(3_599_999_999), hour),
            0
        );
        assert_eq!(
            Schedule::period(Timestamp::from_micros(3_600_000_000), hour),
            1
        );
    }

    proptest! {
        /// The decision never removes a row that is not expired: a drop needs every live project expired for
        /// the whole partition, and a delete names only projects whose cut-off lies above the partition's
        /// start, each with its own cut-off.
        #[test]
        fn a_decision_never_reaches_rows_that_are_kept(
            from in 0i64..100, len in 1i64..40,
            live in prop::collection::btree_set(0u8..6, 0..6),
            cutoffs in prop::collection::btree_map(0u8..6, 0i64..160, 0..6),
        ) {
            let window = TimeWindow::new(at(from), at(from + len)).unwrap();
            let live: Vec<ProjectId> = live.into_iter().map(project).collect();
            let cutoffs: BTreeMap<ProjectId, Timestamp> =
                cutoffs.into_iter().map(|(p, day)| (project(p), at(day))).collect();
            match decide(window, &live, &cutoffs) {
                Decision::Drop => {
                    prop_assert!(!live.is_empty());
                    for p in &live {
                        prop_assert!(cutoffs.get(p).is_some_and(|c| *c >= window.to()));
                    }
                }
                Decision::Delete(expired) => {
                    for (p, cutoff) in &expired {
                        prop_assert!(live.contains(p));
                        prop_assert_eq!(cutoffs.get(p), Some(cutoff));
                        prop_assert!(*cutoff > window.from());
                    }
                    // Everything expired is named, and something is kept.
                    let named: BTreeSet<ProjectId> = expired.iter().map(|(p, _)| *p).collect();
                    for p in &live {
                        let has_expired = cutoffs.get(p).is_some_and(|c| *c > window.from());
                        prop_assert_eq!(named.contains(p), has_expired);
                    }
                    let all_whole = live
                        .iter()
                        .all(|p| cutoffs.get(p).is_some_and(|c| *c >= window.to()));
                    prop_assert!(!all_whole);
                }
                Decision::Keep => {
                    for p in &live {
                        prop_assert!(!cutoffs.get(p).is_some_and(|c| *c > window.from()));
                    }
                }
            }
        }
    }
}
