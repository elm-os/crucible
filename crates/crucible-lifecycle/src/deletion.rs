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
//! - a logged project deletion is planned a table at a time instead: every table of the project, each
//!   partition that holds the project's rows dropped when every project in it is deleted and it ended before
//!   the request by more than the clock margin, otherwise deleted from, whatever the rows' arrival (the
//!   project takes no new data once deleted);
//! - it is `applied` when every delete has finished, `verified` when every delete is verified after the mask
//!   cycle, and `completed` once the places outside ClickHouse are accounted for: for a project deletion, once
//!   its remaining objects and its control-plane rows are removed too;
//! - it is `failed` when one of its actions was given up.
//!
//! Every write is fenced, and what a request has planned so far is in its `places` document, so a new leader
//! continues where the last one stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crucible_auth::stored_project_scope;
use crucible_control::{
    ActionKind, ActionState, DeletionRequest, DeletionSelector, LifecycleAction, Node, NodeAck,
    TxError, duration_micros,
};
use crucible_export::FilesError;
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_plan::{
    DELETION_TRACES_PAGE, Row, TRACE_ID_LIST_CHUNK, TraceSelector, Value, deletion_issuers,
    deletion_traces, lifecycle_column, lifecycle_days,
};
use crucible_records::{
    DeletionState, PartitionId, ProjectId, ProjectScope, TableId, Timestamp, TraceId, WalEpoch,
};
use crucible_schema::PartitionExpr;
use serde::{Deserialize, Serialize};

use crate::actions::{Place, Shared, advance, planned};
use crate::detail::{Detail, subject_hash, subject_hex};
use crate::partitions::{day_of, days_spanned, partition_window, partitions_of_days};
use crate::reach::{
    CONTROL_PLANE, EXPORT_FILES, MEDIA, OTHER_PLACES, Reach, final_reach, identity_reach,
    page_reach, project_reach,
};

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

    /// Expires the exports of the project that were created before the request and removes their files from
    /// this node (contract 13.4). The expiry is a fenced write; a file on another node goes at that node's
    /// next sweep. Files on the object store go once the expiry has committed, because the transaction must not
    /// wait on the store; what that misses, the sweep removes.
    async fn purge_exports(&self, request: &DeletionRequest) -> JobResult {
        let disk = Arc::clone(&self.ctx.env.disk);
        let now = self.ctx.env.clock.wall_now();
        let (project_id, requested_at) = (request.project_id, request.requested_at);
        let purged = self
            .ctx
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
        if let Some(objects) = &self.shared.exports
            && let Err(error) = objects.remove(&purged.expired).await
        {
            tracing::warn!(
                %error,
                request = %request.id,
                "removing the export files of a deletion from the object store failed; the exports sweep removes them"
            );
        }
        Ok(())
    }

    /// Plans a project deletion (contract 13.4), a table of [`project_reach`] at a time: the partitions that
    /// hold the project's rows are read from the table, and each gets a drop or a delete
    /// ([`Executor::purge_action`]). The tables planned so far are kept in the request's progress, so a later
    /// run or another leader goes on from the next one.
    async fn plan_project(
        &self,
        request: &mut DeletionRequest,
        scope: ProjectScope,
        progress: &mut Progress,
    ) -> JobResult {
        let mut gone = BTreeMap::new();
        for table in project_reach() {
            if progress.purged.iter().any(|name| name == table.name()) {
                continue;
            }
            let mut actions = Vec::new();
            for partition in self.partitions_of(scope, table).await? {
                if let Some(action) = self
                    .purge_action(request, table, partition, &mut gone)
                    .await?
                {
                    actions.push(action);
                }
            }
            progress.purged.push(table.name().to_owned());
            self.record(request, progress, actions, &[]).await?;
        }
        progress.resolved = true;
        progress.planned = true;
        request.error = None;
        self.record(request, progress, Vec::new(), &[]).await
    }

    /// The partitions of a table that hold rows of the project: those of the UTC days its rows fall in, or the
    /// one partition of an unpartitioned table.
    async fn partitions_of(
        &self,
        scope: ProjectScope,
        table: TableId,
    ) -> Result<BTreeSet<PartitionId>, JobError> {
        if crucible_schema::table(table).partition == PartitionExpr::None {
            return Ok(BTreeSet::from([PartitionId::UNPARTITIONED]));
        }
        let plan = lifecycle_days(scope, table).map_err(|error| {
            JobError::Failed(format!("the read of a table's days was refused: {error}"))
        })?;
        let rows = match self.shared.telemetry.execute(plan).await {
            Ok(stream) => stream.rows().await,
            Err(error) => Err(error),
        }
        .map_err(|error| JobError::Failed(format!("the read of a table's days failed: {error}")))?;
        let days = rows.iter().filter_map(|row| match row.values().first() {
            Some(Value::Time(bucket)) => day_of(*bucket),
            _ => None,
        });
        Ok(partitions_of_days(
            table,
            self.shared.config.granularity,
            days,
        ))
    }

    /// Whether a project is deleted, read once per run.
    async fn is_gone(
        &self,
        project_id: ProjectId,
        gone: &mut BTreeMap<ProjectId, bool>,
    ) -> Result<bool, JobError> {
        if let Some(known) = gone.get(&project_id) {
            return Ok(*known);
        }
        let deleted = self
            .ctx
            .store()
            .projects()
            .get(project_id)
            .await?
            .is_some_and(|project| project.deleted_at.is_some());
        gone.insert(project_id, deleted);
        Ok(deleted)
    }

    /// What removes the project's rows from one partition. A partition is dropped when every project with
    /// rows in it is deleted and it ended before the request by more than the clock margin, so that no live
    /// project can still be writing its first rows there (contract 13.2, as retention drops a partition);
    /// otherwise the project's rows are deleted from it. Nothing is planned for a partition that a read finds
    /// without a visible row of the project.
    async fn purge_action(
        &self,
        request: &DeletionRequest,
        table: TableId,
        partition: PartitionId,
        gone: &mut BTreeMap<ProjectId, bool>,
    ) -> Result<Option<LifecycleAction>, JobError> {
        let config = &self.shared.config;
        let margin = duration_micros(config.clock_margin);
        let settled =
            partition_window(table, config.granularity, partition).is_some_and(|window| {
                window.to() <= request.requested_at.saturating_add_micros(-margin)
            });
        let mut kind = ActionKind::Delete;
        if settled || partition.is_unpartitioned() {
            let listed = self
                .shared
                .telemetry
                .partition_projects(table, partition)
                .await
                .map_err(|error| {
                    JobError::Failed(format!(
                        "the read of a partition's projects failed: {error}"
                    ))
                })?;
            if !listed.contains(&request.project_id) {
                return Ok(None);
            }
            if settled {
                kind = ActionKind::Drop;
                for project in listed {
                    if config.drops_shared_partitions() {
                        break;
                    }
                    if project != request.project_id && !self.is_gone(project, gone).await? {
                        kind = ActionKind::Delete;
                        break;
                    }
                }
            }
        }
        let detail = match kind {
            ActionKind::Drop => Detail::Drop,
            _ => Detail::Purge,
        };
        Ok(Some(planned(
            self.ctx.env.new_id(),
            self.ctx.env.clock.wall_now(),
            format!("deletion:{}:purge:{}:{partition}", request.id, table.name()),
            kind,
            Place {
                table,
                partition,
                project_id: Some(request.project_id),
                request_id: Some(request.id),
            },
            &detail,
        )))
    }

    /// The control-plane side of a project deletion, once every place of its telemetry is verified (contract
    /// 13.4): every object left under the project's key prefix, then the project's rows, its salt among them,
    /// in one fenced statement. An object store that does not answer holds the request at `verified`, and the
    /// next run tries again, because a deleted project's objects are swept by nothing else.
    async fn purge_project(&self, request: &DeletionRequest) -> JobResult {
        if let Some(objects) = &self.shared.exports {
            let removed = objects
                .remove_project(request.project_id)
                .await
                .map_err(|error| {
                    JobError::Failed(format!(
                        "removing the deleted project's objects failed: {error}"
                    ))
                })?;
            if removed > 0 {
                tracing::info!(request = %request.id, project = %request.project_id, removed, "removed the objects of a deleted project");
            }
        }
        let project_id = request.project_id;
        let purged = self
            .ctx
            .fenced(move |tx| {
                Box::pin(async move { tx.lifecycle().purge_project(project_id).await })
            })
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
                self.plan_project(&mut request, scope, &mut progress)
                    .await?;
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
            // Step 8. Export files made before the request are expired and removed. The media uses of the
            // request's traces went with each page, and their objects go with the media collector's next run;
            // without an object store there is no media. Comments have no store yet. A place that is not there
            // is reported as not present, so that the request's status says what was and was not reached. A
            // project deletion then removes the project's objects and control-plane rows.
            self.purge_exports(&request).await?;
            let project = request.selector == DeletionSelector::Project;
            if project {
                self.purge_project(&request).await?;
                progress
                    .other
                    .insert(CONTROL_PLANE.to_owned(), "done".to_owned());
            }
            for place in OTHER_PLACES {
                let reached = place == EXPORT_FILES || (place == MEDIA && self.shared.config.media);
                let status = if reached { "done" } else { "not_present" };
                progress.other.insert(place.to_owned(), status.to_owned());
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
