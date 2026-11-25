//! Retention and deletion over `ChModel` and `PgModel` under faults (contract 13, invariant I9).
//!
//! Several nodes each run the real lifecycle: the node timer, the retention executor, the mask scheduler and
//! the deletion executor of `crucible-lifecycle` through the real job registry, and the real `Service` that the
//! API calls. What the simulation models is the node's log and its stage B: frames of spans are appended to a
//! log per node, a shipper task moves them to the store in log order and raises the checkpoint watermark, and a
//! `Tombstone` frame is one more frame of that log.
//!
//! - **crashes**: every task of a node is aborted; the node starts again over the same log, sometimes after
//!   more than the 60 s for which its heartbeat counts as live;
//! - **shipper stalls**: a node's shipper stops for up to 90 s, so frames received before a deletion request
//!   are still on their way when the request is logged;
//! - **store faults**: the control store fails over, loses commits with an unknown outcome and aborts
//!   transactions; the telemetry store goes away for up to 20 s and fails reads and maintenance calls;
//! - **lease steals**: the `lifecycle` lease row is rewritten for another holder under its leader.
//!
//! A fault-free tail of more than two mask cycles follows. The oracle:
//!
//! - **L1 (fencing)**: every delete, partition drop and mask application that reaches the store has an action
//!   row recorded as issued under a fencing token.
//! - **L2 (hidden at once)**: when a request is accepted, the accepting node's pending-deletion set holds it,
//!   and a read that carries the set returns none of the rows the request selects.
//! - **L3 (completion)**: at the end every deletion request is `completed` and every action is `verified`.
//! - **I9**: at the end no reach-list table holds a row, masked or not, that a completed request selects or
//!   that retention has expired, and every span and MCP row that nothing selected is still there.
//!
//! The model's stage B keeps the two rules that the deletion executor relies on: it applies a tombstone to the
//! frames after it in the log, and a node that starts registers first and then consults the deletion requests
//! before it ships.
//!
//! A trace shows its user on its spans, on its MCP operations or on both; an operation carries the user as
//! `user_id` or only as the hash of the user's subject under one of two issuers. A deletion by user must reach
//! the MCP rows by their own identity, also where no span of the trace names the user, and when it names an
//! issuer it must keep the rows hashed under the other one.
//!
//! A third project, made long before the other two, is deleted during the run, in the transaction the API's
//! project deletion runs: its row is marked deleted and its project deletion request made. Besides what the run
//! sends, it holds older history that it sent when that was younger than `ingest.max_event_age`, which is 12 days
//! here: partitions that no project can write to any more, which the deletion drops. Its frames are still taken
//! for up to 5 s after the deletion, as nodes whose key snapshot has not caught up take them. At the end, I9 also
//! requires that no table of the reach list holds a row of the deleted project, the minute rollups, the hourly tier,
//! the deploys and the inventory included, whatever the row's arrival, that its control-plane rows are gone, and
//! that the other projects lost no row of a partition that the deletion dropped or deleted from.
//!
//! The project that keeps everything also writes late: at the moment the deletion's drop or delete of a
//! `span_core` partition reaches the store, a frame of it whose spans start in that partition lands there, as a
//! node ships a span late that ingest took when it started at most `max_event_age` before its arrival, after the
//! project was made. The deletion must keep those rows too, so it may drop a partition only where no project can
//! still write.
//!
//! Rows of the deleted project also land after its request has completed, once the faulty phase is over: a frame a
//! node took just after the deletion while its key snapshot lagged, one a node without a heartbeat took before the
//! deletion and ships late, one a node cut off from Postgres took under `fail_open`, and one from the WAL of a node
//! that returns after weeks, into a partition the deletion dropped. **I9 for these is eventual**: within one sweep
//! interval and two runs of the executor of landing, a delete or a drop removes them from every read, and at the
//! end they are gone.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_auth::test_support::scope;
use crucible_chfmt::EncodedBlock;
use crucible_control::{
    ActionKind, ActionState, AlertRule, ApiKey, Channel, ChannelKind, ControlStore,
    DeletionRequest, DeletionSelector, Email, ExportFormat, JsonDoc, KeyScope, LEASE_TTL,
    LeaseName, NewExport, Project, ProjectMembership, ProjectRole, Repos, RetentionFloor,
    SavedQuery, SealedProjectSalt, Secret, SqlActor, SqlRun, SqlRunKind, SqlVisibility, TxError,
    User, UserStatus, transaction,
};
use crucible_env::{
    Clock, ClockExt, Env, InsertError, InsertSettings, Instant, MutationId, MutationStatus,
    QueryError, Rng, RowStream, StoreError, StoreFacts, StoreFlavour, StoreHealth, TelemetryStore,
};
use crucible_jobs::Registry;
use crucible_lifecycle::{
    LeaderSource, LeaderStores, LifecycleConfig, LogError, NewDeletion, NodeLog, NodeState,
    Service, Subjects, leader_jobs, node_job, partition_window, retention_tables,
};
use crucible_normalize::ProjectSalt;
use crucible_plan::{
    BatchProbe, DeletePlan, DeleteTarget, Pred, QueryPlan, TraceSelector, Value, deletion_traces,
};
use crucible_records::{
    AlertRuleId, ApiKeyId, BatchToken, ChannelId, DeletionRequestId, DeletionState, ExportJobId,
    Lsn, PartitionId, ProjectId, SavedQueryId, SqlRunId, TableId, Timestamp, TraceId, UserId, Uuid,
    WalEpoch,
};
use crucible_schema::{
    Granularity, MigrationStep, RetentionSettings, TableKind, day_of, route, table,
};
use futures::future::BoxFuture;

use crate::blocks::Rows;
use crate::ch_model::{ChFaults, ChModel};
use crate::ch_read::rows_of as stored_rows;
use crate::pg_model::{PgFaults, PgModel};
use crate::{Sim, run};

const DAY: i64 = 86_400_000_000;
/// The retention the first project gets during the run, in days.
const RETENTION_DAYS: u32 = 3;
/// The tables a frame of spans writes one row per span to.
const SPAN_TABLES: [TableId; 3] = [TableId::SpanCore, TableId::SpanPayload, TableId::TraceHeads];
/// The MCP tables, which a deletion by user also reaches by their own `user_id` and subject hash.
const MCP_TABLES: [TableId; 2] = [TableId::McpOps, TableId::McpContent];
/// The tables a deletion reaches through the traces it resolves.
const TRACE_TABLES: [TableId; 7] = [
    TableId::McpOps,
    TableId::McpContent,
    TableId::FactScores,
    TableId::FactTags,
    TableId::FactFlags,
    TableId::FactCorrections,
    TableId::TraceStats,
];
const ROLLUPS: [TableId; 2] = [TableId::SessionRollup, TableId::UserRollup];
/// The tables without a user, session or trace dimension, which only retention and a project deletion reach.
const COUNTS: [TableId; 5] = [
    TableId::SpanMetrics,
    TableId::McpToolRollup,
    TableId::McpToolRollupHourly,
    TableId::McpDeploys,
    TableId::McpToolInventory,
];
/// How long after a project's deletion its frames are still taken: a node takes them until its key snapshot
/// has the revocation, which the key feed brings within 5 s.
const REVOCATION_WINDOW_MILLIS: u64 = 5_000;
/// `lifecycle.sweep_every` in the simulation.
const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// How many days before the run the deleted project was made.
const DELETED_MADE_DAYS_AGO: u64 = 40;
/// The days back, before the run's start, in which the deleted project's history starts.
const HISTORY_DAYS: std::ops::RangeInclusive<u32> = 15..=25;

/// A seeded defect of the lifecycle that the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// The deletion executor does not wait for every live node's checkpoint to pass the tombstone.
    SkipCheckpointWait,
    /// The mask scheduler never plans a mask application.
    DropMaskCycle,
    /// An action is sent to the store without being recorded under the fencing token first.
    DeleteWithoutFencing,
    /// A deletion by user reaches the MCP rows only through the traces its spans resolve to.
    McpByTraceOnly,
    /// A project deletion drops the partitions it reaches whatever other projects hold rows there.
    DropSharedPartitions,
    /// A project deletion drops a partition in which every project with rows is deleted without the event-age
    /// bound, although a live project can still write rows there.
    DropWithinEventAge,
    /// The purge sweep never runs.
    SweepDisabled,
    /// The purge sweep removes the rows it finds without the project predicate: it drops their partition.
    SweepWithoutProject,
}

/// What one run does.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Nodes that run the lifecycle.
    pub nodes: usize,
    /// How long the faulty phase lasts, in virtual time.
    pub faulty_for: Duration,
    /// How long the fault-free tail lasts: more than two mask cycles.
    pub tail: Duration,
    /// Whether nodes crash.
    pub crashes: bool,
    /// Whether the `lifecycle` lease is stolen.
    pub steals: bool,
    /// Whether the stores fail.
    pub store_faults: bool,
    /// The seeded defect, if any.
    pub mutant: Option<Mutant>,
}

impl Options {
    /// Every fault.
    pub const FAULTY: Self = Self {
        nodes: 3,
        faulty_for: Duration::from_secs(900),
        tail: Duration::from_secs(6_000),
        crashes: true,
        steals: true,
        store_faults: true,
        mutant: None,
    };
    /// No fault but shipper stalls: the run in which a defect of the executors shows with nothing else in
    /// the way.
    pub const CALM: Self = Self {
        crashes: false,
        steals: false,
        store_faults: false,
        ..Self::FAULTY
    };
}

/// What happened in a run, so that a test can require that the interesting cases occurred.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Times a node started.
    pub boots: u64,
    /// Crashes.
    pub crashes: u64,
    /// Crashes after which the node stayed down for longer than its heartbeat counts as live.
    pub long_downtimes: u64,
    /// Shipper stalls.
    pub stalls: u64,
    /// Leases stolen.
    pub steals: u64,
    /// Failovers of the control store.
    pub failovers: u64,
    /// Outages of the telemetry store.
    pub outages: u64,
    /// Frames of spans appended.
    pub frames: u64,
    /// Spans shipped to the store.
    pub shipped: u64,
    /// Spans the model's stage B dropped at a tombstone or on recovery.
    pub stage_b_drops: u64,
    /// Deletion requests accepted, by trace, by user and by session.
    pub by_trace: u64,
    /// See `by_trace`.
    pub by_user: u64,
    /// See `by_trace`.
    pub by_session: u64,
    /// Of the requests by user, those that name a token issuer.
    pub by_user_issuer: u64,
    /// Requests whose call failed and was sent again under the same idempotency key.
    pub request_retries: u64,
    /// Retention changes accepted.
    pub retention_changes: u64,
    /// Deletes that reached the store.
    pub deletes: u64,
    /// Partition drops that reached the store.
    pub drops: u64,
    /// Mask applications that reached the store.
    pub masks: u64,
    /// Requests that were completed at the end.
    pub completed: u64,
    /// Reads with the pending-deletion set that were checked.
    pub hidden_reads: u64,
    /// Rows the oracle looked at in the store at the end.
    pub rows_checked: u64,
    /// Frames shipped whose MCP rows carry their user where their spans do not: by `user_id`, or only by
    /// the subject hash.
    pub mcp_identity_frames: u64,
    /// Frames whose MCP rows the model's stage B dropped for a deletion by user while it kept their spans.
    pub mcp_drops: u64,
    /// MCP rows that a deletion by user would select if it named no issuer, kept because it names another issuer
    /// than theirs, and checked still there at the end.
    pub mcp_kept_by_issuer: u64,
    /// Reads of the project salts that failed.
    pub salt_failures: u64,
    /// Projects deleted, each with its project deletion request.
    pub by_project: u64,
    /// Frames of a deleted project taken after its deletion, as nodes whose key snapshot was behind take them.
    pub late_frames: u64,
    /// Control-plane rows of the projects checked at the end: gone for the deleted project, there for another.
    pub control_rows_checked: u64,
    /// Late frames of the project that keeps everything that landed in a partition just as the deletion's drop
    /// or delete of it reached the store.
    pub live_late_frames: u64,
    /// Frames of the deleted project's history, sent before the run.
    pub history_frames: u64,
    /// Partitions that a project deletion or the purge sweep dropped.
    pub purge_drops: u64,
    /// Frames of the deleted project that landed after its request completed.
    pub after_completion: u64,
    /// Of those, the frames whose rows a delete or a drop removed within one sweep interval and two runs of the
    /// executor.
    pub swept_in_time: u64,
}

impl Stats {
    /// Adds another run's counts.
    pub fn add(&mut self, other: &Self) {
        self.boots += other.boots;
        self.crashes += other.crashes;
        self.long_downtimes += other.long_downtimes;
        self.stalls += other.stalls;
        self.steals += other.steals;
        self.failovers += other.failovers;
        self.outages += other.outages;
        self.frames += other.frames;
        self.shipped += other.shipped;
        self.stage_b_drops += other.stage_b_drops;
        self.by_trace += other.by_trace;
        self.by_user += other.by_user;
        self.by_session += other.by_session;
        self.by_user_issuer += other.by_user_issuer;
        self.request_retries += other.request_retries;
        self.retention_changes += other.retention_changes;
        self.deletes += other.deletes;
        self.drops += other.drops;
        self.masks += other.masks;
        self.completed += other.completed;
        self.hidden_reads += other.hidden_reads;
        self.rows_checked += other.rows_checked;
        self.mcp_identity_frames += other.mcp_identity_frames;
        self.mcp_drops += other.mcp_drops;
        self.mcp_kept_by_issuer += other.mcp_kept_by_issuer;
        self.salt_failures += other.salt_failures;
        self.by_project += other.by_project;
        self.late_frames += other.late_frames;
        self.control_rows_checked += other.control_rows_checked;
        self.live_late_frames += other.live_late_frames;
        self.history_frames += other.history_frames;
        self.purge_drops += other.purge_drops;
        self.after_completion += other.after_completion;
        self.swept_in_time += other.swept_in_time;
    }
}

/// The result of one run.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The run seed.
    pub seed: u64,
    /// What happened.
    pub stats: Stats,
    /// Every invariant the run broke; empty when retention and deletion held.
    pub violations: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn below(rng: &dyn Rng, bound: u64) -> u64 {
    rng.next_u64() % bound.max(1)
}

fn millis(rng: &dyn Rng, from: u64, to: u64) -> Duration {
    Duration::from_millis(from + below(rng, to - from + 1))
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Where a trace's user shows: on its spans, on its MCP operations, or both. An operation carries the user as
/// `user_id`, or only as the hash of the user's subject under an issuer (contract 9.3).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    /// The span tables and the rollups carry `user_id`.
    spans: bool,
    /// The MCP rows carry `user_id`.
    mcp_user: bool,
    /// The MCP rows carry the hash of the user under this issuer.
    issuer: Option<&'static str>,
}

/// The token issuers of the workload's MCP operations.
const ISSUERS: [&str; 2] = ["https://a.example", "https://b.example"];

/// The spans of one frame: one trace of one project, of one user and one session, starting on one day.
#[derive(Clone, Debug)]
struct Spans {
    project: ProjectId,
    trace: TraceId,
    user: String,
    identity: Identity,
    session: String,
    start: Timestamp,
    received_at: Timestamp,
    span_ids: Vec<[u8; 8]>,
}

impl Spans {
    /// Whether the request selects the frame's spans, and with them every row of the frame.
    fn selected_by(&self, request: &DeletionRequest) -> bool {
        request.project_id == self.project
            && self.received_at < request.requested_at
            && match &request.selector {
                DeletionSelector::Traces(ids) => ids.contains(&self.trace),
                DeletionSelector::User { user_id, .. } => {
                    self.identity.spans && *user_id == self.user
                }
                DeletionSelector::Session(session_id) => *session_id == self.session,
                DeletionSelector::Project => true,
            }
    }

    /// Whether a completed project deletion removed every row of the frame, whatever its arrival: the deletion
    /// reaches every row of its project (contract 13.4).
    fn purged_by(&self, request: &DeletionRequest) -> bool {
        request.project_id == self.project
            && request.selector == DeletionSelector::Project
            && request.state == DeletionState::Completed
    }

    /// Whether a deletion by user selects the frame's MCP rows by their own `user_id`, or by their subject hash
    /// under the issuer the request names, or under any issuer when it names none (contract 13.3).
    fn mcp_selected_by(&self, request: &DeletionRequest) -> bool {
        request.project_id == self.project
            && self.received_at < request.requested_at
            && matches!(&request.selector, DeletionSelector::User { user_id, issuer }
            if *user_id == self.user
                && (self.identity.mcp_user
                    || self.identity.issuer.is_some_and(|own| {
                        issuer.as_deref().is_none_or(|named| named == own)
                    })))
    }
}

/// The salt of a project in the simulation.
fn salt(project: ProjectId) -> ProjectSalt {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&project.uuid().to_bytes());
    ProjectSalt::new(bytes)
}

/// The project salts as the deletion executor reads them: they fail now and then while the stores do.
struct SimSubjects {
    rng: Arc<dyn Rng>,
    faulty: Arc<AtomicBool>,
    ledger: Arc<Mutex<Ledger>>,
}

impl fmt::Debug for SimSubjects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SimSubjects")
    }
}

impl Subjects for SimSubjects {
    fn hashes<'a>(
        &'a self,
        project_id: ProjectId,
        issuers: &'a [String],
        subject: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<[u8; 16]>>, String>> {
        Box::pin(async move {
            if self.faulty.load(Ordering::SeqCst) && below(self.rng.as_ref(), 4) == 0 {
                lock(&self.ledger).stats.salt_failures += 1;
                return Err("the control store did not answer; the salt is read again".to_owned());
            }
            let salt = salt(project_id);
            Ok(Some(
                issuers
                    .iter()
                    .map(|issuer| salt.subject_hash(issuer, subject))
                    .collect(),
            ))
        })
    }
}

#[derive(Clone, Debug)]
enum Frame {
    Spans(Spans),
    Tombstone(DeletionRequest),
}

#[derive(Default)]
struct LogState {
    last_lsn: u64,
    /// The frames stage B has not resolved yet, in log order.
    unshipped: VecDeque<(u64, Frame)>,
    /// The tables of the front frame whose block the store has acknowledged.
    front_done: BTreeSet<TableId>,
    watermark: u64,
}

/// A node's log: it survives the node's crashes.
struct SimLog {
    epoch: WalEpoch,
    state: Mutex<LogState>,
}

impl fmt::Debug for SimLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SimLog({})", self.epoch)
    }
}

impl SimLog {
    fn append(&self, frame: Frame) -> u64 {
        let mut state = lock(&self.state);
        state.last_lsn += 1;
        let lsn = state.last_lsn;
        state.unshipped.push_back((lsn, frame));
        lsn
    }
}

#[async_trait]
impl NodeLog for SimLog {
    fn epoch(&self) -> WalEpoch {
        self.epoch
    }

    async fn append_tombstone(&self, request: &DeletionRequest) -> Result<Lsn, LogError> {
        Ok(Lsn::new(self.append(Frame::Tombstone(request.clone()))))
    }

    fn watermark(&self) -> Lsn {
        Lsn::new(lock(&self.state).watermark)
    }
}

/// What the harness saw, shared by every node.
#[derive(Default)]
struct Ledger {
    stats: Stats,
    violations: Vec<String>,
    /// Every span frame that was appended, with whether the store got it.
    spans: Vec<(Spans, Fate)>,
    /// Whether the first project's retention was shortened.
    retention_set: bool,
    /// The frames of the deleted project that landed after its request completed, with when they landed on the
    /// database host's clock, in nanoseconds.
    after_completion: Vec<(Spans, u64)>,
    /// Every delete of the deleted project's rows and every partition drop that reached the store, with when.
    removals: Vec<(TableId, PartitionId, u64)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    Unshipped,
    Shipped,
    Dropped,
}

/// The late frames of the project that keeps everything: one per `span_core` partition that the project deletion
/// acts on.
#[derive(Default)]
struct Late {
    /// The partitions that have had their late frame.
    landed: BTreeSet<PartitionId>,
    /// The log position of the last late frame, in the log of [`late_epoch`].
    lsn: u64,
}

/// The log the late frames come from: a node that ships only them.
fn late_epoch() -> WalEpoch {
    WalEpoch::from_uuid(Uuid::from_v7_parts(1_800_000_000_200, 1))
}

/// The log the deleted project's history and its frames after the completion come from.
fn history_epoch() -> WalEpoch {
    WalEpoch::from_uuid(Uuid::from_v7_parts(1_800_000_000_300, 1))
}

/// `ingest.max_event_age` in the simulation: older history is out of every project's reach.
const MAX_EVENT_AGE_DAYS: i64 = 12;

/// When a project of the simulation was made: the time of its UUIDv7 id.
fn made_at(project: ProjectId) -> Timestamp {
    project
        .uuid()
        .v7_parts()
        .map_or(Timestamp::EPOCH, |(millis, _)| {
            Timestamp::from_micros(i64::try_from(millis).unwrap_or(0).saturating_mul(1_000))
        })
}

/// Lands every row of a frame in the store at once, as a shipper writes it, from the log `epoch` at `lsn`. `false`
/// when the store did not take the whole frame.
async fn land(ch: &ChModel, spans: &Spans, epoch: WalEpoch, lsn: u64) -> bool {
    for table_id in frame_tables() {
        let Some((rows, partition)) = rows_of(table_id, spans, epoch, lsn) else {
            return false;
        };
        let Ok(block) = rows.encode() else {
            return false;
        };
        let Ok(token) = BatchToken::new(epoch, Lsn::new(lsn), Lsn::new(lsn), table_id, partition)
        else {
            return false;
        };
        let settings = InsertSettings::new(token, StoreFlavour::SingleNode);
        if ch.insert_block(block, settings).await.is_err() {
            return false;
        }
    }
    true
}

/// The telemetry store as the leader reaches it, with the oracle of L1.
struct Audited {
    ch: ChModel,
    pg: PgModel,
    ledger: Arc<Mutex<Ledger>>,
    /// The project that keeps everything writes the late frames, into partitions of the deleted project.
    projects: [ProjectId; 3],
    ids: Arc<AtomicU64>,
    late: Arc<Mutex<Late>>,
    /// The database host's clock, which stamps a late frame's arrival.
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Audited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Audited")
    }
}

impl Audited {
    /// L1: the action is in the ledger of Postgres as issued under a fencing token before the store sees it.
    fn fenced(&self, kind: ActionKind, table_id: TableId, partition: PartitionId) {
        let recorded = self.pg.lifecycle_actions().into_iter().any(|action| {
            action.kind == kind
                && action.table == table_id
                && action.partition == partition
                && action.state == ActionState::Issued
                && action.fencing_token > 0
        });
        let mut ledger = lock(&self.ledger);
        match kind {
            ActionKind::Delete => ledger.stats.deletes += 1,
            ActionKind::Drop => ledger.stats.drops += 1,
            ActionKind::Mask => ledger.stats.masks += 1,
        }
        if !recorded {
            ledger.violations.push(format!(
                "L1: a {kind:?} on {} partition {partition} reached the store without an action recorded \
                 as issued under a fencing token",
                table_id.name()
            ));
        }
    }

    /// Whether a drop of the partition belongs to a deletion request: only a project deletion drops partitions
    /// for a request, retention drops for none.
    fn drops_for_a_request(&self, table_id: TableId, partition: PartitionId) -> bool {
        self.pg.lifecycle_actions().into_iter().any(|action| {
            action.kind == ActionKind::Drop
                && action.table == table_id
                && action.partition == partition
                && action.state == ActionState::Issued
                && action.request_id.is_some()
        })
    }

    /// A late frame of the project that keeps everything lands in a `span_core` partition, just before the project
    /// deletion's drop or delete of it reaches the store: its spans start in the partition, as a span that ingest
    /// took at most `ingest.max_event_age` after it started, once the project was made, and that a node ships late.
    /// A partition that ends before that gets no such frame, since no node takes one. Every table gets the frame's
    /// rows, as a shipper writes them. Once per partition; a frame the store does not take whole is not counted,
    /// and no request can select it.
    async fn land_late(&self, partition: PartitionId) {
        let Some(window) = partition_window(TableId::SpanCore, Granularity::Day, partition) else {
            return;
        };
        let reach =
            made_at(self.projects[1]).saturating_add_micros(-MAX_EVENT_AGE_DAYS * DAY + 60_000_000);
        let start = window
            .from()
            .saturating_add_micros(3_600_000_000)
            .max(reach);
        if start >= window.to() {
            return;
        }
        let lsn = {
            let mut late = lock(&self.late);
            if !late.landed.insert(partition) {
                return;
            }
            late.lsn += 1;
            late.lsn
        };
        let trace = self.ids.fetch_add(1, Ordering::SeqCst) + 1;
        let span = self.ids.fetch_add(1, Ordering::SeqCst) + 1;
        let spans = Spans {
            project: self.projects[1],
            trace: TraceId::from_bytes(u128::from(trace).to_be_bytes()),
            user: "late".to_owned(),
            identity: Identity {
                spans: true,
                mcp_user: true,
                issuer: None,
            },
            session: "late".to_owned(),
            start,
            received_at: self.clock.wall_now(),
            span_ids: vec![span.to_be_bytes()],
        };
        if !land(&self.ch, &spans, late_epoch(), lsn).await {
            return;
        }
        let mut ledger = lock(&self.ledger);
        ledger.stats.live_late_frames += 1;
        ledger.spans.push((spans, Fate::Shipped));
    }
}

#[async_trait]
impl TelemetryStore for Audited {
    async fn insert_block(
        &self,
        block: EncodedBlock,
        settings: InsertSettings,
    ) -> Result<(), InsertError> {
        self.ch.insert_block(block, settings).await
    }

    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError> {
        self.ch.execute(plan).await
    }

    async fn probe(&self, probe: BatchProbe) -> Result<bool, StoreError> {
        self.ch.probe(probe).await
    }

    async fn delete(&self, plan: DeletePlan) -> Result<MutationId, StoreError> {
        self.fenced(ActionKind::Delete, plan.table(), plan.partition());
        let deleted = matches!(plan.target(), DeleteTarget::Project(scope) if scope.project_id() == self.projects[2]);
        let purge = deleted && *plan.pred() == Pred::all();
        if purge && plan.table() == TableId::SpanCore {
            self.land_late(plan.partition()).await;
        }
        let (table_id, partition) = (plan.table(), plan.partition());
        let answer = self.ch.delete(plan).await;
        if deleted && answer.is_ok() {
            let at = self.clock.now().nanos();
            lock(&self.ledger).removals.push((table_id, partition, at));
        }
        answer
    }

    async fn drop_partition(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<(), StoreError> {
        self.fenced(ActionKind::Drop, table_id, partition);
        let for_a_request = self.drops_for_a_request(table_id, partition);
        if table_id == TableId::SpanCore && for_a_request {
            self.land_late(partition).await;
        }
        let answer = self.ch.drop_partition(table_id, partition).await;
        if answer.is_ok() {
            let at = self.clock.now().nanos();
            let mut ledger = lock(&self.ledger);
            ledger.removals.push((table_id, partition, at));
            ledger.stats.purge_drops += u64::from(for_a_request);
        }
        answer
    }

    async fn apply_deleted_mask(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<MutationId, StoreError> {
        self.fenced(ActionKind::Mask, table_id, partition);
        self.ch.apply_deleted_mask(table_id, partition).await
    }

    async fn mutation_status(&self, id: &MutationId) -> Result<MutationStatus, StoreError> {
        self.ch.mutation_status(id).await
    }

    async fn partition_projects(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        self.ch.partition_projects(table_id, partition).await
    }

    async fn masked_parts(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<u64, StoreError> {
        self.ch.masked_parts(table_id, partition).await
    }

    async fn partitions(&self, table_id: TableId) -> Result<Vec<PartitionId>, StoreError> {
        self.ch.partitions(table_id).await
    }

    async fn holds_project(
        &self,
        table_id: TableId,
        partition: Option<PartitionId>,
        project: ProjectId,
    ) -> Result<bool, StoreError> {
        self.ch.holds_project(table_id, partition, project).await
    }

    async fn apply_migration(&self, step: &MigrationStep) -> Result<(), StoreError> {
        self.ch.apply_migration(step).await
    }

    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        self.ch.detect().await
    }

    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        self.ch.dedup_selftest().await
    }

    async fn kill(&self, query_id: Uuid) -> Result<(), StoreError> {
        self.ch.kill(query_id).await
    }

    async fn health(&self) -> StoreHealth {
        self.ch.health().await
    }
}

/// A node that is up, as the workload reaches it.
#[derive(Clone)]
struct Up {
    service: Service,
    state: Arc<NodeState>,
    clock: Arc<dyn Clock>,
    /// Set once the node has registered: from then on it takes ingest and ships.
    ready: Arc<AtomicBool>,
}

/// The deletion of the third project, once it is made.
#[derive(Clone, Copy, Debug)]
struct Deleted {
    /// Until when, on the database host's monotonic clock in nanoseconds, nodes still take the project's frames.
    taken_until: u64,
}

/// What every node and the harness share.
#[derive(Clone)]
struct World {
    pg: PgModel,
    ch: ChModel,
    /// The database host: its task scope outlives every node.
    database: Env,
    ledger: Arc<Mutex<Ledger>>,
    logs: Arc<Vec<Arc<SimLog>>>,
    up: Arc<Mutex<Vec<Option<Up>>>>,
    /// The project whose retention is shortened, a project that keeps everything, and the project that is
    /// deleted.
    projects: [ProjectId; 3],
    deleted: Arc<Mutex<Option<Deleted>>>,
    config: LifecycleConfig,
    options: Options,
    ids: Arc<AtomicU64>,
    /// Whether reads of the project salts fail now and then: while the stores fail.
    salt_faults: Arc<AtomicBool>,
    salt_rng: Arc<dyn Rng>,
    late: Arc<Mutex<Late>>,
}

fn uuid(id: Uuid) -> Value {
    Value::Uuid(id)
}

/// The rows of one frame for one table, with their partition. A span table gets one row per span, every other
/// table one row; an aggregating table also gets the marker row that names its batch.
fn rows_of(
    table_id: TableId,
    spans: &Spans,
    epoch: WalEpoch,
    lsn: u64,
) -> Option<(Rows, PartitionId)> {
    let def = table(table_id);
    let data = if SPAN_TABLES.contains(&table_id) {
        spans.span_ids.len()
    } else {
        1
    };
    let marker = def.kind == TableKind::Aggregating;
    let total = data + usize::from(marker);
    let has = |name: &str| def.column(name).is_some();
    let mut rows = Rows::new(table_id, total).each("project_id", |row| {
        uuid(if row < data {
            spans.project.uuid()
        } else {
            Uuid::NIL
        })
    });
    let start = spans.start;
    for name in ["start_time", "target_time"] {
        if has(name) {
            rows = rows.all(name, Value::Time(start));
        }
    }
    for name in ["start_minute", "minute"] {
        if has(name) {
            rows = rows.all(name, Value::Time(start.floor_minute()));
        }
    }
    if has("hour") {
        let micros = start.micros();
        let hour = Timestamp::from_micros(micros - micros.rem_euclid(3_600_000_000));
        rows = rows.all("hour", Value::Time(hour));
    }
    for name in ["target_day", "day", "row_day"] {
        if has(name) {
            rows = rows.all(name, Value::Date(start.date()));
        }
    }
    if has("trace_id") {
        rows = rows.all("trace_id", Value::Bytes(spans.trace.to_bytes().to_vec()));
    }
    if has("trace_hash") {
        rows = rows.all(
            "trace_hash",
            Value::UInt(u64::from(spans.trace.trace_hash())),
        );
    }
    if has("span_id") {
        let ids = spans.span_ids.clone();
        rows = rows.each("span_id", move |row| {
            Value::Bytes(ids[row.min(ids.len() - 1)].to_vec())
        });
    }
    let mcp = MCP_TABLES.contains(&table_id);
    if has("user_id") {
        let shown = if mcp {
            spans.identity.mcp_user
        } else {
            spans.identity.spans
        };
        let user = if shown {
            spans.user.clone()
        } else {
            String::new()
        };
        rows = rows.all("user_id", Value::String(user));
    }
    if mcp && let Some(issuer) = spans.identity.issuer {
        let hash = salt(spans.project).subject_hash(issuer, &spans.user);
        rows = rows.all("user_subject_hash", Value::Bytes(hash.to_vec()));
        if has("user_issuer") {
            rows = rows.all("user_issuer", Value::String(issuer.to_owned()));
        }
    }
    if has("session_id") {
        rows = rows.all("session_id", Value::String(spans.session.clone()));
    }
    if has("received_at") {
        rows = rows.all("received_at", Value::Time(spans.received_at));
    }
    if marker {
        rows = rows
            .each("marker_epoch", |row| {
                uuid(if row < data { Uuid::NIL } else { epoch.uuid() })
            })
            .each("marker_batch_seq", |row| {
                Value::UInt(if row < data { 0 } else { lsn })
            });
    } else {
        rows = rows
            .all("wal_epoch", uuid(epoch.uuid()))
            .all("batch_seq", Value::UInt(lsn))
            .all("lsn", Value::UInt(lsn));
    }
    let partition = route(table_id, Granularity::Day, start).ok()?;
    Some((rows.sorted(), partition))
}

fn frame_tables() -> impl Iterator<Item = TableId> {
    SPAN_TABLES
        .into_iter()
        .chain(TRACE_TABLES)
        .chain(ROLLUPS)
        .chain(COUNTS)
}

impl World {
    fn violation(&self, text: String) {
        lock(&self.ledger).violations.push(text);
    }

    /// The model's stage B and shipper of one node, from the moment the node has registered: frames go to
    /// the store in log order, and the watermark follows.
    async fn ship(self, n: usize, env: Env, rng: Arc<dyn Rng>, faulty_until: u64) {
        let log = Arc::clone(&self.logs[n]);
        // Recovery consults the deletion requests before anything is shipped (contract 13.3).
        let mut known: BTreeMap<Uuid, DeletionRequest> = self
            .pg
            .deletion_requests()
            .into_iter()
            .map(|request| (request.id.uuid(), request))
            .collect();
        loop {
            env.clock.sleep(millis(rng.as_ref(), 100, 1_500)).await;
            if env.clock.now().nanos() < faulty_until && below(rng.as_ref(), 25) == 0 {
                lock(&self.ledger).stats.stalls += 1;
                env.clock.sleep(millis(rng.as_ref(), 30_000, 90_000)).await;
            }
            let Some((lsn, frame)) = lock(&log.state).unshipped.front().cloned() else {
                continue;
            };
            let spans = match frame {
                Frame::Tombstone(request) => {
                    known.insert(request.id.uuid(), request);
                    self.resolve(&log, lsn);
                    continue;
                }
                Frame::Spans(spans) => spans,
            };
            let index = self.span_index(&spans);
            if known.values().any(|request| spans.selected_by(request)) {
                let mut ledger = lock(&self.ledger);
                ledger.stats.stage_b_drops += spans.span_ids.len() as u64;
                if let Some(index) = index {
                    ledger.spans[index].1 = Fate::Dropped;
                }
                drop(ledger);
                self.resolve(&log, lsn);
                continue;
            }
            // A deletion by user also selects MCP rows by their own identity: stage B drops those rows and
            // ships the rest of the frame.
            let mcp_dropped = known.values().any(|request| spans.mcp_selected_by(request));
            let mut whole = true;
            for table_id in frame_tables() {
                if lock(&log.state).front_done.contains(&table_id)
                    || (mcp_dropped && MCP_TABLES.contains(&table_id))
                {
                    continue;
                }
                let Some((rows, partition)) = rows_of(table_id, &spans, log.epoch, lsn) else {
                    self.violation(format!("the model could not route a frame of {table_id:?}"));
                    continue;
                };
                let block = match rows.encode() {
                    Ok(block) => block,
                    Err(error) => {
                        self.violation(format!(
                            "the model could not build a block of {}: {error:#}",
                            table_id.name()
                        ));
                        continue;
                    }
                };
                let token = match BatchToken::new(
                    log.epoch,
                    Lsn::new(lsn),
                    Lsn::new(lsn),
                    table_id,
                    partition,
                ) {
                    Ok(token) => token,
                    Err(error) => {
                        self.violation(format!("the model could not name a batch: {error}"));
                        continue;
                    }
                };
                let settings = InsertSettings::new(token, StoreFlavour::SingleNode);
                match self.ch.insert_block(block, settings).await {
                    Ok(()) => {
                        lock(&log.state).front_done.insert(table_id);
                    }
                    Err(_) => {
                        whole = false;
                        break;
                    }
                }
            }
            if whole {
                let mut ledger = lock(&self.ledger);
                ledger.stats.shipped += spans.span_ids.len() as u64;
                let apart = !spans.identity.spans
                    && (spans.identity.mcp_user || spans.identity.issuer.is_some());
                if mcp_dropped {
                    ledger.stats.mcp_drops += 1;
                } else if apart {
                    ledger.stats.mcp_identity_frames += 1;
                }
                if let Some(index) = index {
                    ledger.spans[index].1 = Fate::Shipped;
                }
                drop(ledger);
                self.resolve(&log, lsn);
            }
        }
    }

    fn span_index(&self, spans: &Spans) -> Option<usize> {
        lock(&self.ledger)
            .spans
            .iter()
            .position(|(known, _)| known.span_ids == spans.span_ids)
    }

    /// The front frame is resolved: the checkpoint watermark passes it.
    fn resolve(&self, log: &SimLog, lsn: u64) {
        let mut state = lock(&log.state);
        state.unshipped.pop_front();
        state.front_done.clear();
        state.watermark = lsn;
    }

    fn source(&self) -> LeaderSource {
        let stores = LeaderStores {
            telemetry: Arc::new(Audited {
                ch: self.ch.clone(),
                pg: self.pg.clone(),
                ledger: Arc::clone(&self.ledger),
                projects: self.projects,
                ids: Arc::clone(&self.ids),
                late: Arc::clone(&self.late),
                clock: Arc::clone(&self.database.clock),
            }),
            subjects: Arc::new(SimSubjects {
                rng: Arc::clone(&self.salt_rng),
                faulty: Arc::clone(&self.salt_faults),
                ledger: Arc::clone(&self.ledger),
            }),
            exports: None,
        };
        Arc::new(move || Some(stores.clone()))
    }

    fn ready(&self) -> Vec<(usize, Up)> {
        lock(&self.up)
            .iter()
            .enumerate()
            .filter_map(|(n, up)| up.clone().map(|up| (n, up)))
            .filter(|(_, up)| up.ready.load(Ordering::SeqCst))
            .collect()
    }

    /// The deletion of the third project, once it is made.
    fn deleted(&self) -> Option<Deleted> {
        *lock(&self.deleted)
    }
}

/// One node across its boots.
async fn node(sim: &Sim, world: World, n: usize, faulty_until: u64, end: u64) {
    let rng = sim.rng(&format!("lifecycle-sim/node-{n}"));
    // Up to 2 s either way.
    let skew = i64::try_from(below(rng.as_ref(), 4_000_001)).unwrap_or(0) - 2_000_000;
    let disk = sim.disk(&format!("lifecycle-node-{n}"));
    let clock = Arc::clone(&world.database.clock);
    let mut boot = 0;
    loop {
        boot += 1;
        let name = format!("lifecycle-node-{n}#{boot}");
        let host = sim.host(&name, &disk, skew);
        let log = Arc::clone(&world.logs[n]);
        let state = Arc::new(NodeState::new(log.clone(), "sim", ""));
        let control: Arc<dyn ControlStore> = Arc::new(world.pg.clone());
        let ready = Arc::new(AtomicBool::new(false));
        lock(&world.up)[n] = Some(Up {
            service: Service::new(&host.env, Arc::clone(&control), Arc::clone(&state)),
            state: Arc::clone(&state),
            clock: Arc::clone(&host.env.clock),
            ready: Arc::clone(&ready),
        });
        let mut registry = Registry::new();
        registry.register(node_job(state, &world.config));
        for job in leader_jobs(&world.source(), world.config) {
            registry.register(job);
        }
        let mut running = registry.start(&host.env, Some(control), &name);
        lock(&world.ledger).stats.boots += 1;
        // The node registers, then its stage B starts.
        let stage_b = {
            let (world, env, rng) = (world.clone(), host.env.clone(), rng.fork(&name));
            let live_within = world.config.node_every;
            async move {
                loop {
                    env.clock.sleep(Duration::from_millis(500)).await;
                    let live = world.pg.lifecycle().live_nodes(live_within).await;
                    if live.is_ok_and(|nodes| nodes.iter().any(|node| node.wal_epoch == log.epoch))
                    {
                        break;
                    }
                }
                ready.store(true, Ordering::SeqCst);
                world.ship(n, env, rng, faulty_until).await;
            }
        };
        drop(host.env.spawn("stage-b", stage_b));
        let pause = millis(rng.as_ref(), 20_000, 120_000);
        let wake = clock.now().saturating_add(pause).nanos();
        if !world.options.crashes || wake >= faulty_until {
            clock.sleep_until(Instant::from_nanos(end)).await;
            lock(&world.up)[n] = None;
            host.spawner.request_shutdown();
            if clock
                .timeout(Duration::from_secs(60), running.join())
                .await
                .is_err()
            {
                world.violation(format!("{name} did not stop within 60 s of its shutdown"));
            }
            host.stop().await;
            return;
        }
        clock.sleep(pause).await;
        lock(&world.up)[n] = None;
        host.crash().await;
        let down = if below(rng.as_ref(), 4) == 0 {
            lock(&world.ledger).stats.long_downtimes += 1;
            millis(rng.as_ref(), 70_000, 150_000)
        } else {
            millis(rng.as_ref(), 0, 10_000)
        };
        lock(&world.ledger).stats.crashes += 1;
        clock.sleep(down).await;
    }
}

fn pg_faults(on: bool) -> PgFaults {
    if on {
        PgFaults {
            unavailable_ppm: 10_000,
            commit_unknown_ppm: 20_000,
            spurious_serialization_ppm: 10_000,
            slow_commit_ppm: 20_000,
            slow_commit_micros: (1_000, 1_000_000),
            failover_ppm: 300,
            failover_micros: (200_000, 6_000_000),
            latency_micros: (100, 40_000),
            ..PgFaults::default()
        }
    } else {
        PgFaults::default()
    }
}

fn ch_faults(on: bool) -> ChFaults {
    if on {
        ChFaults {
            read_error_ppm: 30_000,
            latency_micros: (100, 50_000),
            ..ChFaults::default()
        }
    } else {
        ChFaults::default()
    }
}

/// Fails the stores and steals the lease at random moments until `until`.
async fn chaos(world: World, rng: Arc<dyn Rng>, until: u64) {
    let clock = &world.database.clock;
    let mut thieves = 0;
    loop {
        clock.sleep(millis(rng.as_ref(), 5_000, 30_000)).await;
        if clock.now().nanos() >= until {
            return;
        }
        match below(rng.as_ref(), 3) {
            0 if world.options.store_faults => {
                world.pg.fail_over(millis(rng.as_ref(), 200, 8_000));
                lock(&world.ledger).stats.failovers += 1;
            }
            1 if world.options.store_faults => {
                world.ch.start_outage(millis(rng.as_ref(), 2_000, 20_000));
                lock(&world.ledger).stats.outages += 1;
            }
            2 if world.options.steals => {
                thieves += 1;
                let holder = format!("thief#{thieves}");
                let Some(lease) = world
                    .pg
                    .steal_lease(LeaseName::Lifecycle, &holder, LEASE_TTL)
                else {
                    continue;
                };
                lock(&world.ledger).stats.steals += 1;
                if below(rng.as_ref(), 2) == 0 {
                    let _released = world.pg.leases().release(&lease).await;
                }
            }
            _ => {}
        }
    }
}

/// A trace the workload has made.
#[derive(Clone)]
struct Made {
    project: ProjectId,
    trace: TraceId,
    user: String,
    identity: Identity,
    session: String,
    start: Timestamp,
}

/// Where a new trace's user shows. Half the traces name the user everywhere; the others show it only on the
/// MCP rows, by `user_id` or by subject hash, or on the spans with the MCP rows hashed.
fn identity(rng: &dyn Rng) -> Identity {
    let issuer = Some(ISSUERS[below(rng, ISSUERS.len() as u64) as usize]);
    let (spans, mcp_user, issuer) = match below(rng, 10) {
        0..=4 => (true, true, None),
        5 | 6 => (false, false, issuer),
        7 => (false, true, None),
        _ => (true, false, issuer),
    };
    Identity {
        spans,
        mcp_user,
        issuer,
    }
}

/// Appends frames of spans to the nodes that are up until `until`.
async fn ingest(world: World, rng: Arc<dyn Rng>, today: u32, until: u64) {
    let clock = &world.database.clock;
    let mut traces: Vec<Made> = Vec::new();
    let day_start = |back: u32| Timestamp::from_micros(i64::from(today - back) * DAY);
    loop {
        clock.sleep(millis(rng.as_ref(), 300, 1_500)).await;
        if clock.now().nanos() >= until {
            return;
        }
        let ready = world.ready();
        if ready.is_empty() {
            continue;
        }
        let (n, up) = &ready[below(rng.as_ref(), ready.len() as u64) as usize];
        let which = below(rng.as_ref(), 3) as usize;
        let project = world.projects[which];
        // A deleted project's frames are taken only while a node's key snapshot may lack the revocation.
        let late = match world.deleted() {
            Some(deleted) if which == 2 => {
                if clock.now().nanos() >= deleted.taken_until {
                    continue;
                }
                true
            }
            _ => false,
        };
        let shortened = which == 0 && lock(&world.ledger).retention_set;
        let reuse: Vec<&Made> = traces
            .iter()
            .filter(|made| made.project == project)
            // An expired span is refused at ingest (contract 1.2), so an old trace gets no new span once
            // its project's retention has been shortened.
            .filter(|made| !shortened || made.start >= day_start(1))
            .collect();
        let made = if !reuse.is_empty() && below(rng.as_ref(), 10) < 4 {
            reuse[below(rng.as_ref(), reuse.len() as u64) as usize].clone()
        } else {
            let id = world.ids.fetch_add(1, Ordering::SeqCst);
            // Fresh days are today and yesterday. The first project's old days reach back nine days, the
            // second's only the two oldest, so some old partitions hold one project and some both. The deleted
            // project's reach back eleven days, so two old partitions hold its rows alone.
            let back = match (below(rng.as_ref(), 2), which, shortened) {
                (0, _, _) | (_, 0, true) => below(rng.as_ref(), 2),
                (_, 0, false) => 6 + below(rng.as_ref(), 4),
                (_, 1, _) => 8 + below(rng.as_ref(), 2),
                _ => 7 + below(rng.as_ref(), 5),
            };
            let back = u32::try_from(back).unwrap_or(0);
            let minute = i64::try_from(below(rng.as_ref(), 600)).unwrap_or(0);
            let made = Made {
                project,
                trace: TraceId::from_bytes(u128::from(id + 1).to_be_bytes()),
                user: format!("u{}", below(rng.as_ref(), 5)),
                identity: identity(rng.as_ref()),
                session: format!("s{}", below(rng.as_ref(), 7)),
                start: day_start(back).saturating_add_micros(minute * 60_000_000),
            };
            traces.push(made.clone());
            made
        };
        let count = 1 + below(rng.as_ref(), 3);
        let span_ids = (0..count)
            .map(|_| (world.ids.fetch_add(1, Ordering::SeqCst) + 1).to_be_bytes())
            .collect();
        let spans = Spans {
            project: made.project,
            trace: made.trace,
            user: made.user,
            identity: made.identity,
            session: made.session,
            start: made.start,
            received_at: up.clock.wall_now(),
            span_ids,
        };
        let mut ledger = lock(&world.ledger);
        ledger.stats.frames += 1;
        ledger.stats.late_frames += u64::from(late);
        ledger.spans.push((spans.clone(), Fate::Unshipped));
        drop(ledger);
        world.logs[*n].append(Frame::Spans(spans));
    }
}

/// Deletes the third project at `at`, as `DELETE projects/{id}` does: in one transaction its row is marked deleted
/// and its project deletion request made (contract 13.4). A call that fails is sent again; one whose commit has an
/// unknown outcome is decided by what is stored.
async fn delete_project(world: World, rng: Arc<dyn Rng>, at: u64) {
    let clock = &world.database.clock;
    clock.sleep_until(Instant::from_nanos(at)).await;
    let id = world.projects[2];
    for _ in 0..60 {
        let ready = world.ready();
        let Some((_, up)) = ready.get(below(rng.as_ref(), ready.len() as u64) as usize) else {
            clock.sleep(Duration::from_secs(1)).await;
            continue;
        };
        let now = up.clock.wall_now();
        let request = DeletionRequest::project(
            DeletionRequestId::from_uuid(Uuid::from_v7_parts(
                u64::try_from(now.micros() / 1_000).unwrap_or(0),
                u128::from(world.ids.fetch_add(1, Ordering::SeqCst)),
            )),
            id,
            now,
            "user:sim".to_owned(),
        );
        let row = request;
        // Whatever the answer, what is stored decides below: a commit with an unknown outcome may have applied.
        let _answer = transaction(&world.pg, move |tx| {
            Box::pin(async move {
                let Some(project) = tx.projects().get(id).await? else {
                    return Ok(false);
                };
                if project.deleted_at.is_some() {
                    return Ok(false);
                }
                let deleted = Project {
                    deleted_at: Some(row.requested_at),
                    version: project.version + 1,
                    ..project.clone()
                };
                if !tx
                    .projects()
                    .update_if_version(&deleted, project.version)
                    .await?
                {
                    return Err(TxError::SerializationFailure);
                }
                tx.lifecycle().insert_request(&row).await?;
                Ok(true)
            })
        })
        .await;
        let stored =
            world.pg.deletion_requests().iter().any(|stored| {
                stored.project_id == id && stored.selector == DeletionSelector::Project
            });
        if stored {
            let window = Duration::from_millis(below(rng.as_ref(), REVOCATION_WINDOW_MILLIS + 1));
            *lock(&world.deleted) = Some(Deleted {
                taken_until: clock.now().saturating_add(window).nanos(),
            });
            lock(&world.ledger).stats.by_project += 1;
            return;
        }
        clock.sleep(Duration::from_millis(700)).await;
    }
    world.violation("the third project could not be deleted".to_owned());
}

/// The deleted project's history, which it sent long before the run, each frame shipped an hour after its spans
/// started: frames whose spans start in [`HISTORY_DAYS`], partitions that no project can write to any more.
async fn history(world: &World, rng: &dyn Rng, today: u32) {
    for n in 0..6_u64 {
        let back = HISTORY_DAYS.start() + u32::try_from(below(rng, 11)).unwrap_or(0);
        let minute = i64::try_from(below(rng, 1_200)).unwrap_or(0);
        let start = Timestamp::from_micros(i64::from(today - back) * DAY + minute * 60_000_000);
        let trace = world.ids.fetch_add(1, Ordering::SeqCst) + 1;
        let spans = Spans {
            project: world.projects[2],
            trace: TraceId::from_bytes(u128::from(trace).to_be_bytes()),
            user: "early".to_owned(),
            identity: Identity {
                spans: true,
                mcp_user: true,
                issuer: None,
            },
            session: "early".to_owned(),
            start,
            received_at: start.saturating_add_micros(3_600_000_000),
            span_ids: vec![(world.ids.fetch_add(1, Ordering::SeqCst) + 1).to_be_bytes()],
        };
        if land(&world.ch, &spans, history_epoch(), n + 1).await {
            let mut ledger = lock(&world.ledger);
            ledger.stats.history_frames += 1;
            ledger.spans.push((spans, Fate::Shipped));
        } else {
            world.violation("the deleted project's history could not be stored".to_owned());
        }
    }
}

/// Lands frames of the deleted project once its request has completed and the faulty phase is over, each a minute or
/// less after the one before, as nodes that were behind or cut off ship them: one a node took just after the deletion
/// while its key snapshot lagged, one a node without a heartbeat took before the deletion, one a node cut off from
/// Postgres took under `fail_open` twenty minutes after it, and one from the WAL of a node back after weeks, into a
/// partition of the project's history. A request that completes too late for a sweep and a mask cycle to follow
/// before the end gets none.
async fn after_completion(
    world: World,
    rng: Arc<dyn Rng>,
    today: u32,
    faulty_until: u64,
    end: u64,
) {
    let clock = &world.database.clock;
    let deleted = world.projects[2];
    let latest = end.saturating_sub(nanos(
        SWEEP_EVERY + world.config.mask_cycle + Duration::from_secs(600),
    ));
    let requested_at = loop {
        clock.sleep(Duration::from_secs(5)).await;
        let now = clock.now().nanos();
        if now >= latest {
            return;
        }
        let completed = world.pg.deletion_requests().into_iter().find(|request| {
            request.project_id == deleted
                && request.selector == DeletionSelector::Project
                && request.state == DeletionState::Completed
        });
        if let Some(request) = completed
            && now >= faulty_until
        {
            break request.requested_at;
        }
    };
    let day_start = |back: u32| Timestamp::from_micros(i64::from(today - back) * DAY);
    let minutes = |n: i64| n * 60_000_000;
    let old = day_start(*HISTORY_DAYS.start() + 5).saturating_add_micros(minutes(90));
    let kinds = [
        (requested_at.saturating_add_micros(2_000_000), day_start(0)),
        (
            requested_at.saturating_add_micros(-minutes(1)),
            day_start(10),
        ),
        (
            requested_at.saturating_add_micros(minutes(20)),
            day_start(1),
        ),
        (old.saturating_add_micros(minutes(60)), old),
    ];
    for (n, (received_at, day)) in kinds.into_iter().enumerate() {
        clock.sleep(millis(rng.as_ref(), 0, 60_000)).await;
        let minute = i64::try_from(below(rng.as_ref(), 300)).unwrap_or(0);
        let start = if day == old {
            day
        } else {
            day.saturating_add_micros(minutes(minute))
        };
        let trace = world.ids.fetch_add(1, Ordering::SeqCst) + 1;
        let spans = Spans {
            project: deleted,
            trace: TraceId::from_bytes(u128::from(trace).to_be_bytes()),
            user: "behind".to_owned(),
            identity: Identity {
                spans: true,
                mcp_user: true,
                issuer: None,
            },
            session: "behind".to_owned(),
            start,
            received_at,
            span_ids: vec![(world.ids.fetch_add(1, Ordering::SeqCst) + 1).to_be_bytes()],
        };
        let lsn = 100 + n as u64;
        if !land(&world.ch, &spans, history_epoch(), lsn).await {
            world.violation("a frame of the deleted project could not be landed".to_owned());
            continue;
        }
        let landed = clock.now().nanos();
        let mut ledger = lock(&world.ledger);
        ledger.stats.after_completion += 1;
        ledger.after_completion.push((spans.clone(), landed));
        ledger.spans.push((spans, Fate::Shipped));
    }
}

/// I9 for the rows of the deleted project that landed after its request completed: a delete of the project's rows
/// or a drop of their `span_core` partition reached the store within one sweep interval and three runs of the
/// executor of their landing. That they are gone at the end is the rest of I9.
fn check_swept(world: &World, ledger: &mut Ledger) {
    let bound = nanos(SWEEP_EVERY + world.config.deletion_every * 3);
    let landed = std::mem::take(&mut ledger.after_completion);
    for (spans, at) in &landed {
        let Ok(partition) = route(TableId::SpanCore, Granularity::Day, spans.start) else {
            continue;
        };
        let removed = ledger
            .removals
            .iter()
            .filter(|(table_id, held, when)| {
                *table_id == TableId::SpanCore && *held == partition && *when >= *at
            })
            .map(|(_, _, when)| *when)
            .min();
        match removed {
            Some(when) if when - at <= bound => ledger.stats.swept_in_time += 1,
            _ => ledger.violations.push(format!(
                "I9 (eventual): rows of the deleted project that landed in span_core partition {partition} after \
                 its request completed were not removed within one sweep interval: removed {removed:?}, landed \
                 at {at}"
            )),
        }
    }
    ledger.after_completion = landed;
}

/// L2: the accepting node hides the request's rows at once.
fn check_hidden(world: &World, up: &Up, request: &DeletionRequest) {
    let hidden = up.state.pending().hidden(request.project_id);
    let held = up
        .state
        .pending()
        .of(request.project_id)
        .iter()
        .any(|pending| pending.request_id == request.id);
    if !held {
        world.violation(format!(
            "L2: request {} was accepted and is not in the accepting node's pending-deletion set",
            request.id
        ));
        return;
    }
    let Ok(scope) = scope(request.project_id) else {
        return;
    };
    let selector = match &request.selector {
        DeletionSelector::Traces(ids) => TraceSelector::Traces(ids),
        DeletionSelector::User { user_id, .. } => TraceSelector::User(user_id),
        DeletionSelector::Session(session_id) => TraceSelector::Session(session_id),
        // A deleted project's reads are refused before a plan is built.
        DeletionSelector::Project => return,
    };
    // Exactly the spans the request selects, read as a typed read would read them.
    let Ok(plan) = deletion_traces(scope, selector, request.requested_at, None, 1_000) else {
        return;
    };
    match world.ch.read(&plan.hiding(&hidden)) {
        Ok(answer) => {
            lock(&world.ledger).stats.hidden_reads += 1;
            let rows = answer.into_rows();
            if !rows.is_empty() {
                world.violation(format!(
                    "L2: a read with the pending-deletion set returned {} traces that request {} selects",
                    rows.len(),
                    request.id
                ));
            }
        }
        Err(error) => world.violation(format!("the model refused the hidden read: {error}")),
    }
}

/// Asks for deletions and shortens the first project's retention until `until`. A deletion by user names one of
/// the two token issuers two times in three: it then reaches MCP rows hashed under that issuer and keeps those
/// hashed under the other.
async fn requests(world: World, rng: Arc<dyn Rng>, until: u64) {
    let clock = &world.database.clock;
    let issuers = rng.fork("issuers");
    let retention_at = clock
        .now()
        .saturating_add(millis(rng.as_ref(), 120_000, 420_000))
        .nanos();
    let mut asked = 0_u64;
    loop {
        clock.sleep(millis(rng.as_ref(), 15_000, 50_000)).await;
        let now = clock.now().nanos();
        if now >= until {
            return;
        }
        let ready = world.ready();
        if ready.is_empty() {
            continue;
        }
        let (_, up) = &ready[below(rng.as_ref(), ready.len() as u64) as usize];
        if now >= retention_at && !lock(&world.ledger).retention_set {
            let Ok(scope) = scope(world.projects[0]) else {
                continue;
            };
            let new = RetentionSettings {
                retention_days: Some(RETENTION_DAYS),
                payload_retention_days: None,
                mcp_content_retention_days: None,
            };
            if let Ok(current) = up.service.retention(scope).await {
                // No frame of an old trace is appended between the change and its acknowledgement.
                lock(&world.ledger).retention_set = true;
                if current.settings == new
                    || up
                        .service
                        .set_retention(scope, current.version, new)
                        .await
                        .is_ok()
                {
                    lock(&world.ledger).stats.retention_changes += 1;
                } else {
                    let stored = world.pg.projects().get(world.projects[0]).await;
                    let applied = stored
                        .ok()
                        .flatten()
                        .is_some_and(|project| project.retention_days == Some(RETENTION_DAYS));
                    lock(&world.ledger).retention_set = applied;
                }
            }
            continue;
        }
        asked += 1;
        let which = below(rng.as_ref(), 3) as usize;
        // A deleted project is not visible to the API any more, so no request names it.
        if which == 2 && world.deleted().is_some() {
            continue;
        }
        let project = world.projects[which];
        let made: Vec<Spans> = lock(&world.ledger)
            .spans
            .iter()
            .filter(|(spans, _)| spans.project == project)
            .map(|(spans, _)| spans.clone())
            .collect();
        if made.is_empty() {
            continue;
        }
        let pick = |rng: &dyn Rng| made[below(rng, made.len() as u64) as usize].clone();
        let selector = match below(rng.as_ref(), 3) {
            0 => DeletionSelector::Traces(
                (0..=below(rng.as_ref(), 3))
                    .map(|_| pick(rng.as_ref()).trace)
                    .collect(),
            ),
            1 => DeletionSelector::User {
                user_id: pick(rng.as_ref()).user,
                issuer: match below(issuers.as_ref(), 3) {
                    0 => None,
                    n => ISSUERS.get(n as usize - 1).map(|&issuer| issuer.to_owned()),
                },
            },
            _ => DeletionSelector::Session(pick(rng.as_ref()).session),
        };
        let Ok(scope) = scope(project) else {
            continue;
        };
        let new = NewDeletion {
            selector,
            requested_by: "user:sim".to_owned(),
            idempotency: Some((format!("sim-{asked}"), format!("hash-{asked}"))),
        };
        // A caller whose request failed sends it again under the same key.
        for attempt in 0..6 {
            match up.service.request_deletion(scope, new.clone()).await {
                Ok(accepted) => {
                    let mut ledger = lock(&world.ledger);
                    match accepted.request.selector {
                        DeletionSelector::Traces(_) => ledger.stats.by_trace += 1,
                        DeletionSelector::User { ref issuer, .. } => {
                            ledger.stats.by_user += 1;
                            ledger.stats.by_user_issuer += u64::from(issuer.is_some());
                        }
                        DeletionSelector::Session(_) => ledger.stats.by_session += 1,
                        // The service refuses a project selector: deleting the project makes that request.
                        DeletionSelector::Project => {}
                    }
                    if attempt > 0 {
                        ledger.stats.request_retries += 1;
                    }
                    drop(ledger);
                    check_hidden(&world, up, &accepted.request);
                    break;
                }
                Err(_) => clock.sleep(Duration::from_millis(700)).await,
            }
        }
    }
}

/// What a physical row says about itself, by the names of its table's columns.
struct Held<'a> {
    table_id: TableId,
    cells: &'a [Value],
}

impl Held<'_> {
    fn cell(&self, name: &str) -> Option<&Value> {
        let at = table(self.table_id)
            .columns
            .iter()
            .position(|column| column.name == name)?;
        self.cells.get(at)
    }

    fn project(&self) -> Option<ProjectId> {
        match self.cell("project_id")? {
            Value::Uuid(id) if !id.is_nil() => Some(ProjectId::from_uuid(*id)),
            _ => None,
        }
    }

    fn trace(&self) -> Option<TraceId> {
        match self.cell("trace_id")? {
            Value::Bytes(bytes) => <[u8; 16]>::try_from(bytes.as_slice())
                .ok()
                .map(TraceId::from_bytes),
            _ => None,
        }
    }

    fn text(&self, name: &str) -> Option<&str> {
        match self.cell(name)? {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    fn bytes(&self, name: &str) -> Option<&[u8]> {
        match self.cell(name)? {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    fn received_at(&self) -> Option<Timestamp> {
        match self.cell("received_at")? {
            Value::Time(at) => Some(*at),
            _ => None,
        }
    }

    /// The instant retention is evaluated on.
    fn time(&self) -> Option<Timestamp> {
        match self.cell(table(self.table_id).retention.time_column)? {
            Value::Time(at) => Some(*at),
            Value::Date(date) => Some(date.start()),
            _ => None,
        }
    }
}

/// Whether a completed request must have removed the row, a row of the request's project (contract 13.4).
fn selected(request: &DeletionRequest, row: &Held<'_>, spans: &[(Spans, Fate)]) -> bool {
    // A project deletion removes every row of its project, whatever its arrival: the project takes no new data.
    if request.selector == DeletionSelector::Project {
        return true;
    }
    // The spans name a user only when the trace shows its user on its spans.
    let theirs = |spans: &Spans| match &request.selector {
        DeletionSelector::Traces(ids) => ids.contains(&spans.trace),
        DeletionSelector::User { user_id, .. } => spans.identity.spans && *user_id == spans.user,
        DeletionSelector::Session(session_id) => *session_id == spans.session,
        DeletionSelector::Project => true,
    };
    let of_project =
        |spans: &&(Spans, Fate)| spans.0.project == request.project_id && theirs(&spans.0);
    let before = match row.received_at() {
        Some(at) => at < request.requested_at,
        // A table merged by key has no arrival time. Its row is the request's unless data that arrived
        // after the request shares the key, which a deletion keeps.
        None => !spans
            .iter()
            .filter(of_project)
            .any(|(spans, _)| spans.received_at >= request.requested_at),
    };
    if !before {
        return false;
    }
    let by_trace = SPAN_TABLES.contains(&row.table_id) || TRACE_TABLES.contains(&row.table_id);
    let resolved = || {
        row.trace().is_some_and(|trace| {
            spans
                .iter()
                .filter(of_project)
                .any(|(spans, _)| spans.trace == trace && spans.received_at < request.requested_at)
        })
    };
    match &request.selector {
        DeletionSelector::Traces(ids) => {
            by_trace && row.trace().is_some_and(|trace| ids.contains(&trace))
        }
        DeletionSelector::User { user_id, issuer } => {
            let own = SPAN_TABLES.contains(&row.table_id) || row.table_id == TableId::UserRollup;
            // An MCP row also goes by its own user: its `user_id`, or the hash of the user under the issuer the
            // request names, or under any issuer when it names none (contract 13.3), whether or not a span of its
            // trace names the user.
            let salt = salt(request.project_id);
            let issuers: Vec<&str> = match issuer {
                Some(named) => vec![named.as_str()],
                None => ISSUERS.to_vec(),
            };
            let by_identity = MCP_TABLES.contains(&row.table_id)
                && (row.text("user_id") == Some(user_id.as_str())
                    || row.bytes("user_subject_hash").is_some_and(|hash| {
                        issuers
                            .iter()
                            .any(|issuer| salt.subject_hash(issuer, user_id).as_slice() == hash)
                    }));
            (own && row.text("user_id") == Some(user_id.as_str()))
                || by_identity
                || (TRACE_TABLES.contains(&row.table_id) && resolved())
        }
        DeletionSelector::Session(session_id) => {
            let own = SPAN_TABLES.contains(&row.table_id) || row.table_id == TableId::SessionRollup;
            (own && row.text("session_id") == Some(session_id.as_str()))
                || (TRACE_TABLES.contains(&row.table_id) && resolved())
        }
        DeletionSelector::Project => true,
    }
}

/// The request as it would be if it named no issuer, for a deletion by user that names one.
fn any_issuer(request: &DeletionRequest) -> Option<DeletionRequest> {
    match &request.selector {
        DeletionSelector::User {
            user_id,
            issuer: Some(_),
        } => Some(DeletionRequest {
            selector: DeletionSelector::User {
                user_id: user_id.clone(),
                issuer: None,
            },
            ..request.clone()
        }),
        _ => None,
    }
}

/// L3 and I9 over what the stores hold once every node has stopped.
fn audit(world: &World, ledger: &mut Ledger, now: Timestamp) {
    check_swept(world, ledger);
    ledger.violations.extend(
        world
            .ch
            .violations()
            .into_iter()
            .map(|violation| format!("the store model: {violation}")),
    );
    for (n, log) in world.logs.iter().enumerate() {
        let left = lock(&log.state).unshipped.len();
        if left > 0 {
            ledger.violations.push(format!(
                "the run is not quiescent: node {n} still has {left} frames to ship"
            ));
        }
    }
    let requests = world.pg.deletion_requests();
    for request in &requests {
        if request.state == DeletionState::Completed {
            ledger.stats.completed += 1;
        } else {
            ledger.violations.push(format!(
                "L3: request {} ({:?}) is {:?} at the end: {:?}",
                request.id,
                request.selector.kind(),
                request.state,
                request.error
            ));
        }
    }
    for action in world.pg.lifecycle_actions() {
        if action.state != ActionState::Verified {
            ledger.violations.push(format!(
                "L3: the {:?} {} on {} partition {} is {:?} at the end",
                action.kind,
                action.key,
                action.table.name(),
                action.partition,
                action.state
            ));
        }
    }
    // Rows this old are expired whatever the hour of the day; the workload's old rows are older still.
    let today = day_of(now.micros()).unwrap_or(0);
    let expired_before =
        Timestamp::from_micros(i64::from(today.saturating_sub(RETENTION_DAYS + 2)) * DAY);
    let kept: BTreeSet<TableId> = retention_tables().collect();
    let mut visible_spans: BTreeSet<Vec<u8>> = BTreeSet::new();
    for table_id in frame_tables() {
        for (partition, masked, cells) in world.ch.held_rows(table_id) {
            let row = Held {
                table_id,
                cells: &cells,
            };
            let Some(project) = row.project() else {
                continue;
            };
            ledger.stats.rows_checked += 1;
            if table_id == TableId::SpanCore
                && !masked
                && let Some(Value::Bytes(id)) = row.cell("span_id")
            {
                visible_spans.insert(id.clone());
            }
            let state = if masked { "masked" } else { "visible" };
            for request in requests.iter().filter(|r| r.project_id == project) {
                if request.state == DeletionState::Completed
                    && selected(request, &row, &ledger.spans)
                {
                    ledger.violations.push(format!(
                        "I9: {} partition {partition} still holds a {state} row that the completed \
                         request {} ({:?}) selects",
                        table_id.name(),
                        request.id,
                        request.selector
                    ));
                }
            }
            let expired = ledger.retention_set
                && project == world.projects[0]
                && kept.contains(&table_id)
                && row.time().is_some_and(|time| time < expired_before);
            if expired {
                ledger.violations.push(format!(
                    "I9: {} partition {partition} still holds a {state} row that retention expired",
                    table_id.name()
                ));
            }
        }
    }
    // Nothing else went: a span that no request selects and that retention keeps is still there. Stage B drops
    // only what a request selects at its point in time; a completed project deletion also removed what its
    // project received after it.
    for (spans, fate) in &ledger.spans {
        let selected = requests.iter().any(|request| spans.selected_by(request));
        let wanted = selected || requests.iter().any(|request| spans.purged_by(request));
        let expired = ledger.retention_set
            && spans.project == world.projects[0]
            && spans.start < expired_before;
        match fate {
            Fate::Dropped if !selected => ledger.violations.push(format!(
                "the model's stage B dropped spans of trace {} that no request selects",
                spans.trace
            )),
            Fate::Shipped if !wanted && !expired => {
                for id in &spans.span_ids {
                    if !visible_spans.contains(id.as_slice()) {
                        ledger.violations.push(format!(
                            "I9: a span of trace {} that no request selects and retention keeps is gone",
                            spans.trace
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    // Nor did an MCP row go that no request selects and that retention keeps: a deletion by user that names an
    // issuer keeps the rows hashed under the other one. Every row that ever landed is checked.
    let mcp = table(TableId::McpOps);
    let key = |row: &Held<'_>| Some((row.trace()?, row.bytes("span_id")?.to_vec()));
    let visible_ops: BTreeSet<(TraceId, Vec<u8>)> = world
        .ch
        .held_rows(TableId::McpOps)
        .iter()
        .filter(|(_, masked, _)| !masked)
        .filter_map(|(_, _, cells)| {
            key(&Held {
                table_id: TableId::McpOps,
                cells,
            })
        })
        .collect();
    for part in world.ch.parts(TableId::McpOps) {
        for cells in stored_rows(mcp, &part.block).unwrap_or_default() {
            let row = Held {
                table_id: TableId::McpOps,
                cells: &cells,
            };
            let Some(project) = row.project() else {
                continue;
            };
            let of_project = || requests.iter().filter(|r| r.project_id == project);
            // A project deletion that has not completed is an L3 violation of its own.
            let wanted = of_project().any(|request| {
                (request.selector != DeletionSelector::Project
                    || request.state == DeletionState::Completed)
                    && selected(request, &row, &ledger.spans)
            });
            // Rows that a deletion would select if it named no issuer.
            let by_issuer = !wanted
                && of_project()
                    .filter_map(any_issuer)
                    .any(|request| selected(&request, &row, &ledger.spans));
            ledger.stats.mcp_kept_by_issuer += u64::from(by_issuer);
            let expired = ledger.retention_set
                && project == world.projects[0]
                && kept.contains(&TableId::McpOps)
                && row.time().is_some_and(|time| time < expired_before);
            if !wanted && !expired && key(&row).is_some_and(|key| !visible_ops.contains(&key)) {
                ledger.violations.push(format!(
                    "I9: an mcp_ops row of trace {} that no request selects and retention keeps is gone",
                    row.trace().map_or_else(String::new, |trace| trace.to_string())
                ));
            }
        }
    }
}

/// The user who is a member of the projects whose control-plane rows the run seeds.
const MEMBER: UserId = UserId::from_bytes([0x0a; 16]);

/// One row of each control-plane table that holds rows of a project and that a project deletion removes
/// (contract 13.4): a membership, a revoked API key, a channel with its sealed secret, an alert rule with the
/// state of a series, a saved query, a SQL run, an export, a retention floor and the salt. The floor is at the
/// epoch, so it hides nothing.
async fn seed_control(
    pg: &PgModel,
    project: ProjectId,
    n: u8,
    now: Timestamp,
) -> Result<(), TxError> {
    let id = |k: u8| Uuid::from_v7_parts(1_800_000_001_000, (u128::from(n) << 8) | u128::from(k));
    pg.memberships()
        .put_project(ProjectMembership {
            project_id: project,
            user_id: MEMBER,
            role: ProjectRole::Member,
        })
        .await?;
    pg.api_keys()
        .insert(&ApiKey {
            id: ApiKeyId::from_uuid(id(1)),
            project_id: project,
            public_key: format!("cpk_sim{n}"),
            secret_salt: [n; 16],
            secret_hash: [n; 32],
            lookup_tag: [n; 8],
            scopes: vec![KeyScope::Ingest],
            import: false,
            note: String::new(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
            version: 1,
        })
        .await?;
    let secret = Secret {
        id: id(2),
        scope: project.to_string(),
        kind: "channel".to_owned(),
        key_id: "master-0".to_owned(),
        nonce: vec![n; 24],
        ciphertext: vec![n; 32],
    };
    pg.secrets().insert(&secret).await?;
    pg.channels()
        .insert(&Channel {
            id: ChannelId::from_uuid(id(3)),
            project_id: project,
            name: "On call".to_owned(),
            kind: ChannelKind::Webhook,
            config: JsonDoc::empty(),
            secret_id: Some(secret.id),
            enabled: true,
            disabled_reason: None,
            failures: 0,
            last_test: None,
            version: 1,
        })
        .await?;
    let rule = AlertRuleId::from_uuid(id(4));
    pg.alert_rules()
        .insert(&AlertRule {
            id: rule,
            project_id: project,
            name: "errors".to_owned(),
            enabled: false,
            spec: JsonDoc::empty(),
            interval_secs: 60,
            version: 1,
            created_at: now,
            updated_at: now,
            last_eval_seq: None,
            eval_failures: 0,
            skipped_ticks: 0,
        })
        .await?;
    pg.alert_state()
        .put(&crucible_control::AlertSeriesState {
            rule_id: rule,
            series_key: String::new(),
            status: crucible_control::AlertStatus::Firing,
            since: now,
            last_rows_at: now,
            last_notified_at: None,
            value: Some(9),
        })
        .await?;
    pg.saved_queries()
        .insert(&SavedQuery {
            id: SavedQueryId::from_uuid(id(5)),
            project_id: project,
            owner: SqlActor::User(MEMBER),
            name: "errors".to_owned(),
            description: String::new(),
            visibility: SqlVisibility::Project,
            sql: "SELECT 1".to_owned(),
            parameters: Vec::new(),
            version: 1,
            created_at: now,
            updated_at: now,
        })
        .await?;
    pg.sql_runs()
        .insert(&SqlRun {
            id: SqlRunId::from_uuid(id(6)),
            project_id: project,
            actor: SqlActor::User(MEMBER),
            query_id: id(7),
            kind: SqlRunKind::AdHoc,
            saved_query_id: None,
            sql: "SELECT 1".to_owned(),
            started_at: now,
            end: None,
        })
        .await?;
    pg.export_jobs()
        .insert(&NewExport {
            id: ExportJobId::from_uuid(id(8)),
            project_id: project,
            format: ExportFormat::CrucibleJsonl,
            selector: String::new(),
            options: String::new(),
            dedup_key: format!("sim-export-{n}"),
            created_by: "user:sim".to_owned(),
            created_at: now,
            as_of: now,
        })
        .await?;
    pg.lifecycle()
        .raise_floor(
            project,
            RetentionFloor {
                retention: Some(Timestamp::EPOCH),
                payload: None,
                mcp_content: None,
            },
        )
        .await?;
    pg.project_salts()
        .insert_if_absent(&SealedProjectSalt {
            project_id: project,
            key_id: "master-0".to_owned(),
            nonce: vec![n; 24],
            ciphertext: vec![n; 48],
        })
        .await?;
    Ok(())
}

/// How many control-plane rows of each table a project holds: memberships, API keys, channels, channel
/// secrets, alert rules, series states, saved queries, SQL runs, exports, retention floors and salts.
async fn control_rows(
    pg: &PgModel,
    project: ProjectId,
) -> Result<[(&'static str, usize); 11], TxError> {
    let channels = pg.channels().list(project).await?;
    let mut secrets = 0;
    for channel in &channels {
        if let Some(secret) = channel.secret_id
            && pg.secrets().get(secret).await?.is_some()
        {
            secrets += 1;
        }
    }
    let rules = pg.alert_rules().list(project).await?;
    let mut states = 0;
    for rule in &rules {
        states += pg.alert_state().list(rule.id).await?.len();
    }
    let floors = pg.lifecycle().floors().await?;
    Ok([
        (
            "memberships",
            pg.memberships().list_project(project).await?.len(),
        ),
        ("api_keys", pg.api_keys().list(project).await?.len()),
        ("channels", channels.len()),
        ("secrets", secrets),
        ("alert_rules", rules.len()),
        ("alert_state", states),
        (
            "saved_queries",
            pg.saved_queries().list(project).await?.len(),
        ),
        (
            "sql_runs",
            pg.sql_runs().list(project, None, 100).await?.len(),
        ),
        (
            "export_jobs",
            pg.export_jobs().list(project, 100).await?.len(),
        ),
        (
            "retention_floors",
            floors.iter().filter(|(id, _)| *id == project).count(),
        ),
        (
            "project_salts",
            usize::from(pg.project_salts().get(project).await?.is_some()),
        ),
    ])
}

/// I9 for the control plane: a completed project deletion left none of its project's rows (contract 13.4), and
/// the project that keeps everything still holds every one of its rows.
async fn audit_control(world: &World, ledger: &mut Ledger) {
    let completed = world.pg.deletion_requests().iter().any(|request| {
        request.project_id == world.projects[2]
            && request.selector == DeletionSelector::Project
            && request.state == DeletionState::Completed
    });
    for (project, gone) in [(world.projects[1], false), (world.projects[2], completed)] {
        let rows = match control_rows(&world.pg, project).await {
            Ok(rows) => rows,
            Err(error) => {
                ledger
                    .violations
                    .push(format!("the control-plane rows could not be read: {error}"));
                continue;
            }
        };
        for (table_name, count) in rows {
            ledger.stats.control_rows_checked += 1;
            let wanted = usize::from(!gone);
            if count != wanted {
                ledger.violations.push(format!(
                    "I9: project {project} holds {count} rows of {table_name} at the end, {wanted} expected"
                ));
            }
        }
    }
}

fn project(id: ProjectId, slug: &str) -> Project {
    Project {
        id,
        name: slug.to_owned(),
        slug: slug.to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

fn config(mutant: Option<Mutant>) -> LifecycleConfig {
    let mut config = LifecycleConfig::default();
    config.retention_every = Duration::from_secs(600);
    config.mask_cycle = Duration::from_secs(1_800);
    config.sweep_every = SWEEP_EVERY;
    config.max_event_age =
        Duration::from_secs(u64::try_from(MAX_EVENT_AGE_DAYS).unwrap_or(0) * 86_400);
    match mutant {
        None => config,
        Some(Mutant::SkipCheckpointWait) => {
            config.with_mutant(crucible_lifecycle::Mutant::SkipCheckpointWait)
        }
        Some(Mutant::DropMaskCycle) => {
            config.with_mutant(crucible_lifecycle::Mutant::DropMaskCycle)
        }
        Some(Mutant::DeleteWithoutFencing) => {
            config.with_mutant(crucible_lifecycle::Mutant::DeleteWithoutFencing)
        }
        Some(Mutant::McpByTraceOnly) => {
            config.with_mutant(crucible_lifecycle::Mutant::McpByTraceOnly)
        }
        Some(Mutant::DropSharedPartitions) => {
            config.with_mutant(crucible_lifecycle::Mutant::DropSharedPartitions)
        }
        Some(Mutant::DropWithinEventAge) => {
            config.with_mutant(crucible_lifecycle::Mutant::DropWithinEventAge)
        }
        Some(Mutant::SweepDisabled) => {
            config.with_mutant(crucible_lifecycle::Mutant::SweepDisabled)
        }
        Some(Mutant::SweepWithoutProject) => {
            config.with_mutant(crucible_lifecycle::Mutant::SweepWithoutProject)
        }
    }
}

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let database = sim.host("lifecycle-postgres", &sim.disk("lifecycle-postgres"), 0);
    let clock = Arc::clone(&database.env.clock);
    let pg = PgModel::new(
        database.clock.clone(),
        sim.rng("lifecycle-sim/pg").as_ref(),
        PgFaults::default(),
    );
    let ch = ChModel::new(
        sim.rng("lifecycle-sim/ch").as_ref(),
        StoreFlavour::SingleNode,
        Granularity::Day,
        Duration::from_secs(3_600),
    );
    // The first two are made as the run starts, the deleted one forty days before.
    let projects = [1_u64, 2, 3].map(|n| {
        let made = if n == 3 {
            1_800_000_000_000 - DELETED_MADE_DAYS_AGO * 86_400_000
        } else {
            1_800_000_000_000 + n
        };
        ProjectId::from_uuid(Uuid::from_v7_parts(made, u128::from(n)))
    });
    let ledger: Arc<Mutex<Ledger>> = Arc::default();
    for (id, slug) in projects.iter().zip(["shortened", "kept", "deleted"]) {
        if let Err(error) = pg.projects().insert(&project(*id, slug)).await {
            lock(&ledger)
                .violations
                .push(format!("the fixture project could not be stored: {error}"));
        }
    }
    let member = Email::new("member@example.com").map(|email| User {
        id: MEMBER,
        email,
        name: "Member".to_owned(),
        status: UserStatus::Active,
        email_verified_at: None,
    });
    let seeded = async {
        if let Some(member) = &member {
            pg.users().insert(member).await?;
        }
        let now = clock.wall_now();
        seed_control(&pg, projects[1], 1, now).await?;
        seed_control(&pg, projects[2], 2, now).await
    };
    if let Err(error) = seeded.await {
        lock(&ledger).violations.push(format!(
            "the control-plane rows could not be seeded: {error}"
        ));
    }
    let logs = (0..options.nodes)
        .map(|n| {
            Arc::new(SimLog {
                epoch: WalEpoch::from_uuid(Uuid::from_v7_parts(
                    1_800_000_000_100 + n as u64,
                    n as u128 + 1,
                )),
                state: Mutex::default(),
            })
        })
        .collect();
    let world = World {
        pg: pg.clone(),
        ch: ch.clone(),
        database: database.env.clone(),
        ledger: Arc::clone(&ledger),
        logs: Arc::new(logs),
        up: Arc::new(Mutex::new(vec![None; options.nodes])),
        projects,
        deleted: Arc::default(),
        config: config(options.mutant),
        options,
        ids: Arc::default(),
        salt_faults: Arc::new(AtomicBool::new(options.store_faults)),
        salt_rng: sim.rng("lifecycle-sim/salts"),
        late: Arc::default(),
    };
    let today = day_of(clock.wall_now().micros()).unwrap_or(0);
    history(&world, sim.rng("lifecycle-sim/history").as_ref(), today).await;
    pg.set_faults(pg_faults(options.store_faults));
    ch.set_faults(ch_faults(options.store_faults));
    let faulty_until = clock.now().saturating_add(options.faulty_for).nanos();
    let end = faulty_until + nanos(options.tail);
    // The third project is deleted in the faulty phase, after its frames and requests have had time to land.
    let delete_rng = sim.rng("lifecycle-sim/delete");
    let latest = u64::try_from(options.faulty_for.as_millis())
        .unwrap_or(0)
        .saturating_sub(120_000)
        .max(180_000);
    let delete_at = clock
        .now()
        .saturating_add(millis(delete_rng.as_ref(), 180_000, latest))
        .nanos();
    let nodes: Vec<_> = (0..options.nodes)
        .map(|n| node(&sim, world.clone(), n, faulty_until, end))
        .collect();
    let calm = async {
        clock.sleep_until(Instant::from_nanos(faulty_until)).await;
        pg.set_faults(pg_faults(false));
        ch.set_faults(ch_faults(false));
        ch.end_outage();
        world.salt_faults.store(false, Ordering::SeqCst);
    };
    futures::join!(
        futures::future::join_all(nodes),
        chaos(world.clone(), sim.rng("lifecycle-sim/chaos"), faulty_until),
        ingest(
            world.clone(),
            sim.rng("lifecycle-sim/ingest"),
            today,
            faulty_until
        ),
        requests(
            world.clone(),
            sim.rng("lifecycle-sim/requests"),
            faulty_until
        ),
        delete_project(world.clone(), delete_rng, delete_at),
        after_completion(
            world.clone(),
            sim.rng("lifecycle-sim/after-completion"),
            today,
            faulty_until,
            end
        ),
        calm,
    );
    let mut ledger = std::mem::take(&mut *lock(&ledger));
    audit(&world, &mut ledger, clock.wall_now());
    audit_control(&world, &mut ledger).await;
    database.stop().await;
    Outcome {
        seed: sim.seed(),
        stats: ledger.stats,
        violations: ledger.violations,
    }
}

/// Runs the lifecycle simulation for one seed.
pub fn run_lifecycle_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}

#[cfg(test)]
mod tests {
    use crucible_records::DeletionRequestId;

    use super::*;

    /// A frame whose MCP rows carry the hash of `ada` under the first issuer and no `user_id`.
    fn hashed() -> Spans {
        Spans {
            project: ProjectId::from_bytes([1; 16]),
            trace: TraceId::from_bytes([2; 16]),
            user: "ada".to_owned(),
            identity: Identity {
                spans: false,
                mcp_user: false,
                issuer: Some(ISSUERS[0]),
            },
            session: "s-1".to_owned(),
            start: Timestamp::from_micros(0),
            received_at: Timestamp::from_micros(5),
            span_ids: vec![[3; 8]],
        }
    }

    fn by_user(issuer: Option<&str>) -> DeletionRequest {
        DeletionRequest {
            id: DeletionRequestId::from_bytes([4; 16]),
            project_id: ProjectId::from_bytes([1; 16]),
            selector: DeletionSelector::User {
                user_id: "ada".to_owned(),
                issuer: issuer.map(str::to_owned),
            },
            state: DeletionState::Pending,
            requested_at: Timestamp::from_micros(9),
            deletion_seq: 1,
            requested_by: "user:test".to_owned(),
            idempotency_key: None,
            request_hash: String::new(),
            places: "{}".to_owned(),
            attempts: 0,
            error: None,
            updated_at: Timestamp::from_micros(9),
        }
    }

    #[test]
    fn a_deletion_by_user_selects_mcp_rows_hashed_under_the_issuer_it_names_or_any() {
        let frame = hashed();
        assert!(frame.mcp_selected_by(&by_user(None)));
        assert!(frame.mcp_selected_by(&by_user(Some(ISSUERS[0]))));
        // Hashed under another issuer: another identity, which the request keeps.
        assert!(!frame.mcp_selected_by(&by_user(Some(ISSUERS[1]))));
    }
}
