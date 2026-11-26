//! The deletion executor (contract 13.3, steps 5 to 8): a leader job under the `lifecycle` lease that runs
//! every 30 s. It first moves the open actions of the ledger ([`crate::actions::advance`]), then each open
//! deletion request:
//!
//! - `pending` becomes `logged` once every live node has recorded its tombstone;
//! - a `logged` request is planned once every live node's checkpoint has passed its tombstone: the traces it
//!   selects are resolved in pages and each page, then the request's own column, becomes delete actions, one
//!   per reach-list table and partition, with `received_at < requested_at`. A deletion by user also reaches
//!   the MCP tables by `user_id` and `user_subject_hash`, in the partitions where its MCP rows are found: an
//!   operation can carry the user, or only the hash of the user's subject, when no span of its trace names
//!   the user. While the project's salt cannot be read, such a request waits and says why;
//! - a logged project deletion is planned a table at a time instead: every table of the project, one purge per
//!   partition that holds the project's rows, whatever their arrival (the project takes no new data once
//!   deleted). Each time a purge is sent it is decided again whether it drops the partition or deletes the
//!   project's rows from it: it drops only when the partition ends before every project, live or made later, can
//!   write ([`write_horizon`]), every live node's clock is close to the database clock ([`clocks_agree`]) and every
//!   project with rows there is deleted ([`Purges::kind`]);
//! - a deleted project without a project deletion request, deleted before deleting a project made one or while a
//!   node of an earlier version was live, gets one at the start of every run once no live node runs a version that
//!   cannot read it ([`held`]), so an upgrade purges the projects deleted before it;
//! - it is `applied` when every delete has finished, `verified` when every delete is verified after the mask
//!   cycle, and `completed` once the places outside ClickHouse are accounted for: for a project deletion, once
//!   its remaining objects and its control-plane rows are removed too ([`finish`]). A step of those that fails is
//!   recorded on the request, which stays `verified`, and the next run tries again;
//! - it is `failed` when one of its actions was given up. A failed project deletion is finished by the purge sweep
//!   once it finds no row of the project in any table.
//!
//! Every write is fenced, and what a request has planned so far is in its `places` document, so a new leader
//! continues where the last one stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crucible_auth::stored_project_scope;
use crucible_control::{
    ActionKind, ActionState, DeletionRequest, DeletionSelector, LifecycleAction, Node, NodeAck,
    TxError, duration_micros,
};
use crucible_env::StoreError;
use crucible_export::FilesError;
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_plan::{
    DELETION_TRACES_PAGE, Row, TRACE_ID_LIST_CHUNK, TraceSelector, Value, deletion_issuers,
    deletion_traces, lifecycle_column,
};
use crucible_records::{
    DeletionRequestId, DeletionState, PartitionId, ProjectId, ProjectScope, TableId, Timestamp,
    TraceId, WalEpoch,
};
use serde::{Deserialize, Serialize};

use crate::actions::{self, Place, Shared, advance, planned};
use crate::config::LifecycleConfig;
use crate::detail::{Detail, subject_hash, subject_hex};
use crate::partitions::{days_spanned, partition_window, partitions_of_days};
use crate::reach::{
    CONTROL_PLANE, EXPORT_FILES, MEDIA, OTHER_PLACES, Reach, final_reach, identity_reach,
    page_reach, project_reach,
};

/// The most pages of traces one run resolves for one request.
const MAX_PAGES_PER_RUN: usize = 20;
/// The most probes one run sends to find where a deleted project's rows are. Each stops at the first row of the
/// project it finds, so a run stays short whatever the project holds; the next run goes on after the last partition
/// probed.
pub(crate) const MAX_PROBES_PER_RUN: usize = 1_000;

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
    /// Whether every trace the request selects is resolved and its page planned.
    #[serde(default)]
    pub resolved: bool,
    /// For a deletion by user: the subject hashes its MCP rows are matched with, as 32 hex characters, once
    /// they are known; empty when the project has no salt or its operations carry no issuer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subjects: Option<Vec<String>>,
    /// For a deletion by user: the last trace of the last page of MCP rows found by the user's own columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_cursor: Option<String>,
    /// The UTC days in which those MCP rows start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_days: Vec<u32>,
    /// For a project deletion: the tables whose partitions are planned, by name, in the order of
    /// [`project_reach`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purged: Vec<String>,
    /// For a project deletion: the last partition probed of the next table, when a run's probes ran out in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed: Option<String>,
    /// For a project deletion that has ended: how many purges the purge sweep planned after the end, one per
    /// partition in which rows of the project landed after its purge.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub late: u32,
    /// When the purge sweep last planned such a purge, in microseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_at: Option<i64>,
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

#[allow(clippy::trivially_copy_pass_by_ref)] // The signature serde's `skip_serializing_if` calls.
fn is_zero(n: &u32) -> bool {
    *n == 0
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
            // A project deletion also drops partitions; a drop is verified when it returns.
            if !matches!(action.kind, ActionKind::Delete | ActionKind::Drop) {
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

/// When a project was made: the time of its UUIDv7 id, which `Env::new_id` mints from the clock of the node that
/// makes the project (contract 1.1). An id of another form gives the epoch, which keeps every partition from being
/// dropped while that project lives.
fn made_at(project: ProjectId) -> Timestamp {
    project
        .uuid()
        .v7_parts()
        .map_or(Timestamp::EPOCH, |(millis, _)| {
            Timestamp::from_micros(
                i64::try_from(millis)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000),
            )
        })
}

/// The oldest instant at which a project, live now or made later, can still write a row: the latest end of a
/// partition that a project deletion may drop (contract 13.4).
///
/// Ingest refuses a span that starts more than `ingest.max_event_age` before it arrives (contract 1.2), and spans
/// place every row: each table holds a row at the start of a span its project sent, a fact at the start of its target,
/// which is a stored span of the same project (contract 3.8). A span arrives after its project was made, so a live
/// project writes nothing that starts more than `max_event_age` before it was made, however late a node ships it, and
/// a project made after `db_now` writes nothing that starts more than `max_event_age` before `db_now`. Each instant is
/// taken a clock margin earlier for the node that took the span, and a project's making a margin more for the node
/// that minted its id, so node clocks up to a margin off the database clock change nothing. `db_now` must be read
/// before the live projects: a project the list misses was made visible after it, so it took no span before then.
pub(crate) fn write_horizon(
    db_now: Timestamp,
    live: impl IntoIterator<Item = ProjectId>,
    margin: Duration,
    max_event_age: Duration,
) -> Timestamp {
    let margin = duration_micros(margin);
    live.into_iter()
        .map(|project| made_at(project).saturating_add_micros(-margin))
        .fold(db_now, std::cmp::min)
        .saturating_add_micros(-margin)
        .saturating_add_micros(-duration_micros(max_event_age))
}

/// How the purges of a project deletion that one run of the actions sends remove their rows (contract 13.4).
#[derive(Debug, Default)]
pub(crate) struct Purges {
    /// Whether each project is deleted, read once per run: a deleted project stays deleted, and a project read
    /// as live only keeps a partition from being dropped.
    gone: BTreeMap<ProjectId, bool>,
    /// [`write_horizon`], read once per run: as time passes it only moves later, and a project made after the read
    /// writes nothing before it.
    horizon: Option<Timestamp>,
    /// Whether every live node's clock is within half the clock margin of the database clock ([`clocks_agree`]),
    /// read once per run.
    clocks: Option<bool>,
}

/// Whether every node of `live` reported, at its latest heartbeat, a clock within half of `margin` of the database
/// clock. The drop rule ([`write_horizon`]) holds while every clock is within the margin, so a node whose clock is
/// further off, or that reports none (a version before nodes reported their clocks), keeps every partition from
/// being dropped: the purge deletes the project's rows instead, which is correct whatever the clocks say.
pub(crate) fn clocks_agree(live: &[Node], margin: Duration) -> Result<(), Vec<&Node>> {
    let limit = duration_micros(margin) / 2;
    let off: Vec<&Node> = live
        .iter()
        .filter(|node| {
            node.clock_skew()
                .is_none_or(|skew| skew.saturating_abs() > limit)
        })
        .collect();
    if off.is_empty() { Ok(()) } else { Err(off) }
}

impl Purges {
    /// The kind a purge is sent as: the drop of its partition when the partition ends before every project can
    /// write ([`write_horizon`]), every live node's clock is close enough to the database clock for that horizon to
    /// hold ([`clocks_agree`]) and every project with rows there is deleted, otherwise the delete of its project's
    /// rows, which keeps every other project's. It is decided each time the purge is sent. No project, live or made
    /// later, can write into a partition that ends before the horizon, so nothing that lands after the partition's
    /// projects are listed goes with a drop. Every other action keeps its kind.
    pub(crate) async fn kind(
        &mut self,
        ctx: &LeaderCtx,
        shared: &Shared,
        action: &LifecycleAction,
    ) -> Result<ActionKind, JobError> {
        if Detail::parse(&action.detail) != Ok(Detail::Purge) {
            return Ok(action.kind);
        }
        let config = &shared.config;
        // An unpartitioned table has no partition to drop.
        let Some(window) = partition_window(action.table, config.granularity, action.partition)
        else {
            return Ok(ActionKind::Delete);
        };
        if config.drops_shared_partitions() {
            return Ok(ActionKind::Drop);
        }
        if !config.drops_within_event_age() && window.to() > self.horizon(ctx, config).await? {
            return Ok(ActionKind::Delete);
        }
        // The horizon holds only while every server's clock is close to the database clock.
        if !self.clocks(ctx, config).await? {
            return Ok(ActionKind::Delete);
        }
        // While the store cannot list the partition's projects, the delete is the safe choice.
        let Ok(listed) = shared
            .telemetry
            .partition_projects(action.table, action.partition)
            .await
        else {
            return Ok(ActionKind::Delete);
        };
        for project in listed {
            if !self.is_gone(ctx, project).await? {
                return Ok(ActionKind::Delete);
            }
        }
        Ok(ActionKind::Drop)
    }

    /// [`write_horizon`] over the database clock, then the live projects.
    async fn horizon(
        &mut self,
        ctx: &LeaderCtx,
        config: &LifecycleConfig,
    ) -> Result<Timestamp, JobError> {
        if let Some(horizon) = self.horizon {
            return Ok(horizon);
        }
        let db_now = ctx.store().db_now().await?;
        let live = ctx.store().projects().list().await?;
        let horizon = write_horizon(
            db_now,
            live.iter().map(|project| project.id),
            config.clock_margin,
            config.max_event_age,
        );
        self.horizon = Some(horizon);
        Ok(horizon)
    }

    /// [`clocks_agree`] over the live nodes; when one is off, a warning names it with how far its clock is off.
    async fn clocks(
        &mut self,
        ctx: &LeaderCtx,
        config: &LifecycleConfig,
    ) -> Result<bool, JobError> {
        if let Some(agree) = self.clocks {
            return Ok(agree);
        }
        let live = ctx
            .store()
            .lifecycle()
            .live_nodes(config.live_within)
            .await?;
        let agree = match clocks_agree(&live, config.clock_margin) {
            Ok(()) => true,
            Err(off) => {
                for node in off {
                    // Ahead of the database clock when positive; none from a version that does not report it.
                    let skew_seconds = node.clock_skew().map(|micros| micros / 1_000_000);
                    tracing::warn!(
                        node = %node.wal_epoch, version = %node.version, skew_seconds,
                        "a server's clock is more than {} minutes from the Postgres server's clock, or it reports \
                         none, so a project deletion deletes the project's rows instead of dropping partitions; \
                         set the server's clock right (for example with NTP)",
                        config.clock_margin.as_secs() / 120
                    );
                }
                false
            }
        };
        self.clocks = Some(agree);
        Ok(agree)
    }

    /// Whether a project is deleted. A project the control store does not have counts as live.
    async fn is_gone(&mut self, ctx: &LeaderCtx, project_id: ProjectId) -> Result<bool, JobError> {
        if let Some(gone) = self.gone.get(&project_id) {
            return Ok(*gone);
        }
        let gone = ctx
            .store()
            .projects()
            .get(project_id)
            .await?
            .is_some_and(|project| project.deleted_at.is_some());
        self.gone.insert(project_id, gone);
        Ok(gone)
    }
}

/// Where a table holds rows of a project, as far as one run's probes went.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// The partitions that hold rows of the project, ascending.
    pub(crate) partitions: Vec<PartitionId>,
    /// The last partition probed, when the probes ran out before the table's last partition.
    pub(crate) stopped_after: Option<PartitionId>,
}

/// The partitions of `table` that hold visible rows of `project`, after the partition `after`, with at most about
/// `probes` probes, each of which stops at the first visible row of the project (contract 13.4). Without `after`, one
/// probe first asks whether the table holds any row of the project. Then each partition that the store's catalogue
/// lists is probed in order, and the probes that are left are counted down; when they run out,
/// [`Found::stopped_after`] says where to go on.
/// Before a table's purges have run, no probe reads past a row of the project, so a project of any size is found
/// within the limits of the store's reads. Once they have run, a probe reads past the rows they hide until the mask
/// cycle removes them.
pub(crate) async fn find(
    shared: &Shared,
    table: TableId,
    project: ProjectId,
    after: Option<PartitionId>,
    probes: &mut usize,
) -> Result<Found, StoreError> {
    let telemetry = &shared.telemetry;
    if after.is_none() {
        *probes = probes.saturating_sub(1);
        if !telemetry.holds_project(table, None, project).await? {
            return Ok(Found::default());
        }
    }
    let mut found = Found::default();
    let mut last = None;
    let partitions = telemetry.partitions(table).await?;
    for partition in partitions
        .into_iter()
        .filter(|partition| after.is_none_or(|after| *partition > after))
    {
        if *probes == 0 && last.is_some() {
            found.stopped_after = last;
            return Ok(found);
        }
        *probes = probes.saturating_sub(1);
        if telemetry
            .holds_project(table, Some(partition), project)
            .await?
        {
            found.partitions.push(partition);
        }
        last = Some(partition);
    }
    Ok(found)
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

    /// The delete actions of one set of tables over the partitions of `days`. A table that carries
    /// `user_subject_hash` and is reached by the user also matches `subjects`.
    fn deletes(
        &self,
        request: &DeletionRequest,
        label: &str,
        reach: &[Reach],
        days: &BTreeSet<u32>,
        traces: &[String],
        subjects: &[String],
    ) -> Vec<LifecycleAction> {
        let (user_id, session_id) = match &request.selector {
            DeletionSelector::User { user_id, .. } => (Some(user_id.clone()), None),
            DeletionSelector::Session(session_id) => (None, Some(session_id.clone())),
            DeletionSelector::Traces(_) | DeletionSelector::Project => (None, None),
        };
        let now = self.ctx.env.clock.wall_now();
        let mut actions = Vec::new();
        for place in reach {
            let hashed = place.user && lifecycle_column(place.table, "user_subject_hash").is_some();
            let detail = Detail::Request {
                requested_at: request.requested_at.micros(),
                traces: if place.traces {
                    traces.to_vec()
                } else {
                    Vec::new()
                },
                user_id: user_id.clone().filter(|_| place.user),
                session_id: session_id.clone().filter(|_| place.session),
                subjects: if hashed {
                    subjects.to_vec()
                } else {
                    Vec::new()
                },
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

    /// One fenced transaction: the actions, the request's progress and the deletion of the media uses in
    /// `traces` together, so that a page whose progress is recorded has its uses deleted (contract 13.4: the
    /// stored objects go once their last use is gone, at the next run of the media collector).
    async fn record(
        &self,
        request: &mut DeletionRequest,
        progress: &Progress,
        actions: Vec<LifecycleAction>,
        traces: &[TraceId],
    ) -> JobResult {
        request.places = progress.to_text();
        let row = DeletionRequest {
            updated_at: self.ctx.env.clock.wall_now(),
            ..request.clone()
        };
        let traces = traces.to_vec();
        self.ctx
            .fenced(move |tx| {
                Box::pin(async move {
                    for action in &actions {
                        tx.lifecycle().plan_action(action).await?;
                    }
                    if !traces.is_empty() {
                        tx.media()
                            .delete_trace_refs(row.project_id, &traces, row.requested_at)
                            .await?;
                    }
                    tx.lifecycle().update_request(&row).await?;
                    Ok(())
                })
            })
            .await
    }

    /// Plans the request's deletes (contract 13.3, step 5): resolves its traces page by page, then for a
    /// deletion by user its subject hashes and the days of its MCP rows, then the deletes by the request's own
    /// columns. Each step is kept in the request's progress, so a later run or another leader goes on from it.
    async fn plan(
        &self,
        request: &mut DeletionRequest,
        scope: ProjectScope,
        progress: &mut Progress,
    ) -> JobResult {
        let mut pages = MAX_PAGES_PER_RUN;
        if !progress.resolved {
            self.resolve(request, scope, progress, &mut pages).await?;
            if !progress.resolved {
                return Ok(());
            }
        }
        let mut identity = Vec::new();
        if let DeletionSelector::User { user_id, issuer } = request.selector.clone()
            && !self.shared.config.skips_mcp_identity()
        {
            let Some(subjects) = self
                .subjects(request, scope, &user_id, issuer.as_deref(), progress)
                .await?
            else {
                return Ok(());
            };
            if !self
                .resolve_identity(request, scope, &user_id, &subjects, progress, &mut pages)
                .await?
            {
                return Ok(());
            }
            let days: BTreeSet<u32> = progress.mcp_days.iter().copied().collect();
            identity = self.deletes(
                request,
                "identity",
                &identity_reach(&request.selector),
                &days,
                &[],
                &subjects,
            );
        }
        let days: BTreeSet<u32> = progress.days.iter().copied().collect();
        let mut actions = self.deletes(
            request,
            "final",
            &final_reach(&request.selector),
            &days,
            &[],
            &[],
        );
        actions.extend(identity);
        progress.planned = true;
        request.error = None;
        self.record(request, progress, actions, &[]).await
    }

    /// One page of the traces `selector` resolves, after the trace `after`.
    async fn traces(
        &self,
        scope: ProjectScope,
        selector: TraceSelector<'_>,
        requested_at: Timestamp,
        after: Option<TraceId>,
    ) -> Result<Vec<(TraceId, Timestamp, Timestamp)>, JobError> {
        let plan = deletion_traces(scope, selector, requested_at, after, DELETION_TRACES_PAGE)
            .map_err(|error| {
                JobError::Failed(format!("the trace resolution was refused: {error}"))
            })?;
        let rows = match self.shared.telemetry.execute(plan).await {
            Ok(stream) => stream.rows().await,
            Err(error) => Err(error),
        }
        .map_err(|error| JobError::Failed(format!("the trace resolution failed: {error}")))?;
        Ok(resolved(&rows))
    }

    /// The subject hashes of a deletion by user, as 32 hex characters: the user hashed with the project's salt
    /// under the request's issuer, or under every issuer of the project's MCP operations received before the
    /// request (contract 13.3). They are read once, after the checkpoint wait, so the operations of every
    /// frame received before the request are in the store. `None` while the salt cannot be read: the request
    /// waits, and its error says why.
    async fn subjects(
        &self,
        request: &mut DeletionRequest,
        scope: ProjectScope,
        user_id: &str,
        issuer: Option<&str>,
        progress: &mut Progress,
    ) -> Result<Option<Vec<String>>, JobError> {
        if let Some(subjects) = &progress.subjects {
            return Ok(Some(subjects.clone()));
        }
        let issuers = match issuer {
            Some(issuer) => vec![issuer.to_owned()],
            None => {
                // One more than a delete can carry tells a project with too many issuers apart.
                let limit = u32::try_from(TRACE_ID_LIST_CHUNK + 1).unwrap_or(u32::MAX);
                let plan = deletion_issuers(scope, request.requested_at, None, limit).map_err(
                    |error| JobError::Failed(format!("the issuer read was refused: {error}")),
                )?;
                let rows = match self.shared.telemetry.execute(plan).await {
                    Ok(stream) => stream.rows().await,
                    Err(error) => Err(error),
                }
                .map_err(|error| JobError::Failed(format!("the issuer read failed: {error}")))?;
                let issuers: Vec<String> = rows
                    .iter()
                    .filter_map(|row| match row.values().first() {
                        Some(Value::String(issuer)) => Some(issuer.clone()),
                        _ => None,
                    })
                    .collect();
                if issuers.len() > TRACE_ID_LIST_CHUNK {
                    let why = format!(
                        "the project's MCP operations carry more than {TRACE_ID_LIST_CHUNK} token issuers, more than \
                         one delete can match; ask again with the user's issuer"
                    );
                    self.fail(request, why).await?;
                    return Ok(None);
                }
                issuers
            }
        };
        let hashes = if issuers.is_empty() {
            Vec::new()
        } else {
            match self
                .shared
                .subjects
                .hashes(request.project_id, &issuers, user_id)
                .await
            {
                Ok(hashes) => hashes.unwrap_or_default(),
                Err(why) => {
                    tracing::warn!(
                        request = %request.id, project = %request.project_id, why,
                        "a deletion by user waits for the project's salt"
                    );
                    if request.error.as_deref() != Some(why.as_str()) {
                        request.error = Some(why);
                        self.save(request).await?;
                    }
                    return Ok(None);
                }
            }
        };
        let subjects: Vec<String> = hashes.iter().map(subject_hex).collect();
        progress.subjects = Some(subjects.clone());
        request.error = None;
        self.record(request, progress, Vec::new(), &[]).await?;
        Ok(Some(subjects))
    }

    /// Finds the days of the MCP rows a deletion by user selects by the user's own columns, page by page.
    /// `true` once every page is read.
    async fn resolve_identity(
        &self,
        request: &mut DeletionRequest,
        scope: ProjectScope,
        user_id: &str,
        subjects: &[String],
        progress: &mut Progress,
        pages: &mut usize,
    ) -> Result<bool, JobError> {
        let hashes = subjects
            .iter()
            .map(|hex| subject_hash(hex))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| JobError::Failed(error.to_string()))?;
        while *pages > 0 {
            *pages -= 1;
            let after = progress
                .mcp_cursor
                .as_deref()
                .and_then(|id| TraceId::parse(id).ok());
            let selector = TraceSelector::McpUser(user_id, &hashes);
            let page = self
                .traces(scope, selector, request.requested_at, after)
                .await?;
            let finished = page.len() < DELETION_TRACES_PAGE as usize;
            if let Some((last, _, _)) = page.last() {
                let mut days: BTreeSet<u32> = progress.mcp_days.iter().copied().collect();
                days.extend(
                    page.iter()
                        .flat_map(|(_, first, last)| days_spanned(*first, *last)),
                );
                progress.mcp_days = days.into_iter().collect();
                progress.mcp_cursor = Some(last.to_string());
                self.record(request, progress, Vec::new(), &[]).await?;
            }
            if finished {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Resolves the request's traces page by page and plans the deletes of each page.
    async fn resolve(
        &self,
        request: &mut DeletionRequest,
        scope: ProjectScope,
        progress: &mut Progress,
        pages: &mut usize,
    ) -> JobResult {
        // A list of traces is resolved a chunk of ids at a time: one statement cannot carry the whole list.
        let listed: Option<Vec<TraceId>> = match &request.selector {
            DeletionSelector::Traces(ids) => {
                let mut ids = ids.clone();
                ids.sort_unstable();
                Some(ids)
            }
            DeletionSelector::User { .. }
            | DeletionSelector::Session(_)
            | DeletionSelector::Project => None,
        };
        while *pages > 0 {
            *pages -= 1;
            let after = progress
                .cursor
                .as_deref()
                .and_then(|id| TraceId::parse(id).ok());
            let chunk: Option<&[TraceId]> = listed.as_deref().map(|ids| {
                let from = after.map_or(0, |after| ids.partition_point(|id| *id <= after));
                let rest = &ids[from..];
                &rest[..rest.len().min(TRACE_ID_LIST_CHUNK)]
            });
            let (selector, plan_after) = match (&request.selector, chunk) {
                (_, Some(chunk)) => (TraceSelector::Traces(chunk), None),
                (DeletionSelector::User { user_id, .. }, None) => {
                    (TraceSelector::User(user_id), after)
                }
                (DeletionSelector::Session(session_id), None) => {
                    (TraceSelector::Session(session_id), after)
                }
                (DeletionSelector::Traces(ids), None) => (TraceSelector::Traces(ids), after),
                // A project deletion resolves no traces: it is planned by table (`plan_project`).
                (DeletionSelector::Project, None) => return Ok(()),
            };
            let page = if chunk.is_some_and(<[TraceId]>::is_empty) {
                Vec::new()
            } else {
                self.traces(scope, selector, request.requested_at, plan_after)
                    .await?
            };
            // The next read starts after the chunk of ids, or after the last trace of the page.
            let next = match chunk {
                Some(chunk) => chunk.last().copied(),
                None => page.last().map(|(id, _, _)| *id),
            };
            let finished = match chunk {
                Some(chunk) => chunk.len() < TRACE_ID_LIST_CHUNK,
                None => page.len() < DELETION_TRACES_PAGE as usize,
            };
            if let Some(next) = next {
                let days: BTreeSet<u32> = page
                    .iter()
                    .flat_map(|(_, first, last)| days_spanned(*first, *last))
                    .collect();
                let traces: Vec<String> = page.iter().map(|(id, _, _)| id.to_string()).collect();
                let actions = if page.is_empty() {
                    Vec::new()
                } else {
                    let label = format!("page{}", progress.pages);
                    progress.pages += 1;
                    self.deletes(
                        request,
                        &label,
                        &page_reach(&request.selector),
                        &days,
                        &traces,
                        &[],
                    )
                };
                let mut all: BTreeSet<u32> = progress.days.iter().copied().collect();
                all.extend(days);
                progress.days = all.into_iter().collect();
                progress.cursor = Some(next.to_string());
                progress.traces += page.len() as u64;
                progress.resolved = finished;
                // A listed trace's media uses go even when none of its spans was stored.
                let media: Vec<TraceId> = match chunk {
                    Some(chunk) => chunk.to_vec(),
                    None => page.iter().map(|(id, _, _)| *id).collect(),
                };
                self.record(request, progress, actions, &media).await?;
            }
            if finished {
                // Kept with the next write of the progress, when it was not with this page's.
                progress.resolved = true;
                return Ok(());
            }
        }
        Ok(())
    }

    /// Plans a project deletion (contract 13.4), a table of [`project_reach`] at a time: one purge for each
    /// partition that holds the project's rows ([`find`]). A purge is planned as a delete of the project's rows, and
    /// each time it is sent it becomes the drop of its partition instead when no project can write there any more and
    /// nothing in it is kept ([`Purges::kind`]). A table's purges are recorded in transactions of a bounded size,
    /// then the table joins the request's progress, so a later run or another leader goes on from the next one; a
    /// purge planned again has the key of the first and is not recorded twice. A run sends at most
    /// [`MAX_PROBES_PER_RUN`] probes and records where it stopped.
    async fn plan_project(
        &self,
        request: &mut DeletionRequest,
        progress: &mut Progress,
    ) -> JobResult {
        let mut probes = MAX_PROBES_PER_RUN;
        for table in project_reach() {
            if progress.purged.iter().any(|name| name == table.name()) {
                continue;
            }
            if probes == 0 {
                return Ok(());
            }
            let after = progress
                .probed
                .as_deref()
                .and_then(|text| PartitionId::parse(text).ok());
            let found = match find(self.shared, table, request.project_id, after, &mut probes).await
            {
                Ok(found) => found,
                Err(error) => return self.unreadable(request, table, &error).await,
            };
            let now = self.ctx.env.clock.wall_now();
            let purges = found
                .partitions
                .iter()
                .map(|partition| {
                    planned(
                        self.ctx.env.new_id(),
                        now,
                        format!("deletion:{}:purge:{}:{partition}", request.id, table.name()),
                        ActionKind::Delete,
                        Place {
                            table,
                            partition: *partition,
                            project_id: Some(request.project_id),
                            request_id: Some(request.id),
                        },
                        &Detail::Purge,
                    )
                })
                .collect();
            actions::plan(self.ctx, purges).await?;
            match found.stopped_after {
                Some(last) => progress.probed = Some(last.to_string()),
                None => {
                    progress.purged.push(table.name().to_owned());
                    progress.probed = None;
                }
            }
            request.error = None;
            self.record(request, progress, Vec::new(), &[]).await?;
            if found.stopped_after.is_some() {
                return Ok(());
            }
        }
        progress.resolved = true;
        progress.planned = true;
        request.error = None;
        self.record(request, progress, Vec::new(), &[]).await
    }

    /// A read that finds where a deleted project's rows are failed. The request records why and how many of its
    /// steps failed, so the list of deletion requests shows it, and the next run reads again.
    async fn unreadable(
        &self,
        request: &mut DeletionRequest,
        table: TableId,
        error: &StoreError,
    ) -> JobResult {
        request.attempts = request.attempts.saturating_add(1);
        tracing::warn!(
            request = %request.id, project = %request.project_id, table = table.name(), %error,
            attempts = request.attempts,
            "a read that finds a deleted project's rows failed; the next run of the deletion executor reads again"
        );
        request.error = Some(format!(
            "finding the project's rows in {} failed ({error}); the server tries again every {} s",
            table.name(),
            self.shared.config.deletion_every.as_secs()
        ));
        self.save(request).await
    }

    /// A step of a request whose telemetry is verified failed (contract 13.3, step 8): the request stays `verified`
    /// and records it, with one more failed attempt, so that the list of deletion requests shows why, and the next
    /// run tries again.
    async fn retry(&self, request: &mut DeletionRequest, error: &JobError) -> JobResult {
        let why = format!(
            "{error}; the server tries again every {} s",
            self.shared.config.deletion_every.as_secs()
        );
        request.attempts = request.attempts.saturating_add(1);
        // Once per failure that repeats, rather than at every run.
        if request.error.as_deref() == Some(why.as_str()) {
            tracing::debug!(request = %request.id, %error, attempts = request.attempts, "the last step of a deletion request failed again");
        } else {
            tracing::warn!(
                request = %request.id, project = %request.project_id, %error, attempts = request.attempts,
                "the last step of a deletion request failed; the next run of the deletion executor tries again"
            );
        }
        request.error = Some(why);
        self.save(request).await
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
            if request.selector == DeletionSelector::Project {
                // Only deleting the project makes this request, so a live project is never purged.
                if project.deleted_at.is_none() {
                    let why = "a project deletion names a project that is not deleted".to_owned();
                    return self.fail(&mut request, why).await;
                }
                self.plan_project(&mut request, &mut progress).await?;
            } else {
                self.plan(&mut request, scope, &mut progress).await?;
            }
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
            match finish(self.ctx, self.shared, &request, &mut progress).await {
                Ok(()) => {
                    request.state = DeletionState::Completed;
                    request.error = None;
                }
                Err(JobError::LeaseLost) => return Err(JobError::LeaseLost),
                // The request is saved as `verified`, its telemetry gone, with what failed.
                Err(error) => {
                    request.places = progress.to_text();
                    return self.retry(&mut request, &error).await;
                }
            }
        }
        if (request.state, &progress) != (before.0, &before.1) {
            request.places = progress.to_text();
            self.save(&request).await?;
        }
        Ok(())
    }
}

/// Step 8 of a request whose telemetry is verified (contract 13.3): the export files made before it are expired and
/// removed, and a project deletion then removes the project's objects and control-plane rows. The media uses of the
/// request's traces went with each page, and their objects go with the media collector's next run; without an object
/// store there is no media. Comments have no store yet. A place that is not there is reported as not present, so that
/// the request's status says what was and was not reached. Every step can run again: on an error the stored request
/// is as it was, and the caller records the error and tries again.
pub(crate) async fn finish(
    ctx: &LeaderCtx,
    shared: &Shared,
    request: &DeletionRequest,
    progress: &mut Progress,
) -> JobResult {
    purge_exports(ctx, shared, request).await?;
    if request.selector == DeletionSelector::Project {
        purge_project(ctx, shared, request).await?;
        progress
            .other
            .insert(CONTROL_PLANE.to_owned(), "done".to_owned());
    }
    for place in OTHER_PLACES {
        let reached = place == EXPORT_FILES || (place == MEDIA && shared.config.media);
        let status = if reached { "done" } else { "not_present" };
        progress.other.insert(place.to_owned(), status.to_owned());
    }
    Ok(())
}

/// Expires the exports of the project that were created before the request and removes their files from this node
/// (contract 13.4). The expiry is a fenced write; a file on another node goes at that node's next sweep. Files on the
/// object store go once the expiry has committed, because the transaction must not wait on the store; what that
/// misses, the exports sweep removes, and for a project deletion the removal of the project's objects that follows.
async fn purge_exports(ctx: &LeaderCtx, shared: &Shared, request: &DeletionRequest) -> JobResult {
    let disk = Arc::clone(&ctx.env.disk);
    let now = ctx.env.clock.wall_now();
    let (project_id, requested_at) = (request.project_id, request.requested_at);
    let purged = ctx
        .fenced(move |tx| {
            Box::pin(async move {
                crucible_export::purge(tx, disk.as_ref(), project_id, requested_at, now)
                    .await
                    .map_err(|error| match error {
                        FilesError::Store(error) => error,
                        other => TxError::Unavailable(other.to_string()),
                    })
            })
        })
        .await?;
    if let Some(objects) = &shared.exports
        && let Err(error) = objects.remove(&purged.expired).await
    {
        tracing::warn!(
            %error,
            request = %request.id,
            "removing the export files of a deletion from the object store failed; the exports sweep removes them, \
             or for a project deletion the removal of the project's objects"
        );
    }
    Ok(())
}

/// The control-plane side of a project deletion, once every place of its telemetry is verified (contract 13.4): every
/// object left under the project's key prefix, then the project's rows, its salt among them, in one fenced statement.
/// When the object store refuses, the error says what to check, and the request that is being finished stays where
/// it is and records the error with one more failed attempt: nothing else removes a deleted project's objects, so the
/// next run tries again until the store answers.
async fn purge_project(ctx: &LeaderCtx, shared: &Shared, request: &DeletionRequest) -> JobResult {
    if let Some(objects) = &shared.exports {
        let removed = objects
            .remove_project(request.project_id)
            .await
            .map_err(|error| {
                JobError::Failed(format!(
                    "the object store refused to remove the deleted project's objects under {} ({error}); check \
                     that the object store is reachable and that the server's credentials may list and delete \
                     objects under that prefix",
                    objects.project_root(request.project_id)
                ))
            })?;
        if removed > 0 {
            tracing::info!(request = %request.id, project = %request.project_id, removed, "removed the objects of a deleted project");
        }
    }
    let project_id = request.project_id;
    let purged = ctx
        .fenced(move |tx| Box::pin(async move { tx.lifecycle().purge_project(project_id).await }))
        .await?;
    tracing::info!(
        request = %request.id,
        project = %project_id,
        memberships = purged.memberships,
        alert_rules = purged.alert_rules,
        channels = purged.channels,
        secrets = purged.secrets,
        saved_queries = purged.saved_queries,
        sql_runs = purged.sql_runs,
        export_jobs = purged.export_jobs,
        api_keys = purged.api_keys,
        salts = purged.salts,
        "removed the control-plane rows of a deleted project"
    );
    Ok(())
}

/// Who makes the project deletion request of a project deleted without one.
pub const EARLIER_DELETION_BY: &str = "system:upgrade";
/// The idempotency key of that request, so that a project gets one whichever leader makes it.
const EARLIER_DELETION_KEY: &str = "project-purge";

/// The id of the project deletion request that the leader makes for a project deleted without one: the project's
/// own id, so that the list of deletion requests shows a held request under the id it is made with ([`held`]).
pub(crate) fn earlier_deletion_id(project_id: ProjectId) -> DeletionRequestId {
    DeletionRequestId::from_uuid(project_id.uuid())
}

/// Why no project deletion request can be made now: a live node runs 0.1.3 or earlier, which cannot read one, and one
/// such row would fail its every read of deletion requests (contract 13.4). The text names each such node and what to
/// do; `None` when every live node reads them.
pub(crate) fn held(live: &[Node]) -> Option<String> {
    let earlier: Vec<String> = live
        .iter()
        .filter(|node| !node.reads_project_requests())
        .map(|node| {
            format!(
                "the server with WAL epoch {} ({})",
                node.wal_epoch, node.version
            )
        })
        .collect();
    (!earlier.is_empty()).then(|| {
        format!(
            "the purge waits until every server runs a version later than 0.1.3, which can read it; still on an \
             earlier version: {}. Upgrade or stop it, and the purge starts within about 90 seconds",
            earlier.join(", ")
        )
    })
}

/// Makes the project deletion request of every deleted project that has none (contract 13.4): a project deleted before
/// deleting a project made that request, by this version or by an older server during an upgrade, or while a server
/// of an earlier version was live, is purged as one deleted now. Nothing is made while a live node cannot read such a
/// request ([`held`]); the run warns which nodes hold it, and the list of the project's deletion requests says so.
/// Each request is made under one idempotency key per project and with the project's id, so leaders that race make it
/// once, and a later run finds nothing to do.
async fn request_earlier_deletions(ctx: &LeaderCtx, shared: &Shared) -> JobResult {
    let unrequested = ctx.store().lifecycle().deleted_without_request().await?;
    if unrequested.is_empty() {
        return Ok(());
    }
    let live = ctx
        .store()
        .lifecycle()
        .live_nodes(shared.config.live_within)
        .await?;
    if let Some(why) = held(&live) {
        tracing::warn!(
            projects = unrequested.len(),
            why,
            "the purge of a deleted project waits for every server to run a version that can read it"
        );
        return Ok(());
    }
    for project_id in unrequested {
        let request = DeletionRequest {
            idempotency_key: Some(EARLIER_DELETION_KEY.to_owned()),
            ..DeletionRequest::project(
                earlier_deletion_id(project_id),
                project_id,
                ctx.env.clock.wall_now(),
                EARLIER_DELETION_BY.to_owned(),
            )
        };
        let made = ctx
            .fenced(move |tx| {
                Box::pin(async move { tx.lifecycle().insert_request(&request).await })
            })
            .await;
        match made {
            Ok(_) => tracing::info!(
                project = %project_id,
                "requested the purge of a project that was deleted before deleting a project purged its data"
            ),
            // Another leader made it first.
            Err(JobError::Store(TxError::UniqueViolation(_))) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// One pass of the deletion executor.
pub(crate) async fn run(ctx: LeaderCtx, shared: Arc<Shared>) -> JobResult {
    advance(&ctx, &shared).await?;
    request_earlier_deletions(&ctx, &shared).await?;
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
    use proptest::prelude::*;

    use super::*;

    const DAY: i64 = 86_400_000_000;

    fn day(n: i64) -> Timestamp {
        Timestamp::from_micros(n * DAY)
    }

    fn made(day_number: i64, n: u8) -> ProjectId {
        let millis = u64::try_from(day(day_number).micros() / 1_000).unwrap();
        ProjectId::from_uuid(crucible_records::Uuid::from_v7_parts(millis, u128::from(n)))
    }

    /// The horizon is `max_event_age` and a clock margin before the database clock with no live project, and
    /// `max_event_age` and two margins before the making of the oldest live project when that is earlier; an id that is
    /// not a UUIDv7 keeps every partition.
    #[test]
    fn the_write_horizon_is_before_the_oldest_live_project_and_before_now() {
        let hour = Duration::from_secs(3_600);
        let age = Duration::from_secs(5 * 86_400);
        let hours = |n: i64| n * 3_600_000_000;
        let now = day(100);
        assert_eq!(
            write_horizon(now, [], hour, age),
            day(95).saturating_add_micros(-hours(1))
        );
        assert_eq!(
            write_horizon(now, [made(40, 1), made(90, 2)], hour, age),
            day(35).saturating_add_micros(-hours(2))
        );
        // A project made just now still counts a margin for the node that minted its id.
        assert_eq!(
            write_horizon(now, [made(100, 3)], hour, age),
            day(95).saturating_add_micros(-hours(2))
        );
        assert!(write_horizon(now, [ProjectId::from_bytes([1; 16])], hour, age) < Timestamp::EPOCH);
    }

    proptest! {
        /// No project writes into a partition that ends at or before the horizon: neither a live project, whose spans
        /// arrive after it was made, however late they are shipped, nor a project made after the database clock was
        /// read, with every node's clock at most a margin off the database clock.
        #[test]
        fn no_project_writes_before_the_write_horizon(
            now in 2_000i64..4_000,
            live in prop::collection::vec((0i64..4_000, -60i64..=60), 0..4),
            later in 0i64..500,
            age_days in 1i64..400,
            arrives_after in 0i64..2_000,
            skew in -60i64..=60,
            margin_minutes in 60i64..120,
        ) {
            let minute = 60_000_000_i64;
            let margin = Duration::from_secs(u64::try_from(margin_minutes * 60).unwrap());
            let age = Duration::from_secs(u64::try_from(age_days * 86_400).unwrap());
            let at = |days: i64| day(days);
            // Each live project was made at a true instant; the node that minted its id ran up to an hour off.
            let ids: Vec<ProjectId> = live
                .iter()
                .enumerate()
                .map(|(n, (made_day, id_skew))| {
                    let minted = at(*made_day).min(at(now)).saturating_add_micros(id_skew * minute);
                    let millis = u64::try_from(minted.micros().max(0) / 1_000).unwrap();
                    ProjectId::from_uuid(crucible_records::Uuid::from_v7_parts(millis, n as u128))
                })
                .collect();
            let horizon = write_horizon(at(now), ids.iter().copied(), margin, age);
            // A span of any of them, or of a project made after `now`, arrives at a node whose clock is up to an hour
            // off and starts no earlier than ingest takes it; a fact takes the start of such a span.
            let mut made_days: Vec<i64> = live.iter().map(|(made_day, _)| (*made_day).min(now)).collect();
            made_days.push(now + later);
            for made_day in made_days {
                let arrival = at(made_day).saturating_add_micros(arrives_after * minute);
                let earliest = arrival
                    .saturating_add_micros(skew * minute)
                    .saturating_add_micros(-duration_micros(age));
                prop_assert!(earliest >= horizon, "a row at {earliest} is before the horizon {horizon}");
            }
        }
    }

    fn node(n: u8) -> Node {
        Node {
            wal_epoch: WalEpoch::from_bytes([n; 16]),
            heartbeat_at: Timestamp::EPOCH,
            version: String::new(),
            checkpoint_lsn: Lsn::ZERO,
            address: String::new(),
            node_clock: Some(Timestamp::EPOCH),
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

    /// Contract 13.4: a live node whose clock was more than half the clock margin from the database clock at its latest
    /// heartbeat, or that reported no clock, keeps every partition from being dropped.
    #[test]
    fn a_node_whose_clock_is_off_by_more_than_half_the_margin_keeps_every_partition() {
        let hour = Duration::from_secs(3_600);
        let off_by = |minutes: Option<i64>| Node {
            node_clock: minutes.map(|minutes| Timestamp::from_micros(minutes * 60_000_000)),
            ..node(1)
        };
        assert!(clocks_agree(&[], hour).is_ok());
        assert!(
            clocks_agree(
                &[off_by(Some(0)), off_by(Some(30)), off_by(Some(-30))],
                hour
            )
            .is_ok()
        );
        assert_eq!(
            clocks_agree(&[off_by(Some(0)), off_by(Some(31))], hour)
                .unwrap_err()
                .len(),
            1
        );
        assert!(clocks_agree(&[off_by(Some(-31))], hour).is_err());
        assert!(clocks_agree(&[off_by(None)], hour).is_err());
    }

    /// Contract 13.4: a project deletion request is held while a live node runs 0.1.3 or earlier, and the reason names
    /// each such node with its version.
    #[test]
    fn a_project_deletion_is_held_while_a_live_node_cannot_read_it() {
        let running = |version: &str| Node {
            version: version.to_owned(),
            ..node(2)
        };
        assert_eq!(held(&[]), None);
        assert_eq!(
            held(&[running("0.1.4"), running("0.1.3+project-deletions")]),
            None
        );
        let why = held(&[running("0.1.4"), running("0.1.2")]).unwrap();
        assert!(
            why.contains(&format!("{} (0.1.2)", node(2).wal_epoch)),
            "{why}"
        );
        assert!(why.contains("Upgrade or stop"), "{why}");
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
            r#"{"pages":0,"traces":0,"resolved":false,"planned":false}"#
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
