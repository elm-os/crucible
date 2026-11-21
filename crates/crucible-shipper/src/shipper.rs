//! The shipper task: tail the durable WAL, normalize, fill and seal batches, ship their pairs, checkpoint, and
//! drive garbage collection (contract sections 4.6, 5.1, 5.7 and 5.9). Stage B drops what the deletion requests
//! select (contract 13.3, step 3): those of the `Tombstone` frames it has passed, and every request the control
//! store holds that can select a frame, which it reads before it admits the frame.
//!
//! Stage A of the frames read ahead ([`Window`]) and the encoding of each sealed batch's blocks run as blocking
//! jobs under their shares of the CPU budget ([`CpuStage`]); the task keeps reading, stage B, the rows and the
//! WAL's records.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crucible_env::metrics::registry;
use crucible_env::{Counter, Env, Gauge, Instant, JoinHandle, Metrics, Rng, Stores};
use crucible_normalize::{
    Admission, IdentityCtx, NORMALIZER_VERSION, NormCtx, NormalizedFrame, PriceTable, ProjectSalt,
    StageB, Tombstone, TombstoneSelector, Tombstones,
};
use crucible_records::{
    DeletionRequestId, DeletionState, DetMap, Fact, Lsn, PartitionId, ProjectId, TableId,
    Timestamp, det_map,
};
use crucible_wal::{AppendError, RecoveryReport, Replay, ReplayError, Wal, WalFrame};
use crucible_wal_codec::{
    self as codec, BatchSeal, Checkpoint, Compression, PreparedFrame, Record, RecordType,
};
use futures::stream::{FuturesUnordered, StreamExt};
use futures::{FutureExt, select_biased};
use tokio::sync::mpsc;

use crate::batch::{BatchId, OpenBatch, block, pair_token};
use crate::config::ShipperConfig;
use crate::cpu::{CpuStage, ENCODE, STAGE_A};
use crate::error::ShipError;
use crate::frames::Content;
use crate::hooks::{
    Barrier, Barriers, DeletionRequests, NoBarriers, NoDeletions, NoRecent, NoSalts, ProjectSalts,
    RecentIndex,
};
use crate::limits::{DrainRate, Gate, SealLimits, backoff};
use crate::pair::{PairEnd, PairJob, ship_pair};
use crate::phase::{Laps, Phase};
use crate::quarantine::quarantine_frame;
use crate::tables::{Defects, RoutedRow, derive_facts, derive_rows};
use crate::window::{Read, Ready, Window};

/// What the shipper takes besides `Env`, `Stores` and the WAL.
#[derive(Clone, Debug)]
pub struct Hooks {
    /// The crash barriers; [`NoBarriers`] in production.
    pub barriers: Arc<dyn Barriers>,
    /// The recent-span index: fed by stage B, and its floor pins WAL segments.
    pub recent: Arc<dyn RecentIndex>,
    /// The project salts, for the user subjects of MCP operations; [`NoSalts`] without a control store.
    pub salts: Arc<dyn ProjectSalts>,
    /// The deletion requests, which stage B applies; [`NoDeletions`] without a control store.
    pub deletions: Arc<dyn DeletionRequests>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            barriers: Arc::new(NoBarriers),
            recent: Arc::new(NoRecent),
            salts: Arc::new(NoSalts),
            deletions: Arc::new(NoDeletions),
        }
    }
}

/// One counter of a shipper: exact for the simulation's assertions, and a metric of the registry.
#[derive(Debug)]
pub(crate) struct Tally {
    count: AtomicU64,
    metric: Counter,
}

impl Tally {
    fn new(metric: Counter) -> Self {
        Self {
            count: AtomicU64::new(0),
            metric,
        }
    }

    pub(crate) fn add(&self, n: u64) {
        self.count.fetch_add(n, Ordering::Relaxed);
        self.metric.add(n);
    }

    fn get(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Counters of one shipper, for tests and for the simulation's assertions. Each is also a metric of the
/// `crucible-env` registry.
#[derive(Debug)]
pub struct ShipperStats {
    pub(crate) sealed: Tally,
    pub(crate) reshipped: Tally,
    pub(crate) checkpoints: Tally,
    pub(crate) inserts: Tally,
    pub(crate) unknown_outcomes: Tally,
    pub(crate) probes: Tally,
    pub(crate) probes_present: Tally,
    pub(crate) probes_expired: Tally,
    pub(crate) quarantined: Tally,
    pub(crate) quarantined_frames: Tally,
    pub(crate) mixed_version_batches: Tally,
    pub(crate) inventory_dropped: Tally,
    deleted_spans: Tally,
    deleted_ops: Tally,
    deleted_facts: Tally,
    /// The rows held now, in estimated bytes (the `shipper.encoded_bytes` budget), with its gauge.
    encoded_bytes: AtomicU64,
    encoded_gauge: Gauge,
    encoded_peak: AtomicU64,
    largest_frame: AtomicU64,
    /// The most frames read ahead of stage B at once.
    ahead_peak: AtomicU64,
    drain_gauge: Gauge,
}

/// A copy of the [`ShipperStats`] counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StatsSnapshot {
    /// Batches sealed with fresh boundaries.
    pub sealed: u64,
    /// Sealed batches found by recovery and shipped again.
    pub reshipped: u64,
    /// Checkpoints appended.
    pub checkpoints: u64,
    /// Inserts sent.
    pub inserts: u64,
    /// Inserts whose outcome was unknown.
    pub unknown_outcomes: u64,
    /// Existence probes sent.
    pub probes: u64,
    /// Probes that read present.
    pub probes_present: u64,
    /// Absent probes whose answer expired before the send it would have authorized.
    pub probes_expired: u64,
    /// Pairs quarantined.
    pub quarantined: u64,
    /// Frames quarantined because this binary cannot decode or normalize them.
    pub quarantined_frames: u64,
    /// Recovered batches shipped again with another normalizer or price table version than their seal records.
    pub mixed_version_batches: u64,
    /// Tool inventory entries beyond the per-frame cap, which were not kept (contract 5.1).
    pub inventory_dropped: u64,
    /// Spans that stage B dropped from the frames it shipped because a deletion request selects them.
    pub deleted_spans: u64,
    /// MCP operations that stage B dropped from the frames it shipped because a deletion request selects them:
    /// with their spans, or by their own identity.
    pub deleted_ops: u64,
    /// Facts that stage B dropped from the frames it shipped because a deletion request selects them.
    pub deleted_facts: u64,
    /// The most estimated row bytes held at once (the `shipper.encoded_bytes` budget).
    pub encoded_bytes_peak: u64,
    /// The estimated row bytes of the largest frame placed.
    pub largest_frame_bytes: u64,
    /// The most frames read from the WAL ahead of stage B at once: one when stage A runs in place, as the
    /// simulation's spawner runs blocking work.
    pub frames_ahead_peak: u64,
}

impl ShipperStats {
    fn new(m: &dyn Metrics) -> Self {
        let batches = |kind| Tally::new(m.counter(&registry::SHIPPER_BATCHES, &[kind]));
        let inserts = |outcome| Tally::new(m.counter(&registry::SHIPPER_INSERTS, &[outcome]));
        let probes = |result| Tally::new(m.counter(&registry::SHIPPER_PROBES, &[result]));
        let deleted = |kind| Tally::new(m.counter(&registry::SHIPPER_DELETION_DROPS, &[kind]));
        let encoded_gauge = m.gauge(&registry::SHIPPER_ENCODED_BYTES, &[]);
        encoded_gauge.set(0);
        let drain_gauge = m.gauge(&registry::SHIPPER_DRAIN_BYTES_PER_SECOND, &[]);
        drain_gauge.set(0);
        Self {
            sealed: batches("sealed"),
            reshipped: batches("reshipped"),
            checkpoints: Tally::new(m.counter(&registry::SHIPPER_CHECKPOINTS, &[])),
            inserts: inserts("sent"),
            unknown_outcomes: inserts("unknown"),
            probes: probes("sent"),
            probes_present: probes("present"),
            probes_expired: probes("expired"),
            quarantined: Tally::new(m.counter(&registry::SHIPPER_QUARANTINED_PAIRS, &[])),
            quarantined_frames: Tally::new(m.counter(&registry::SHIPPER_QUARANTINED_FRAMES, &[])),
            mixed_version_batches: Tally::new(
                m.counter(&registry::SHIPPER_MIXED_VERSION_BATCHES, &[]),
            ),
            inventory_dropped: Tally::new(
                m.counter(&registry::SHIPPER_INVENTORY_ROWS_DROPPED, &[]),
            ),
            deleted_spans: deleted("span"),
            deleted_ops: deleted("mcp_operation"),
            deleted_facts: deleted("fact"),
            encoded_bytes: AtomicU64::new(0),
            encoded_gauge,
            encoded_peak: AtomicU64::new(0),
            largest_frame: AtomicU64::new(0),
            ahead_peak: AtomicU64::new(0),
            drain_gauge,
        }
    }

    /// The counters now.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            sealed: self.sealed.get(),
            reshipped: self.reshipped.get(),
            checkpoints: self.checkpoints.get(),
            inserts: self.inserts.get(),
            unknown_outcomes: self.unknown_outcomes.get(),
            probes: self.probes.get(),
            probes_present: self.probes_present.get(),
            probes_expired: self.probes_expired.get(),
            quarantined: self.quarantined.get(),
            quarantined_frames: self.quarantined_frames.get(),
            mixed_version_batches: self.mixed_version_batches.get(),
            inventory_dropped: self.inventory_dropped.get(),
            deleted_spans: self.deleted_spans.get(),
            deleted_ops: self.deleted_ops.get(),
            deleted_facts: self.deleted_facts.get(),
            encoded_bytes_peak: self.encoded_peak.load(Ordering::Relaxed),
            largest_frame_bytes: self.largest_frame.load(Ordering::Relaxed),
            frames_ahead_peak: self.ahead_peak.load(Ordering::Relaxed),
        }
    }

    /// Notes that `frames` are read ahead of stage B.
    fn ahead(&self, frames: usize) {
        self.ahead_peak.fetch_max(frames as u64, Ordering::Relaxed);
    }

    /// The estimated row bytes held now.
    fn held_bytes(&self) -> u64 {
        self.encoded_bytes.load(Ordering::Relaxed)
    }

    /// Charges the rows of a frame to the budget. Only the shipper task changes the total.
    fn charge(&self, bytes: u64) {
        let held = self.held_bytes().saturating_add(bytes);
        self.encoded_bytes.store(held, Ordering::Relaxed);
        self.encoded_peak.fetch_max(held, Ordering::Relaxed);
        self.largest_frame.fetch_max(bytes, Ordering::Relaxed);
        self.encoded_gauge
            .set(i64::try_from(held).unwrap_or(i64::MAX));
    }

    /// Releases the rows of a resolved batch.
    fn release(&self, bytes: u64) {
        let held = self.held_bytes().saturating_sub(bytes);
        self.encoded_bytes.store(held, Ordering::Relaxed);
        self.encoded_gauge
            .set(i64::try_from(held).unwrap_or(i64::MAX));
    }
}

/// What every part of one shipper shares.
#[derive(Debug)]
pub(crate) struct Ctx {
    pub(crate) env: Env,
    pub(crate) stores: Stores,
    pub(crate) wal: Wal,
    pub(crate) config: ShipperConfig,
    /// The bundled price table, which stage A prices with and every new seal records.
    pub(crate) prices: &'static PriceTable,
    pub(crate) barriers: Arc<dyn Barriers>,
    pub(crate) recent: Arc<dyn RecentIndex>,
    salts: Arc<dyn ProjectSalts>,
    deletions: Arc<dyn DeletionRequests>,
    pub(crate) gate: Gate,
    pub(crate) rng: Arc<dyn Rng>,
    pub(crate) stats: Arc<ShipperStats>,
    limits: Mutex<SealLimits>,
    /// The CPU budget's share for stage A of the frames read ahead of stage B.
    pub(crate) stage_a: CpuStage,
    /// The CPU budget's share for encoding the blocks of sealed batches.
    pub(crate) encode: CpuStage,
}

impl Ctx {
    pub(crate) fn with_limits<T>(&self, f: impl FnOnce(&mut SealLimits) -> T) -> T {
        f(&mut self.limits.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn limits(&self) -> SealLimits {
        self.with_limits(|limits| *limits)
    }

    async fn pause(&self, attempt: u32) {
        let delay = backoff(
            self.config.backoff_min,
            self.config.backoff_max,
            attempt,
            self.rng.as_ref(),
        );
        self.env.clock.sleep(delay).await;
    }

    fn prepare(record: &Record<'_>) -> Result<PreparedFrame, ShipError> {
        let invalid = |_| ShipError::Append(AppendError::InvalidControl);
        let payload = record.encode().map_err(invalid)?;
        PreparedFrame::new(record.record_type(), payload, Compression::Lz4)
            .map_err(|_| ShipError::Append(AppendError::InvalidControl))
    }

    /// Appends a control record and returns once it is durable. A failed group is retried with the same record;
    /// a failed WAL stops the shipper.
    pub(crate) async fn append_durable(&self, record: &Record<'_>) -> Result<Lsn, ShipError> {
        let frame = Self::prepare(record)?;
        let mut attempt = 0;
        loop {
            match self.wal.append(frame.clone()).await {
                Ok(lsn) => return Ok(lsn),
                Err(AppendError::Unavailable) => {
                    self.pause(attempt).await;
                    attempt = attempt.saturating_add(1);
                }
                Err(AppendError::Failed | AppendError::Closed) => return Err(ShipError::WalFailed),
                Err(e) => return Err(ShipError::Append(e)),
            }
        }
    }
}

/// When a failed garbage collection is logged: the first failure after a success, then at most one per
/// interval, each with the number of failures since the line before it.
#[derive(Debug, Default)]
struct GcFailures {
    logged_at: Option<Instant>,
    unreported: u64,
}

impl GcFailures {
    /// Counts a failure at `now`. Returns the failures to report when this one is logged.
    fn failed(&mut self, now: Instant, every: Duration) -> Option<u64> {
        self.unreported += 1;
        if self
            .logged_at
            .is_some_and(|at| now.saturating_duration_since(at) < every)
        {
            return None;
        }
        self.logged_at = Some(now);
        Some(std::mem::take(&mut self.unreported))
    }
}

/// A pair that a durable `Quarantine` frame names.
type QuarantinedPair = (Lsn, Lsn, TableId, PartitionId);

/// The rows of a frame before they are stamped with their batch and routed.
#[derive(Debug)]
enum Unplaced {
    /// The spans stage B admitted.
    Spans(NormalizedFrame, Admission),
    /// The facts of a `FactBatch` frame.
    Facts(Vec<Fact>),
}

/// A frame that is read and not in a batch yet.
#[derive(Debug)]
struct Held {
    lsn: Lsn,
    rows: Unplaced,
    received_at: Timestamp,
    raw_bytes: u64,
    /// The estimated bytes of the frame's rows, once they were derived for a batch that could not take them.
    row_bytes: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BatchState {
    Shipping,
    Resolved,
    /// A pair was refused for the store's configuration (contract 5.6): the batch never resolves in this
    /// process, so the watermark stays below it and the WAL keeps it.
    Stopped,
}

#[derive(Debug)]
struct InFlight {
    id: BatchId,
    state: BatchState,
    task: JoinHandle<()>,
    /// The raw bytes of the batch's frames, which its checkpoint drains from the WAL.
    raw_bytes: u64,
    /// What the batch holds of the `shipper.encoded_bytes` budget until its task ends.
    row_bytes: u64,
    released: bool,
}

type BatchDone = (Lsn, Result<BatchState, ShipError>);

/// The shipper of one node. [`Shipper::run`] is its only task; it ends on shutdown or with a [`ShipError`].
#[derive(Debug)]
pub struct Shipper {
    ctx: Arc<Ctx>,
    replay: Replay,
    stage_b: StageB,
    /// For each frame above the watermark whose admission used an earlier sighting, the lowest LSN it relies
    /// on (`Admission::relies_on`). Garbage collection keeps the log from the lowest of them on, so that a
    /// restart rebuilds stage B to decide about these frames as this process did.
    relied_on: BTreeMap<Lsn, Lsn>,
    /// Frames at or below this LSN only rebuild stage B (contract 4.6, step 6).
    watermark: Lsn,
    /// Sealed batches recovery found, oldest first: their boundaries are kept and their pairs are in doubt.
    seals: VecDeque<BatchSeal>,
    quarantined: BTreeSet<QuarantinedPair>,
    open: OpenBatch,
    held: Option<Held>,
    /// The frames read ahead of stage B, whose stage A runs off the task.
    window: Window,
    in_flight: VecDeque<InFlight>,
    done_tx: mpsc::UnboundedSender<BatchDone>,
    done_rx: mpsc::UnboundedReceiver<BatchDone>,
    last_gc: Instant,
    /// The last garbage collection failed, so a segment may be removed without the removal being durable: a
    /// restart may or may not find it. No frame is admitted until a collection succeeds, because stage B could
    /// not say what a restart will remember.
    gc_in_doubt: bool,
    gc_failures: GcFailures,
    read_failures: u32,
    /// The salts fetched so far, `None` for a deleted project that has none. A salt never changes, so it is
    /// fetched once per project and process.
    salts: DetMap<ProjectId, Option<ProjectSalt>>,
    /// Consecutive failures to fetch a salt, for the backoff of the frame that waits for it.
    salt_failures: u32,
    /// Whether the drop of identity keys without a control store was logged.
    identity_drop_logged: bool,
    /// The deletions stage B applies (contract 13.3, step 3): the requests of the control store that can select the
    /// frames stage B admits, and those of the `Tombstone` frames it has passed.
    tombstones: Tombstones,
    /// Stage B knows every request that the control store held at a read and that can select data received at or
    /// after this instant; `None` before the first read. See [`Shipper::cover`].
    covered_from: Option<Timestamp>,
    /// The issuer of each deletion by user that is known, by request: a `Tombstone` frame does not carry it.
    issuers: DetMap<DeletionRequestId, Option<Box<str>>>,
    /// Consecutive failures to read the deletion requests, for the backoff of what waits for them.
    deletion_failures: u32,
    drain: DrainRate,
    /// The task's time by phase (`crucible_shipper_task_microseconds_total`).
    laps: Laps,
}

/// The stage B context of a frame: what is known of its project's identity and, unless the seeded defect skips
/// them, the deletions.
fn stage_b_ctx<'a>(
    ctx: &'a Ctx,
    tombstones: &'a Tombstones,
    identity: IdentityCtx<'a>,
) -> NormCtx<'a> {
    let mut norm = NormCtx::new(ctx.prices, ctx.config.caps).with_identity(identity);
    norm.mutant = ctx.config.normalizer_mutant();
    if ctx.config.skips_tombstone_filter() {
        norm
    } else {
        norm.with_tombstones(tombstones)
    }
}

/// The deletion of a `Tombstone` record, without the issuer of a deletion by user, which the record does not carry.
fn tombstone(record: &codec::Tombstone<'_>) -> Tombstone {
    Tombstone {
        request_id: record.deletion_request_id,
        project_id: record.project_id,
        requested_at: record.requested_at,
        deletion_seq: record.deletion_seq,
        selector: match &record.selector {
            codec::TombstoneSelector::Traces(ids) => TombstoneSelector::Traces(ids.clone()),
            codec::TombstoneSelector::User(user_id) => TombstoneSelector::User {
                user_id: (*user_id).into(),
                issuer: None,
            },
            codec::TombstoneSelector::Session(session_id) => {
                TombstoneSelector::Session((*session_id).into())
            }
            codec::TombstoneSelector::Project => TombstoneSelector::Project,
        },
    }
}

/// The issuer a deletion by user names.
fn issuer_of(tombstone: &Tombstone) -> Option<Box<str>> {
    match &tombstone.selector {
        TombstoneSelector::User { issuer, .. } => issuer.clone(),
        _ => None,
    }
}

/// What reading a `Tombstone` frame gave.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Learned {
    /// Stage B applies the deletion from here on.
    Applied,
    /// The control store did not give the issuer of the deletion: the frame is read again after a backoff.
    Waits,
}

/// What [`Shipper::next_ready`] found.
#[derive(Debug)]
enum Next {
    /// Stage B takes this frame now.
    Frame(Ready),
    /// A frame was read while stage A of the frame before it runs: read on.
    Read,
    /// Nothing to do until stage A of the next frame ends or, when the log is `caught_up`, another frame becomes
    /// durable.
    Wait { caught_up: bool },
}

/// What stage B is told about the identity of one frame's project: no control store, or the project's salt
/// when the frame needs one.
#[derive(Clone, Copy, Debug)]
enum Identity {
    Suppressed,
    Salted(Option<ProjectSalt>),
}

impl Identity {
    fn ctx(&self) -> IdentityCtx<'_> {
        match self {
            Self::Suppressed => IdentityCtx::Suppressed,
            Self::Salted(salt) => IdentityCtx::Salted(salt.as_ref()),
        }
    }
}

impl Drop for Shipper {
    fn drop(&mut self) {
        for batch in &self.in_flight {
            batch.task.abort();
        }
    }
}

fn frame_error(lsn: Lsn, reason: impl ToString) -> ShipError {
    ShipError::Frame {
        lsn,
        reason: reason.to_string(),
    }
}

impl Shipper {
    /// A shipper over a WAL that [`Wal::open`] just recovered. `report` is that recovery's report: its unchecked
    /// seals are shipped again with their boundaries, and the pairs its `Quarantine` frames name stay resolved
    /// (contract 4.6, steps 5 and 8).
    pub fn new(
        env: &Env,
        stores: &Stores,
        wal: Wal,
        report: &RecoveryReport,
        config: ShipperConfig,
        hooks: Hooks,
    ) -> Result<Self, ShipError> {
        let watermark = report.watermark_lsn();
        let mut expected = Lsn::new(watermark.get().saturating_add(1));
        let mut seals: VecDeque<BatchSeal> = VecDeque::new();
        for seal in &report.unchecked_seals {
            if seals.back() == Some(seal) {
                continue;
            }
            if seal.first_lsn != expected {
                return Err(ShipError::SealChain {
                    expected,
                    found: seal.first_lsn,
                });
            }
            expected = Lsn::new(seal.last_lsn.get().saturating_add(1));
            seals.push_back(*seal);
        }
        let mut quarantined = BTreeSet::new();
        for frame in &report.quarantines {
            let lsn = frame.lsn();
            let payload = frame.payload().map_err(|e| frame_error(lsn, e))?;
            let record = Record::decode(RecordType::Quarantine, &payload)
                .map_err(|e| frame_error(lsn, e))?;
            if let Record::Quarantine(q) = record {
                for pair in &q.pairs {
                    quarantined.insert((q.first_lsn, q.last_lsn, pair.table_id, pair.partition_id));
                }
            }
        }
        let prices = PriceTable::bundled()?;
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let metrics = env.metrics.as_ref();
        let window = Window::new(
            config.cache_bytes,
            config.stage_a_permits,
            metrics.gauge(&registry::NORMALIZE_CACHE_BYTES, &[]),
        );
        let ctx = Arc::new(Ctx {
            env: env.clone(),
            stores: stores.clone(),
            prices,
            limits: Mutex::new(SealLimits::new(&config)),
            gate: Gate::new(config.connections),
            rng: env.rng.fork("crucible-shipper/backoff"),
            stats: Arc::new(ShipperStats::new(metrics)),
            barriers: hooks.barriers,
            recent: hooks.recent,
            salts: hooks.salts,
            deletions: hooks.deletions,
            stage_a: CpuStage::new(metrics, STAGE_A, "shipper-stage-a", config.stage_a_permits),
            encode: CpuStage::new(metrics, ENCODE, "shipper-encode", config.encode_permits),
            wal,
            config,
        });
        let stage_b = StageB::new(
            ctx.config.duplicate_window_keys,
            ctx.config.first_seen_traces,
        );
        #[cfg(feature = "test-mutants")]
        let stage_b = stage_b.with_mutant(ctx.config.normalizer_mutant());
        Ok(Self {
            replay: ctx.wal.replay(report.first_retained_lsn()),
            stage_b,
            relied_on: BTreeMap::new(),
            watermark,
            seals,
            quarantined,
            open: OpenBatch::new(Lsn::new(watermark.get().saturating_add(1))),
            held: None,
            window,
            in_flight: VecDeque::new(),
            done_tx,
            done_rx,
            last_gc: ctx.env.clock.now(),
            gc_in_doubt: false,
            gc_failures: GcFailures::default(),
            read_failures: 0,
            salts: det_map(),
            salt_failures: 0,
            identity_drop_logged: false,
            tombstones: Tombstones::new(),
            covered_from: None,
            issuers: det_map(),
            deletion_failures: 0,
            drain: DrainRate::new(ctx.env.clock.now()),
            laps: Laps::new(env.metrics.as_ref(), env.clock.now()),
            ctx,
        })
    }

    /// The shipper's counters.
    pub fn stats(&self) -> Arc<ShipperStats> {
        Arc::clone(&self.ctx.stats)
    }

    /// Charges the task's time since the previous lap to `phase`.
    fn lap(&mut self, phase: Phase) {
        let now = self.ctx.env.clock.now();
        self.laps.lap(phase, now);
    }

    /// The stage A context. This build has one price table, so a batch that an earlier build sealed with another
    /// version is priced with this build's table, as it is normalized by this build's normalizer (contract 5.3):
    /// its pairs land wholesale.
    fn norm_ctx(&self) -> NormCtx<'static> {
        let mut ctx = NormCtx::new(self.ctx.prices, self.ctx.config.caps);
        ctx.mutant = self.ctx.config.normalizer_mutant();
        ctx
    }

    /// What stage B is told about the identity of a frame's project (contract 9.3), or `None` when the frame
    /// needs a salt that the control store did not give: the frame then waits.
    ///
    /// Without a control store every MCP operation loses its identity keys, which is logged once. With one,
    /// only a frame that carries a user subject needs the salt of its project, so that a control store that
    /// does not answer holds no other frame; the salt is fetched once and kept, because it never changes. A
    /// deleted project that has no salt gets none: its operations lose the subject and its issuer.
    async fn identity_of(&mut self, frame: &NormalizedFrame) -> Option<Identity> {
        if !self.ctx.salts.configured() {
            if !frame.mcp_ops.is_empty() && !std::mem::replace(&mut self.identity_drop_logged, true)
            {
                tracing::warn!(
                    "this node has no project salts, which the control store holds, so the user identity                      of MCP operations is dropped (user id, subject, issuer, tenant, OAuth client id)"
                );
            }
            return Some(Identity::Suppressed);
        }
        let Some(project) = frame
            .mcp_ops
            .iter()
            .find(|op| op.has_subject())
            .map(|op| op.record.project_id)
        else {
            return Some(Identity::Salted(None));
        };
        if let Some(salt) = self.salts.get(&project) {
            return Some(Identity::Salted(*salt));
        }
        self.lap(Phase::StageB);
        let fetched = self.ctx.salts.salt(project).await;
        // The wait for the control store is not the task's work.
        self.lap(Phase::WaitFrames);
        match fetched {
            Ok(salt) => {
                self.salts.insert(project, salt);
                self.salt_failures = 0;
                Some(Identity::Salted(salt))
            }
            Err(e) => {
                tracing::warn!(lsn = %frame.lsn, error = %e, "a frame waits for the salt of its project");
                None
            }
        }
    }

    /// Makes stage B know every deletion request of the control store, in every state, that can select a frame
    /// received at `received_at`, before it admits the frame (contract 13.3). A request can complete while this
    /// node is down, and then no tombstone of it is in this node's log, yet the frames this node never shipped may
    /// hold what it deleted. A deletion selects data received before its `requested_at`, so the requests that can
    /// select the frame are those requested after `received_at`, and only those are read: requests are never
    /// pruned, and one that completed long before the oldest frame stage B admits selects nothing it ships. The
    /// first frame stage B admits reads them; a later frame reads again only when it was received before every
    /// frame read for so far, which a wall clock that stepped back can cause. The hook registers the node first,
    /// so a request made after a read waits for this node's tombstone. `false` when the read failed: the frame
    /// waits, and nothing after it ships.
    async fn cover(&mut self, received_at: Timestamp) -> bool {
        if !self.ctx.deletions.configured()
            || self.covered_from.is_some_and(|from| from <= received_at)
        {
            return true;
        }
        self.lap(Phase::StageB);
        let read = self.ctx.deletions.requested_after(received_at).await;
        self.lap(Phase::WaitFrames);
        match read {
            Ok(requests) => {
                let count = requests.len();
                for known in requests {
                    let done = matches!(
                        known.state,
                        DeletionState::Completed | DeletionState::Failed
                    );
                    if done && self.ctx.config.seeds_only_open_requests() {
                        continue;
                    }
                    self.learn(known.tombstone);
                }
                self.covered_from = Some(received_at);
                self.deletion_failures = 0;
                tracing::info!(
                    requests = count,
                    requested_after = %received_at,
                    "stage B applies the deletion requests of the control store"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "nothing ships until the deletion requests can be read");
                false
            }
        }
    }

    /// Stage B applies `tombstone` from here on, and remembers the issuer of a deletion by user.
    fn learn(&mut self, tombstone: Tombstone) {
        if matches!(tombstone.selector, TombstoneSelector::User { .. }) {
            self.issuers
                .insert(tombstone.request_id, issuer_of(&tombstone));
        }
        self.tombstones.insert(&tombstone);
    }

    /// Reads a `Tombstone` frame (contract 13.3, step 3): stage B applies its deletion to the frames after it. The
    /// issuer of a deletion by user comes from the control store, which the frame waits for. A frame that does not
    /// decode applies nothing; the requests read from the control store at start still apply.
    async fn on_tombstone(&mut self, frame: &WalFrame) -> Learned {
        let lsn = frame.lsn();
        let decoded = frame.payload().ok().and_then(|payload| {
            match Record::decode(RecordType::Tombstone, &payload) {
                Ok(Record::Tombstone(record)) => Some(tombstone(&record)),
                _ => None,
            }
        });
        let Some(mut tombstone) = decoded else {
            tracing::warn!(lsn = %lsn, "a tombstone frame does not decode, so stage B does not apply it");
            return Learned::Applied;
        };
        if let TombstoneSelector::User { issuer, .. } = &mut tombstone.selector {
            match self.issuers.get(&tombstone.request_id) {
                Some(known) => issuer.clone_from(known),
                None if self.ctx.deletions.configured() => {
                    self.lap(Phase::StageB);
                    let read = self
                        .ctx
                        .deletions
                        .request(tombstone.project_id, tombstone.request_id)
                        .await;
                    self.lap(Phase::WaitFrames);
                    match read {
                        // A request that is not there names no issuer.
                        Ok(found) => *issuer = found.and_then(|known| issuer_of(&known.tombstone)),
                        Err(e) => {
                            tracing::warn!(lsn = %lsn, error = %e, "a tombstone waits for the issuer of its deletion");
                            return Learned::Waits;
                        }
                    }
                }
                // Without a control store no MCP operation keeps an identity, so the issuer changes nothing.
                None => {}
            }
        }
        self.deletion_failures = 0;
        self.learn(tombstone);
        Learned::Applied
    }

    /// Reads a frame again after a backoff, because what it needs from the control store did not come.
    async fn wait_for_deletions(&mut self, lsn: Lsn) {
        self.read_again_from(lsn);
        self.ctx.pause(self.deletion_failures).await;
        self.lap(Phase::WaitFrames);
        self.deletion_failures = self.deletion_failures.saturating_add(1);
    }

    /// Starts the read again at `lsn`, the frame stage B could not take yet: the frames read ahead of it are
    /// dropped and read again after it.
    fn read_again_from(&mut self, lsn: Lsn) {
        self.window.clear();
        self.replay = self.ctx.wal.replay(lsn);
    }

    /// The LSN of the frame stage B takes next.
    fn next_lsn(&self) -> Lsn {
        self.window
            .head_lsn()
            .unwrap_or_else(|| self.replay.position())
    }

    /// Reads one more frame into the window while it wants one, then gives stage B the window's first frame once
    /// its stage A is done. Stage B takes every frame in LSN order. Where stage A runs in place, as the
    /// simulation's spawner runs blocking work, the window holds one frame at a time, read and taken in one step.
    async fn next_ready(&mut self) -> Result<Next, ShipError> {
        let mut caught_up = false;
        if self.window.wants_frame() {
            match self.next_frame().await? {
                Some(frame) => {
                    let norm = self.norm_ctx();
                    self.window.push(&self.ctx, frame, norm);
                    self.ctx.stats.ahead(self.window.len());
                    self.lap(Phase::StageA);
                }
                None => caught_up = true,
            }
        }
        if let Some(ready) = self.window.take_ready()? {
            return Ok(Next::Frame(ready));
        }
        Ok(if !caught_up && self.window.wants_frame() {
            Next::Read
        } else {
            Next::Wait { caught_up }
        })
    }

    /// The next durable frame, or `None` when there is none yet or the read failed. A failed read is tried
    /// again from the same position: the cursor never moves past a frame it did not return.
    async fn next_frame(&mut self) -> Result<Option<WalFrame>, ShipError> {
        let read = self.replay.next().await;
        self.lap(Phase::Read);
        match read {
            Ok(frame) => {
                self.read_failures = 0;
                Ok(frame)
            }
            Err(ReplayError::Reclaimed { lsn, .. }) => Err(ShipError::Reclaimed(lsn)),
            Err(e) => {
                let position = self.replay.position();
                tracing::warn!(lsn = %position, error = %e, "wal read failed; retrying");
                self.replay = self.ctx.wal.replay(position);
                self.ctx.pause(self.read_failures).await;
                self.lap(Phase::WaitFrames);
                self.read_failures = self.read_failures.saturating_add(1);
                Ok(None)
            }
        }
    }

    /// Recovery step 6: rebuilds the duplicate window and the first-seen set by replaying the retained prefix
    /// through stage B, before any garbage collection can shorten it.
    async fn rebuild(&mut self) -> Result<(), ShipError> {
        let shutdown = self.ctx.env.shutdown();
        while self.next_lsn() <= self.watermark && !shutdown.is_shutdown() {
            let (frame, read) = match self.next_ready().await? {
                Next::Frame(ready) => ready,
                Next::Read => continue,
                // Every frame up to the watermark is durable: only a failed read, which waited already, leaves
                // the window without a frame whose stage A runs.
                Next::Wait { .. } => {
                    if self.window.head_runs() {
                        self.window.settled().await;
                        self.lap(Phase::WaitStageA);
                    }
                    continue;
                }
            };
            if frame.record_type() == RecordType::Tombstone
                && self.on_tombstone(&frame).await == Learned::Waits
            {
                self.wait_for_deletions(frame.lsn()).await;
                continue;
            }
            let Some(read) = read else {
                self.ctx.recent.passed(&frame);
                self.lap(Phase::StageB);
                continue;
            };
            if !self.cover(frame.received_at()).await {
                self.wait_for_deletions(frame.lsn()).await;
                continue;
            }
            match read {
                Ok(Content::Spans(mut normalized)) => {
                    // The rows are not shipped again, and no salt changes what stage B remembers. What a deletion
                    // selects stays out of the recent index.
                    let ctx = stage_b_ctx(&self.ctx, &self.tombstones, IdentityCtx::Salted(None));
                    let admission = self
                        .stage_b
                        .admit_with(&mut normalized, &ctx)
                        .map_err(|e| frame_error(frame.lsn(), e))?;
                    self.ctx.recent.applied(&frame, &normalized, &admission);
                }
                Ok(Content::Facts(_)) => self.ctx.recent.passed(&frame),
                // The frame was quarantined before the watermark passed it; the decision is the same now
                // (contract 5.6).
                Err(why) => self.ctx.recent.unshippable(&frame, why.project, why.facts),
            }
            self.lap(Phase::StageB);
        }
        Ok(())
    }

    /// Whether no batch in flight holds rows any more.
    fn all_released(&self) -> bool {
        self.in_flight.iter().all(|batch| batch.released)
    }

    /// Whether the budget of `shipper.encoded_bytes` has room for `bytes` more.
    fn budget_admits(&self, bytes: u64) -> bool {
        self.ctx.stats.held_bytes().saturating_add(bytes) <= self.ctx.config.encoded_bytes
    }

    /// The rows of a held frame in the open batch.
    fn routed(&self, held: &mut Held) -> Result<Vec<RoutedRow>, ShipError> {
        let batch_seq = self.open.first_lsn();
        let granularity = self.ctx.config.granularity;
        match &mut held.rows {
            Unplaced::Spans(frame, admission) => {
                frame.stamp_batch_seq(batch_seq);
                let defects = Defects {
                    count_aggregates: self.ctx.config.counts_aggregate_usage(),
                    sum_capture_sources: self.ctx.config.sums_capture_sources(),
                };
                derive_rows(frame, admission, granularity, defects)
            }
            Unplaced::Facts(facts) => {
                for fact in facts.iter_mut() {
                    fact.envelope.ingestion.batch_seq = batch_seq;
                }
                derive_facts(facts, granularity)
            }
        }
        .map_err(|e| frame_error(held.lsn, e))
    }

    /// Puts a frame's rows into the open batch, or holds the frame: when a builder would pass the block limit
    /// the batch seals first, because frames are never split (contract 5.1), and when the rows would pass the
    /// `shipper.encoded_bytes` budget the frame waits until the open batch is sealed and a batch in flight has
    /// resolved. A batch that recovery found takes every frame of its recorded range.
    fn place(&mut self, mut held: Held) -> Result<(), ShipError> {
        let lsn = held.lsn;
        let rows = self.routed(&mut held)?;
        let row_bytes: u64 = rows.iter().map(|r| r.row.size()).sum();
        if self.seals.is_empty() {
            let fits = self.open.fits(&rows);
            if !fits && !self.open.is_sealable() {
                return Err(ShipError::FrameTooLarge(lsn));
            }
            let waits =
                !self.budget_admits(row_bytes) && (self.open.is_sealable() || !self.all_released());
            if !fits || waits {
                held.row_bytes = Some(row_bytes);
                self.held = Some(held);
                return Ok(());
            }
        }
        let now = self.ctx.env.clock.now();
        self.open
            .take_frame(lsn, held.received_at, held.raw_bytes, true, now);
        let count = match &held.rows {
            Unplaced::Spans(frame, _) => frame.spans.len(),
            Unplaced::Facts(facts) => facts.len(),
        };
        self.open.add_rows(rows, count);
        self.ctx.stats.charge(row_bytes);
        Ok(())
    }

    /// Stage B of the next frame of the log, then its rows into the open batch. `read` is what stage A made of a
    /// frame that carries rows, and `None` for any other frame.
    async fn on_frame(&mut self, frame: &WalFrame, read: Option<Read>) -> Result<(), ShipError> {
        let lsn = frame.lsn();
        let raw_bytes = frame.end_offset().saturating_sub(frame.offset());
        let record_type = frame.record_type();
        if record_type == RecordType::Tombstone && self.on_tombstone(frame).await == Learned::Waits
        {
            self.wait_for_deletions(lsn).await;
            return Ok(());
        }
        let Some(read) = read else {
            // Control frames contribute no rows. The shipper's own do not warrant a seal either; any other
            // frame does, so that the watermark passes it (a tombstone waits for exactly that).
            let own = matches!(
                record_type,
                RecordType::BatchSeal | RecordType::Checkpoint | RecordType::Quarantine
            );
            self.ctx.recent.passed(frame);
            let now = self.ctx.env.clock.now();
            self.open
                .take_frame(lsn, frame.received_at(), raw_bytes, !own, now);
            self.lap(Phase::StageB);
            return Ok(());
        };
        if !self.cover(frame.received_at()).await {
            self.wait_for_deletions(lsn).await;
            return Ok(());
        }
        let rows = match read {
            Ok(Content::Spans(mut normalized)) => {
                let Some(identity) = self.identity_of(&normalized).await else {
                    // Stage B has not seen the frame: it is read again once the backoff has passed.
                    self.read_again_from(lsn);
                    self.ctx.pause(self.salt_failures).await;
                    self.lap(Phase::WaitFrames);
                    self.salt_failures = self.salt_failures.saturating_add(1);
                    return Ok(());
                };
                let ctx = stage_b_ctx(&self.ctx, &self.tombstones, identity.ctx());
                let admission = self
                    .stage_b
                    .admit_with(&mut normalized, &ctx)
                    .map_err(|e| frame_error(lsn, e))?;
                let deleted_ops =
                    u64::from(admission.deleted_span_ops) + u64::from(admission.deleted_ops);
                if admission.deleted > 0 || deleted_ops > 0 {
                    let stats = &self.ctx.stats;
                    stats.deleted_spans.add(u64::from(admission.deleted));
                    stats.deleted_ops.add(deleted_ops);
                }
                if normalized.summary.inventory_dropped > 0 {
                    self.ctx
                        .stats
                        .inventory_dropped
                        .add(u64::from(normalized.summary.inventory_dropped));
                }
                self.ctx.recent.applied(frame, &normalized, &admission);
                if let Some(at) = admission.relies_on {
                    self.relied_on.insert(lsn, at);
                }
                Unplaced::Spans(normalized, admission)
            }
            Ok(Content::Facts(mut facts)) => {
                if !self.ctx.config.skips_tombstone_filter() && !self.tombstones.is_empty() {
                    let before = facts.len();
                    facts.retain(|fact| !self.tombstones.selects_fact(fact));
                    let deleted = before - facts.len();
                    if deleted > 0 {
                        self.ctx.stats.deleted_facts.add(deleted as u64);
                    }
                }
                self.ctx.recent.passed(frame);
                Unplaced::Facts(facts)
            }
            Err(why) => {
                // Frame quarantine (contract 5.6): the copy is durable before the frame joins a batch, so
                // before any checkpoint can pass it. From there the frame yields no rows.
                quarantine_frame(&self.ctx, frame, &why).await?;
                // The copy retries a failing disk; like the control store's reads, it is not counted as work.
                self.lap(Phase::WaitFrames);
                self.ctx.recent.unshippable(frame, why.project, why.facts);
                let now = self.ctx.env.clock.now();
                self.open
                    .take_frame(lsn, frame.received_at(), raw_bytes, true, now);
                self.lap(Phase::StageB);
                return Ok(());
            }
        };
        self.lap(Phase::StageB);
        let placed = self.place(Held {
            lsn,
            rows,
            received_at: frame.received_at(),
            raw_bytes,
            row_bytes: None,
        });
        self.lap(Phase::Rows);
        placed
    }

    /// Whether the open batch takes another frame.
    fn can_read(&self) -> bool {
        match self.seals.front() {
            // A batch that recovery found takes every frame of its range; while the budget is exhausted it
            // waits for the batches in flight before it takes the next one.
            Some(seal) => {
                self.open.last_lsn() < seal.last_lsn
                    && (self.all_released() || self.budget_admits(1))
            }
            None => {
                self.held.is_none()
                    && !self
                        .open
                        .is_full(&self.ctx.limits(), self.ctx.config.seal_bytes)
            }
        }
    }

    fn seal_due(&self) -> bool {
        if self.in_flight.len() >= self.ctx.config.max_batches_in_flight.max(1) {
            return false;
        }
        match self.seals.front() {
            Some(seal) => self.open.last_lsn() == seal.last_lsn,
            None => {
                let now = self.ctx.env.clock.now();
                // A held frame did not fit: the batch seals before it, whatever its size.
                (self.held.is_some() && self.open.is_sealable())
                    || self
                        .open
                        .is_due(&self.ctx.limits(), self.ctx.config.seal_bytes, now)
            }
        }
    }

    /// Seals the open batch and starts shipping it. A batch that recovery found keeps its seal; a new one gets a
    /// `BatchSeal` frame that is durable before any insert is sent (contract 5.1).
    async fn seal(&mut self) -> Result<(), ShipError> {
        let ctx = Arc::clone(&self.ctx);
        let recovered = self.seals.pop_front();
        let id = BatchId {
            epoch: ctx.wal.epoch(),
            first_lsn: self.open.first_lsn(),
            last_lsn: self.open.last_lsn(),
        };
        let next = OpenBatch::new(Lsn::new(id.last_lsn.get().saturating_add(1)));
        let batch = std::mem::replace(&mut self.open, next);
        let (raw_bytes, row_bytes) = (batch.raw_bytes(), batch.row_bytes());
        if let Some(seal) = recovered {
            ctx.stats.reshipped.add(1);
            if seal.normalizer_version != NORMALIZER_VERSION
                || seal.price_table_version != ctx.prices.version()
            {
                // Invariant I7's stated exception: pairs of this batch that landed before the restart were
                // built by another version.
                ctx.stats.mixed_version_batches.add(1);
            }
        } else {
            let seal = BatchSeal {
                first_lsn: id.first_lsn,
                last_lsn: id.last_lsn,
                frame_count: u32::try_from(id.last_lsn.get() - id.first_lsn.get() + 1)
                    .unwrap_or(u32::MAX),
                normalizer_version: NORMALIZER_VERSION,
                price_table_version: ctx.prices.version(),
                // The deletions stage B applied to the range are those it knew when it read the range's last
                // frame.
                deletion_seq: self.tombstones.highest_seq(),
            };
            ctx.barriers.reached(Barrier::BeforeSealAppend).await;
            let record = Record::BatchSeal(seal);
            if ctx.config.sends_before_seal() {
                drop(ctx.wal.append(Ctx::prepare(&record)?));
            } else {
                ctx.append_durable(&record).await?;
            }
            ctx.barriers.reached(Barrier::SealAppended).await;
            ctx.stats.sealed.add(1);
        }
        self.lap(Phase::WalAppend);
        let marker_at = batch.marker_at();
        let compression = ctx.config.compression;
        let mut jobs = Vec::new();
        for (pair, mut rows) in batch.into_pairs() {
            if self
                .quarantined
                .contains(&(id.first_lsn, id.last_lsn, pair.0, pair.1))
            {
                continue;
            }
            let rollup = matches!(
                pair.0,
                TableId::SessionRollup | TableId::UserRollup | TableId::SpanMetrics
            );
            if rollup && ctx.config.double_counts_rollups() {
                rows.extend(rows.first().cloned());
            }
            let marker = !(pair.0 == TableId::McpToolRollup && ctx.config.drops_rollup_marker());
            let token = pair_token(&id, pair)?;
            // The block is encoded once, by a job that owns the rows and frees them, and every attempt of the
            // pair sends those bytes (contract 5.2).
            let encoded = ctx.encode.run(&ctx.env, move || {
                block(&id, pair, rows, marker_at, compression, marker)
            });
            jobs.push(PairJob {
                token,
                block: encoded,
                in_doubt: recovered.is_some(),
            });
        }
        let done = self.done_tx.clone();
        let task_ctx = Arc::clone(&ctx);
        let task = ctx.env.spawn("shipper-batch", async move {
            let result = ship_batch(&task_ctx, &id, jobs).await;
            // The receiver is gone only when the shipper is.
            let _gone = done.send((id.first_lsn, result));
        });
        let state = if ctx.config.checkpoints_before_resolution() {
            BatchState::Resolved
        } else {
            BatchState::Shipping
        };
        self.in_flight.push_back(InFlight {
            id,
            state,
            task,
            raw_bytes,
            row_bytes,
            released: false,
        });
        self.lap(Phase::Encode);
        Ok(())
    }

    fn on_done(&mut self, (first_lsn, result): BatchDone) -> Result<(), ShipError> {
        let state = result?;
        if let Some(batch) = self
            .in_flight
            .iter_mut()
            .find(|b| b.id.first_lsn == first_lsn)
        {
            batch.state = state;
            // The batch's task has ended and dropped its blocks.
            if !std::mem::replace(&mut batch.released, true) {
                self.ctx.stats.release(batch.row_bytes);
            }
        }
        Ok(())
    }

    /// Appends the checkpoint of every batch that is resolved together with every earlier batch
    /// (contract 5.7), then collects garbage.
    async fn checkpoint_resolved(&mut self) -> Result<(), ShipError> {
        while let Some(batch) = self.in_flight.front()
            && batch.state == BatchState::Resolved
        {
            let ctx = Arc::clone(&self.ctx);
            let checkpoint = Checkpoint {
                batch_seq: batch.id.first_lsn.get(),
                last_lsn: batch.id.last_lsn,
                table_bitmap: Checkpoint::ALL_TABLES,
            };
            ctx.barriers.reached(Barrier::BeforeCheckpointAppend).await;
            ctx.append_durable(&Record::Checkpoint(checkpoint)).await?;
            ctx.barriers.reached(Barrier::CheckpointAppended).await;
            ctx.stats.checkpoints.add(1);
            let drained = batch.raw_bytes;
            self.in_flight.pop_front();
            self.drain.record(ctx.env.clock.now(), drained);
            self.report_drain();
            self.collect_garbage().await;
        }
        Ok(())
    }

    /// Reports the drain rate to admission through the WAL (contract 4.8), and as a metric.
    fn report_drain(&mut self) {
        let rate = self.drain.rate(self.ctx.env.clock.now());
        self.ctx.wal.set_drain_rate(rate);
        self.ctx
            .stats
            .drain_gauge
            .set(i64::try_from(rate).unwrap_or(i64::MAX));
    }

    /// Removes the segments below the watermark that neither replay retention, the recent index nor stage B
    /// holds (contract 4.7). A failure is logged; the next run tries again.
    ///
    /// Stage B holds what an unchecked frame's admission relied on: a restart rebuilds stage B from the
    /// retained prefix and normalizes the unchecked batches again, and pairs of one batch that land before and
    /// after it must hold the same spans. For the same reason nothing is collected while a batch that recovery
    /// found still waits for its frames, and stage B forgets what the collection removed.
    async fn collect_garbage(&mut self) {
        self.last_gc = self.ctx.env.clock.now();
        if !self.seals.is_empty() {
            return;
        }
        let watermark = self.ctx.wal.watermark().map_or(Lsn::ZERO, |w| w.last_lsn());
        self.relied_on = self
            .relied_on
            .split_off(&Lsn::new(watermark.get().saturating_add(1)));
        let relied_on = if self.ctx.config.collects_what_frames_rely_on() {
            None
        } else {
            self.relied_on.values().copied().min()
        };
        let floor = relied_on.into_iter().chain(self.ctx.recent.floor()).min();
        let collected = self.ctx.wal.gc(floor).await;
        self.gc_in_doubt = collected.is_err();
        match collected {
            Ok(_) => self.gc_failures = GcFailures::default(),
            // A collection in doubt is tried again at every tail poll; the log gets one line per interval.
            Err(e) => {
                if let Some(failures) = self
                    .gc_failures
                    .failed(self.last_gc, self.ctx.config.gc_interval)
                {
                    tracing::warn!(error = %e, failures, "wal garbage collection failed");
                }
            }
        }
        // A failed collection may have removed segments too.
        if let Some(oldest) = self.ctx.wal.segments().first() {
            self.stage_b.forget_below(oldest.first_lsn);
        }
    }

    /// Runs the shipper until the host shuts down.
    ///
    /// On start it rebuilds stage B from the retained prefix, ships the sealed batches recovery found (every pair
    /// probed first, because everything before a restart is in doubt), then normalizes and seals the tail with
    /// fresh boundaries and keeps tailing the log. Before stage B admits a frame it reads the deletion requests of
    /// the control store that can select it.
    pub async fn run(mut self) -> Result<(), ShipError> {
        let mut shutdown = self.ctx.env.shutdown();
        self.laps.start(self.ctx.env.clock.now());
        self.rebuild().await?;
        loop {
            if shutdown.is_shutdown() {
                self.lap(Phase::WaitFrames);
                return Ok(());
            }
            while let Ok(done) = self.done_rx.try_recv() {
                self.on_done(done)?;
            }
            self.checkpoint_resolved().await?;
            self.lap(Phase::WalAppend);
            if self.seal_due() {
                self.seal().await?;
                continue;
            }
            let room = self.all_released()
                || self
                    .held
                    .as_ref()
                    .is_some_and(|h| self.budget_admits(h.row_bytes.unwrap_or(0)));
            if let Some(held) = self.held.take_if(|_| !self.open.is_sealable() && room) {
                // The batch the frame did not fit is sealed, and the budget has room: the frame opens the
                // next one.
                self.place(held)?;
                self.lap(Phase::Rows);
                continue;
            }
            // What the wait below waits for: frames, unless a full batch or a frame held for the budget waits
            // for the batches in flight, or stage A of the next frame still runs. A collection in doubt waits for
            // the next attempt, which is no work of the task either.
            let mut waiting = Phase::WaitFrames;
            // The open batch takes another frame and the durable log has none: the wait also ends when one
            // becomes durable, with the timer left to the linger, garbage collection and maintenance.
            let mut caught_up = false;
            if !self.gc_in_doubt {
                if !self.can_read() {
                    waiting = Phase::WaitInserts;
                } else {
                    match self.next_ready().await? {
                        Next::Frame((frame, read)) => {
                            self.on_frame(&frame, read).await?;
                            continue;
                        }
                        Next::Read => continue,
                        Next::Wait {
                            caught_up: exhausted,
                        } => {
                            caught_up = exhausted;
                            if self.window.head_runs() {
                                waiting = Phase::WaitStageA;
                            }
                        }
                    }
                }
            }
            let now = self.ctx.env.clock.now();
            if self.gc_in_doubt
                || now.saturating_duration_since(self.last_gc) >= self.ctx.config.gc_interval
            {
                self.report_drain();
                self.collect_garbage().await;
                self.lap(Phase::WalAppend);
            }
            // The wait checks the durable LSN again after it registers for the committer's wake, so a frame that
            // became durable since the read above, during the collection for one, ends it at once.
            let replay = &self.replay;
            let durable = async {
                if caught_up {
                    replay.wait_durable().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            // The end of the next frame's stage A ends the wait too, so stage B takes the frame at once.
            let stage_a = waiting == Phase::WaitStageA;
            let window = &mut self.window;
            let settled = async {
                if stage_a {
                    window.settled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let mut tick = self.ctx.env.clock.sleep(self.ctx.config.tail_poll).fuse();
            select_biased! {
                done = self.done_rx.recv().fuse() => {
                    if let Some(done) = done {
                        self.on_done(done)?;
                    }
                }
                () = settled.fuse() => {}
                () = durable.fuse() => {}
                () = tick => {}
                () = (&mut shutdown).fuse() => {
                    self.lap(waiting);
                    return Ok(());
                }
            }
            self.lap(waiting);
        }
    }
}

/// Ships every pair of one batch concurrently; the connection gate bounds the inserts in flight.
async fn ship_batch(
    ctx: &Arc<Ctx>,
    id: &BatchId,
    jobs: Vec<PairJob>,
) -> Result<BatchState, ShipError> {
    let mut pairs: FuturesUnordered<_> = jobs
        .into_iter()
        .map(|job| ship_pair(ctx, id, job))
        .collect();
    let mut state = BatchState::Resolved;
    while let Some(end) = pairs.next().await {
        if end? == PairEnd::Stopped {
            state = BatchState::Stopped;
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_collection_is_logged_once_per_interval_with_its_count() {
        let every = Duration::from_millis(10);
        let at = |millis: u64| Instant::from_nanos(millis * 1_000_000);
        let mut failures = GcFailures::default();
        // The first failure is logged at once.
        assert_eq!(failures.failed(at(0), every), Some(1));
        // The retries of the following tail polls are counted, not logged.
        for millis in 1..10 {
            assert_eq!(failures.failed(at(millis), every), None);
        }
        assert_eq!(failures.failed(at(10), every), Some(10));
        assert_eq!(failures.failed(at(19), every), None);
        assert_eq!(failures.failed(at(25), every), Some(2));
        // A success starts over.
        failures = GcFailures::default();
        assert_eq!(failures.failed(at(26), every), Some(1));
    }
}
