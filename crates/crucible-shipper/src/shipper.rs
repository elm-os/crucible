//! The shipper task: tail the durable WAL, normalize, fill and seal batches, ship their pairs, checkpoint, and
//! drive garbage collection (contract sections 4.6, 5.1, 5.7 and 5.9).

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crucible_env::metrics::registry;
use crucible_env::{Counter, Env, Gauge, Instant, JoinHandle, Metrics, Rng, Stores};
use crucible_normalize::{
    Admission, NORMALIZER_VERSION, NormCtx, NormalizedFrame, PriceTable, StageB,
};
use crucible_records::{Fact, Lsn, PartitionId, TableId, Timestamp};
use crucible_wal::{AppendError, RecoveryReport, Replay, ReplayError, Wal, WalFrame};
use crucible_wal_codec::{BatchSeal, Checkpoint, Compression, PreparedFrame, Record, RecordType};
use futures::stream::{FuturesUnordered, StreamExt};
use futures::{FutureExt, select_biased};
use tokio::sync::mpsc;

use crate::batch::{BatchId, OpenBatch, pair_block, pair_token};
use crate::config::ShipperConfig;
use crate::error::ShipError;
use crate::frames::{Content, read_frame};
use crate::hooks::{Barrier, Barriers, NoBarriers, NoRecent, RecentIndex};
use crate::limits::{DrainRate, Gate, SealLimits, backoff};
use crate::pair::{PairEnd, PairJob, ship_pair};
use crate::quarantine::quarantine_frame;
use crate::tables::{RoutedRow, derive_facts, derive_rows};

/// What the shipper takes besides `Env`, `Stores` and the WAL.
#[derive(Clone, Debug)]
pub struct Hooks {
    /// The crash barriers; [`NoBarriers`] in production.
    pub barriers: Arc<dyn Barriers>,
    /// The recent-span index: fed by stage B, and its floor pins WAL segments.
    pub recent: Arc<dyn RecentIndex>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            barriers: Arc::new(NoBarriers),
            recent: Arc::new(NoRecent),
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
    /// The rows held now, in estimated bytes (the `shipper.encoded_bytes` budget), with its gauge.
    encoded_bytes: AtomicU64,
    encoded_gauge: Gauge,
    encoded_peak: AtomicU64,
    largest_frame: AtomicU64,
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
    /// The most estimated row bytes held at once (the `shipper.encoded_bytes` budget).
    pub encoded_bytes_peak: u64,
    /// The estimated row bytes of the largest frame placed.
    pub largest_frame_bytes: u64,
}

impl ShipperStats {
    fn new(m: &dyn Metrics) -> Self {
        let batches = |kind| Tally::new(m.counter(&registry::SHIPPER_BATCHES, &[kind]));
        let inserts = |outcome| Tally::new(m.counter(&registry::SHIPPER_INSERTS, &[outcome]));
        let probes = |result| Tally::new(m.counter(&registry::SHIPPER_PROBES, &[result]));
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
            encoded_bytes: AtomicU64::new(0),
            encoded_gauge,
            encoded_peak: AtomicU64::new(0),
            largest_frame: AtomicU64::new(0),
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
            encoded_bytes_peak: self.encoded_peak.load(Ordering::Relaxed),
            largest_frame_bytes: self.largest_frame.load(Ordering::Relaxed),
        }
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
    pub(crate) gate: Gate,
    pub(crate) rng: Arc<dyn Rng>,
    pub(crate) stats: Arc<ShipperStats>,
    limits: Mutex<SealLimits>,
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
    /// Frames at or below this LSN only rebuild stage B (contract 4.6, step 6).
    watermark: Lsn,
    /// Sealed batches recovery found, oldest first: their boundaries are kept and their pairs are in doubt.
    seals: VecDeque<BatchSeal>,
    quarantined: BTreeSet<QuarantinedPair>,
    open: OpenBatch,
    held: Option<Held>,
    in_flight: VecDeque<InFlight>,
    done_tx: mpsc::UnboundedSender<BatchDone>,
    done_rx: mpsc::UnboundedReceiver<BatchDone>,
    last_gc: Instant,
    read_failures: u32,
    drain: DrainRate,
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
        let ctx = Arc::new(Ctx {
            env: env.clone(),
            stores: stores.clone(),
            prices,
            limits: Mutex::new(SealLimits::new(&config)),
            gate: Gate::new(config.connections),
            rng: env.rng.fork("crucible-shipper/backoff"),
            stats: Arc::new(ShipperStats::new(env.metrics.as_ref())),
            barriers: hooks.barriers,
            recent: hooks.recent,
            wal,
            config,
        });
        Ok(Self {
            replay: ctx.wal.replay(report.first_retained_lsn()),
            stage_b: StageB::new(
                ctx.config.duplicate_window_keys,
                ctx.config.first_seen_traces,
            ),
            watermark,
            seals,
            quarantined,
            open: OpenBatch::new(Lsn::new(watermark.get().saturating_add(1))),
            held: None,
            in_flight: VecDeque::new(),
            done_tx,
            done_rx,
            last_gc: ctx.env.clock.now(),
            read_failures: 0,
            drain: DrainRate::new(ctx.env.clock.now()),
            ctx,
        })
    }

    /// The shipper's counters.
    pub fn stats(&self) -> Arc<ShipperStats> {
        Arc::clone(&self.ctx.stats)
    }

    /// The stage A context. This build has one price table, so a batch that an earlier build sealed with another
    /// version is priced with this build's table, as it is normalized by this build's normalizer (contract 5.3):
    /// its pairs land wholesale.
    fn norm_ctx(&self) -> NormCtx<'static> {
        NormCtx::new(self.ctx.prices, self.ctx.config.caps)
    }

    /// The next durable frame, or `None` when there is none yet or the read failed. A failed read is tried
    /// again from the same position: the cursor never moves past a frame it did not return.
    async fn next_frame(&mut self) -> Result<Option<WalFrame>, ShipError> {
        match self.replay.next().await {
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
                self.read_failures = self.read_failures.saturating_add(1);
                Ok(None)
            }
        }
    }

    /// Recovery step 6: rebuilds the duplicate window and the first-seen set by replaying the retained prefix
    /// through stage B, before any garbage collection can shorten it.
    async fn rebuild(&mut self) -> Result<(), ShipError> {
        let shutdown = self.ctx.env.shutdown();
        while self.replay.position() <= self.watermark && !shutdown.is_shutdown() {
            let Some(frame) = self.next_frame().await? else {
                continue;
            };
            if !matches!(
                frame.record_type(),
                RecordType::IngestRequest | RecordType::FactBatch
            ) {
                self.ctx.recent.passed(&frame);
                continue;
            }
            match read_frame(&self.ctx.wal, &frame, &self.norm_ctx(), &self.ctx.config) {
                Ok(Content::Spans(mut normalized)) => {
                    let admission = self
                        .stage_b
                        .admit(&mut normalized)
                        .map_err(|e| frame_error(frame.lsn(), e))?;
                    self.ctx.recent.applied(&frame, &normalized, &admission);
                }
                Ok(Content::Facts(_)) => self.ctx.recent.passed(&frame),
                // The frame was quarantined before the watermark passed it; the decision is the same now
                // (contract 5.6).
                Err(why) => self.ctx.recent.unshippable(&frame, why.project, why.facts),
            }
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
                {
                    let count_aggregates = self.ctx.config.counts_aggregate_usage();
                    derive_rows(frame, admission, granularity, count_aggregates)
                }
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

    async fn on_frame(&mut self, frame: &WalFrame) -> Result<(), ShipError> {
        let lsn = frame.lsn();
        let raw_bytes = frame.end_offset().saturating_sub(frame.offset());
        let record_type = frame.record_type();
        if !matches!(
            record_type,
            RecordType::IngestRequest | RecordType::FactBatch
        ) {
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
            return Ok(());
        }
        let rows = match read_frame(&self.ctx.wal, frame, &self.norm_ctx(), &self.ctx.config) {
            Ok(Content::Spans(mut normalized)) => {
                let admission = self
                    .stage_b
                    .admit(&mut normalized)
                    .map_err(|e| frame_error(lsn, e))?;
                self.ctx.recent.applied(frame, &normalized, &admission);
                Unplaced::Spans(normalized, admission)
            }
            Ok(Content::Facts(facts)) => {
                self.ctx.recent.passed(frame);
                Unplaced::Facts(facts)
            }
            Err(why) => {
                // Frame quarantine (contract 5.6): the copy is durable before the frame joins a batch, so
                // before any checkpoint can pass it. From there the frame yields no rows.
                quarantine_frame(&self.ctx, frame, &why).await?;
                self.ctx.recent.unshippable(frame, why.project, why.facts);
                let now = self.ctx.env.clock.now();
                self.open
                    .take_frame(lsn, frame.received_at(), raw_bytes, true, now);
                return Ok(());
            }
        };
        self.place(Held {
            lsn,
            rows,
            received_at: frame.received_at(),
            raw_bytes,
            row_bytes: None,
        })
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
                // Milestone 1 applies no tombstones while normalizing.
                deletion_seq: 0,
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
        let marker_at = batch.marker_at();
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
            jobs.push(PairJob {
                token: pair_token(&id, pair)?,
                block: pair_block(&id, pair, rows, marker_at, ctx.config.compression)?,
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

    /// Removes the segments below the watermark that neither replay retention nor the recent index holds
    /// (contract 4.7). A failure is logged; the next run tries again.
    async fn collect_garbage(&mut self) {
        self.last_gc = self.ctx.env.clock.now();
        if let Err(e) = self.ctx.wal.gc(self.ctx.recent.floor()).await {
            tracing::warn!(error = %e, "wal garbage collection failed");
        }
    }

    /// Runs the shipper until the host shuts down.
    ///
    /// On start it rebuilds stage B from the retained prefix, ships the sealed batches recovery found (every
    /// pair probed first, because everything before a restart is in doubt), then normalizes and seals the tail
    /// with fresh boundaries and keeps tailing the log.
    pub async fn run(mut self) -> Result<(), ShipError> {
        let mut shutdown = self.ctx.env.shutdown();
        self.rebuild().await?;
        loop {
            if shutdown.is_shutdown() {
                return Ok(());
            }
            while let Ok(done) = self.done_rx.try_recv() {
                self.on_done(done)?;
            }
            self.checkpoint_resolved().await?;
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
                continue;
            }
            if self.can_read()
                && let Some(frame) = self.next_frame().await?
            {
                self.on_frame(&frame).await?;
                continue;
            }
            let now = self.ctx.env.clock.now();
            if now.saturating_duration_since(self.last_gc) >= self.ctx.config.gc_interval {
                self.report_drain();
                self.collect_garbage().await;
            }
            let mut tick = self.ctx.env.clock.sleep(self.ctx.config.tail_poll).fuse();
            select_biased! {
                done = self.done_rx.recv().fuse() => {
                    if let Some(done) = done {
                        self.on_done(done)?;
                    }
                }
                () = tick => {}
                () = (&mut shutdown).fuse() => return Ok(()),
            }
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
