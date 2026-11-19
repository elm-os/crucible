//! The deletion executor (contract 13.3, steps 5 to 8): a leader job under the `lifecycle` lease that runs
//! every 30 s. It first moves the open actions of the ledger ([`crate::actions::advance`]), then each open
//! deletion request:
//!
//! - `pending` becomes `logged` once every live node has recorded its tombstone;
//! - a `logged` request is planned once every live node's checkpoint has passed its tombstone: the traces it
//!   selects are resolved in pages and each page, then the request's own column, becomes delete actions, one
//!   per reach-list table and partition, with `received_at < requested_at`;
//! - it is `applied` when every delete has finished, `verified` when every delete is verified after the mask
//!   cycle, and `completed` once the places outside ClickHouse are accounted for;
//! - it is `failed` when one of its actions was given up.
//!
//! Every write is fenced, and what a request has planned so far is in its `places` document, so a new leader
//! continues where the last one stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crucible_auth::stored_project_scope;
use crucible_control::{
    ActionKind, ActionState, DeletionRequest, DeletionSelector, LifecycleAction, Node, NodeAck,
};
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_plan::{DELETION_TRACES_PAGE, Row, TraceSelector, Value, deletion_traces};
use crucible_records::{DeletionState, ProjectScope, Timestamp, TraceId, WalEpoch};
use serde::{Deserialize, Serialize};

use crate::actions::{Place, Shared, advance, planned};
use crate::detail::Detail;
use crate::partitions::{days_spanned, partitions_of_days};
use crate::reach::{OTHER_PLACES, Reach, final_reach, page_reach};

/// The most pages of traces one run resolves for one request.
const MAX_PAGES_PER_RUN: usize = 20;

/// Where one reach-list table of a request stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PlaceStatus {
    /// Deletes planned for the table, one per partition and page.
    pub actions: u32,
    /// Deletes that have finished: their rows are hidden from every read.
    pub done: u32,
    /// Deletes verified after the mask cycle: their rows are physically gone.
    pub verified: u32,
    /// Deletes that were given up.
    pub failed: u32,
}

/// The `places` document of a deletion request: what the executor has planned so far and where each place
/// stands (contract 11.1: per-place status and counts).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    /// The last trace id of the last planned page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// How many pages were planned.
    #[serde(default)]
    pub pages: u32,
    /// How many traces the request resolved to.
    #[serde(default)]
    pub traces: u64,
    /// The UTC days in which the resolved traces have spans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<u32>,
    /// Whether every action of the request is planned.
    #[serde(default)]
    pub planned: bool,
    /// The ClickHouse tables, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tables: BTreeMap<String, PlaceStatus>,
    /// The places outside ClickHouse, by name: `done`, or `not_present` for a place this deployment does not
    /// have.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub other: BTreeMap<String, String>,
}

impl Progress {
    /// Reads a stored document; an unreadable one is an empty one.
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    /// The stored document.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    fn count(&mut self, actions: &[LifecycleAction]) {
        self.tables.clear();
        for action in actions {
            if action.kind != ActionKind::Delete {
                continue;
            }
            let place = self
                .tables
                .entry(action.table.name().to_owned())
                .or_default();
            place.actions += 1;
            match action.state {
                ActionState::Done | ActionState::Masked => place.done += 1,
                ActionState::Verified => {
                    place.done += 1;
                    place.verified += 1;
                }
                ActionState::Failed => place.failed += 1,
                ActionState::Planned | ActionState::Issued => {}
            }
        }
    }
}

/// Whether every live node satisfies `ok` with its acknowledgement. No live node at all is not enough: the
/// leader itself heartbeats, so an empty list means the registry has not been written yet.
fn every_live_node(live: &[Node], acks: &[NodeAck], ok: impl Fn(&NodeAck) -> bool) -> bool {
    let acked: BTreeMap<WalEpoch, &NodeAck> = acks.iter().map(|ack| (ack.wal_epoch, ack)).collect();
    !live.is_empty()
        && live
            .iter()
            .all(|node| acked.get(&node.wal_epoch).is_some_and(|ack| ok(ack)))
}

/// One resolved trace: its id and the first and last start of its spans.
fn resolved(rows: &[Row]) -> Vec<(TraceId, Timestamp, Timestamp)> {
    rows.iter()
        .filter_map(|row| match row.values() {
            [Value::Bytes(id), Value::Time(first), Value::Time(last)] => {
                let id = <[u8; 16]>::try_from(id.as_slice()).ok()?;
                Some((TraceId::from_bytes(id), *first, *last))
            }
            _ => None,
        })
        .collect()
}

struct Executor<'a> {
    ctx: &'a LeaderCtx,
    shared: &'a Shared,
    live: Vec<Node>,
}

impl Executor<'_> {
    async fn save(&self, request: &DeletionRequest) -> JobResult {
        let row = DeletionRequest {
            updated_at: self.ctx.env.clock.wall_now(),
            ..request.clone()
        };
        self.ctx
            .fenced(move |tx| {
                Box::pin(async move {
                    tx.lifecycle().update_request(&row).await?;
                    Ok(())
                })
            })
            .await
    }

    async fn fail(&self, request: &mut DeletionRequest, why: String) -> JobResult {
        tracing::warn!(
            request = %request.id, project = %request.project_id, why,
            "a deletion request failed and needs an operator; its row and its actions are kept"
        );
        request.state = DeletionState::Failed;
        request.attempts = request.attempts.saturating_add(1);
        request.error = Some(why);
        self.save(request).await
    }

    /// The delete actions of one set of tables over the partitions of `days`.
    fn deletes(
        &self,
        request: &DeletionRequest,
        label: &str,
        reach: &[Reach],
        days: &BTreeSet<u32>,
        traces: &[String],
    ) -> Vec<LifecycleAction> {
        let (user_id, session_id) = match &request.selector {
            DeletionSelector::User { user_id, .. } => (Some(user_id.clone()), None),
            DeletionSelector::Session(session_id) => (None, Some(session_id.clone())),
            DeletionSelector::Traces(_) => (None, None),
        };
        let now = self.ctx.env.clock.wall_now();
        let mut actions = Vec::new();
        for place in reach {
            let detail = Detail::Request {
                requested_at: request.requested_at.micros(),
                traces: if place.traces {
                    traces.to_vec()
                } else {
                    Vec::new()
                },
                user_id: user_id.clone().filter(|_| place.user),
                session_id: session_id.clone().filter(|_| place.session),
            };
            let granularity = self.shared.config.granularity;
            for partition in partitions_of_days(place.table, granularity, days.iter().copied()) {
                actions.push(planned(
                    self.ctx.env.new_id(),
                    now,
                    format!(
                        "deletion:{}:{label}:{}:{partition}",
                        request.id,
                        place.table.name()
                    ),
                    ActionKind::Delete,
                    Place {
                        table: place.table,
                        partition,
                        project_id: Some(request.project_id),
                        request_id: Some(request.id),
                    },
                    &detail,
                ));
            }
        }
        actions
    }

    /// One fenced transaction: the actions and the request's progress together.
    async fn record(
        &self,
        request: &mut DeletionRequest,
        progress: &Progress,
        actions: Vec<LifecycleAction>,
    ) -> JobResult {
        request.places = progress.to_text();
        let row = DeletionRequest {
            updated_at: self.ctx.env.clock.wall_now(),
            ..request.clone()
        };
        self.ctx
            .fenced(move |tx| {
                Box::pin(async move {
                    for action in &actions {
                        tx.lifecycle().plan_action(action).await?;
                    }
                    tx.lifecycle().update_request(&row).await?;
                    Ok(())
                })
            })
            .await
    }

    /// Resolves the request's traces page by page and plans its deletes (contract 13.3, step 5).
    async fn plan(
        &self,
        request: &mut DeletionRequest,
        scope: ProjectScope,
        progress: &mut Progress,
    ) -> JobResult {
        for _ in 0..MAX_PAGES_PER_RUN {
            let selector = match &request.selector {
                DeletionSelector::Traces(ids) => TraceSelector::Traces(ids),
                DeletionSelector::User { user_id, .. } => TraceSelector::User(user_id),
                DeletionSelector::Session(session_id) => TraceSelector::Session(session_id),
            };
            let after = progress
                .cursor
                .as_deref()
                .and_then(|id| TraceId::parse(id).ok());
            let plan = deletion_traces(
                scope,
                selector,
                request.requested_at,
                after,
                DELETION_TRACES_PAGE,
            )
            .map_err(|error| {
                JobError::Failed(format!("the trace resolution was refused: {error}"))
            })?;
            let rows = match self.shared.telemetry.execute(plan).await {
                Ok(stream) => stream.rows().await,
                Err(error) => Err(error),
            }
            .map_err(|error| JobError::Failed(format!("the trace resolution failed: {error}")))?;
            let page = resolved(&rows);
            if let Some((last, _, _)) = page.last() {
                let days: BTreeSet<u32> = page
                    .iter()
                    .flat_map(|(_, first, last)| days_spanned(*first, *last))
                    .collect();
                let traces: Vec<String> = page.iter().map(|(id, _, _)| id.to_string()).collect();
                let label = format!("page{}", progress.pages);
                let actions = self.deletes(
                    request,
                    &label,
                    &page_reach(&request.selector),
                    &days,
                    &traces,
                );
                let mut all: BTreeSet<u32> = progress.days.iter().copied().collect();
                all.extend(days);
                progress.days = all.into_iter().collect();
                progress.cursor = Some(last.to_string());
                progress.pages += 1;
                progress.traces += page.len() as u64;
                self.record(request, progress, actions).await?;
            }
            if page.len() < DELETION_TRACES_PAGE as usize {
                let days: BTreeSet<u32> = progress.days.iter().copied().collect();
                let actions = self.deletes(
                    request,
                    "final",
                    &final_reach(&request.selector),
                    &days,
                    &[],
                );
                progress.planned = true;
                return self.record(request, progress, actions).await;
            }
        }
        Ok(())
    }

    async fn step(&self, mut request: DeletionRequest) -> JobResult {
        let repo = self.ctx.store().lifecycle();
        if request.state == DeletionState::Pending {
            let acks = repo.acks(request.id).await?;
            if !every_live_node(&self.live, &acks, |_| true) {
                return Ok(());
            }
            request.state = DeletionState::Logged;
            self.save(&request).await?;
        }
        let mut progress = Progress::parse(&request.places);
        if request.state == DeletionState::Logged && !progress.planned {
            // Step 5: only after every live node's checkpoint has passed its tombstone. What a node
            // received before the request is in ClickHouse by then, where the deletes find it.
            let acks = repo.acks(request.id).await?;
            let passed = every_live_node(&self.live, &acks, |ack| ack.passed_at.is_some());
            if !passed && !self.shared.config.skips_checkpoint_wait() {
                return Ok(());
            }
            let Some(project) = self.ctx.store().projects().get(request.project_id).await? else {
                return self
                    .fail(&mut request, "the project is gone".to_owned())
                    .await;
            };
            let Ok(scope) = stored_project_scope(&project) else {
                return self
                    .fail(&mut request, "the project is gone".to_owned())
                    .await;
            };
            self.plan(&mut request, scope, &mut progress).await?;
            if !progress.planned {
                return Ok(());
            }
        }
        let actions = repo.request_actions(request.id).await?;
        let before = (request.state, progress.clone());
        progress.count(&actions);
        if let Some(failed) = actions.iter().find(|a| a.state == ActionState::Failed) {
            request.places = progress.to_text();
            let why = format!(
                "the delete on {} partition {} was given up after {} attempts",
                failed.table.name(),
                failed.partition,
                failed.attempts
            );
            return self.fail(&mut request, why).await;
        }
        let all = |ok: fn(ActionState) -> bool| actions.iter().all(|a| ok(a.state));
        if request.state == DeletionState::Logged
            && all(|s| {
                matches!(
                    s,
                    ActionState::Done | ActionState::Masked | ActionState::Verified
                )
            })
        {
            request.state = DeletionState::Applied;
        }
        if request.state == DeletionState::Applied && all(|s| s == ActionState::Verified) {
            request.state = DeletionState::Verified;
        }
        if request.state == DeletionState::Verified {
            // Step 8. Comments, media and export files have no store in this deployment yet: each is
            // reported as not present, so that the request's status says what was and was not reached.
            for place in OTHER_PLACES {
                progress
                    .other
                    .insert(place.to_owned(), "not_present".to_owned());
            }
            request.state = DeletionState::Completed;
        }
        if (request.state, &progress) != (before.0, &before.1) {
            request.places = progress.to_text();
            self.save(&request).await?;
        }
        Ok(())
    }
}

/// One pass of the deletion executor.
pub(crate) async fn run(ctx: LeaderCtx, shared: Arc<Shared>) -> JobResult {
    advance(&ctx, &shared).await?;
    let requests = ctx.store().lifecycle().open_requests().await?;
    if requests.is_empty() {
        return Ok(());
    }
    let live = ctx
        .store()
        .lifecycle()
        .live_nodes(shared.config.live_within)
        .await?;
    let executor = Executor {
        ctx: &ctx,
        shared: &shared,
        live,
    };
    let mut planned_any = false;
    for request in requests {
        let was_logged = request.state != DeletionState::Applied;
        let id = request.id;
        match executor.step(request).await {
            Ok(()) => planned_any |= was_logged,
            Err(JobError::LeaseLost) => return Err(JobError::LeaseLost),
            Err(error) => {
                tracing::debug!(request = %id, %error, "a deletion request could not be moved; it is tried again at the next run");
            }
        }
    }
    // What was planned just now is issued in the same run, so that a deletion hides its rows from the SQL
    // path within one cadence of its last checkpoint (contract 13.4).
    if planned_any {
        advance(&ctx, &shared).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crucible_records::{DeletionRequestId, Lsn};

    use super::*;

    fn node(n: u8) -> Node {
        Node {
            wal_epoch: WalEpoch::from_bytes([n; 16]),
            heartbeat_at: Timestamp::EPOCH,
            version: String::new(),
            checkpoint_lsn: Lsn::ZERO,
            address: String::new(),
        }
    }

    fn ack(n: u8, passed: bool) -> NodeAck {
        NodeAck {
            request_id: DeletionRequestId::from_bytes([9; 16]),
            wal_epoch: WalEpoch::from_bytes([n; 16]),
            tombstone_lsn: Lsn::new(5),
            passed_at: passed.then_some(Timestamp::EPOCH),
        }
    }

    #[test]
    fn a_request_waits_for_every_live_node_and_for_no_other() {
        let live = [node(1), node(2)];
        let passed = |ack: &NodeAck| ack.passed_at.is_some();
        assert!(!every_live_node(&[], &[ack(1, true)], passed));
        assert!(!every_live_node(&live, &[ack(1, true)], passed));
        assert!(!every_live_node(
            &live,
            &[ack(1, true), ack(2, false)],
            passed
        ));
        assert!(every_live_node(
            &live,
            &[ack(1, true), ack(2, false)],
            |_| true
        ));
        // Node 3 is down: its acknowledgement is neither needed nor in the way.
        assert!(every_live_node(
            &live,
            &[ack(1, true), ack(2, true), ack(3, false)],
            passed
        ));
    }

    #[test]
    fn progress_round_trips_and_counts_deletes_by_table() {
        assert_eq!(Progress::parse("{}"), Progress::default());
        assert_eq!(Progress::parse("not json"), Progress::default());
        assert_eq!(
            Progress::default().to_text(),
            r#"{"pages":0,"traces":0,"planned":false}"#
        );
        let mut progress = Progress {
            cursor: Some("ab".to_owned()),
            pages: 2,
            traces: 1_500,
            days: vec![20_000, 20_001],
            planned: true,
            ..Progress::default()
        };
        assert_eq!(Progress::parse(&progress.to_text()), progress);
        let action = |table, state| LifecycleAction {
            id: crucible_records::Uuid::from_bytes([1; 16]),
            key: String::new(),
            kind: ActionKind::Delete,
            table,
            partition: crucible_records::PartitionId::UNPARTITIONED,
            project_id: None,
            request_id: None,
            detail: String::new(),
            state,
            fencing_token: 0,
            mutation: None,
            attempts: 0,
            created_at: Timestamp::EPOCH,
            updated_at: Timestamp::EPOCH,
        };
        use crucible_records::TableId::{SpanCore, TraceHeads};
        progress.count(&[
            action(SpanCore, ActionState::Planned),
            action(SpanCore, ActionState::Masked),
            action(SpanCore, ActionState::Verified),
            action(TraceHeads, ActionState::Failed),
        ]);
        assert_eq!(
            progress.tables["span_core"],
            PlaceStatus {
                actions: 3,
                done: 2,
                verified: 1,
                failed: 0
            }
        );
        assert_eq!(progress.tables["trace_heads"].failed, 1);
    }

    #[test]
    fn resolved_rows_are_read_and_anything_else_is_skipped() {
        let row = |id: u8| {
            Row::new(vec![
                Value::Bytes(vec![id; 16]),
                Value::Time(Timestamp::from_micros(10)),
                Value::Time(Timestamp::from_micros(20)),
            ])
        };
        let rows = [row(1), Row::new(vec![Value::UInt(1)]), row(2)];
        let page = resolved(&rows);
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].0, TraceId::from_bytes([2; 16]));
    }
}
