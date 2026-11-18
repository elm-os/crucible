//! Deterministic simulation of `crucible-wal`: seeded appends, group formation, checkpoints, garbage
//! collection, disk faults and crashes at seeded await points, checked by an oracle for the WAL
//! invariants.
//!
//! - **I1** every acknowledged frame above the recovered watermark is in the recovered log, intact, at
//!   its LSN; frames a recovery returned stay in the log; the log holds no frame that was not submitted
//!   in the incarnation that owns its LSN, and no valid frame is left beyond the end of the recovered log;
//!   nothing above the watermark is removed.
//! - **I12** a recovery interrupted by crashes ends in the state one uninterrupted recovery reaches (the
//!   reference runs on a fault-free copy of the crashed disk); a second recovery repairs nothing; the
//!   bytes scanned are bounded by the retained segments.
//! - **I13** at quiescence no segment wholly at or below the watermark is left.
//! - **I14** the run is a function of the seed: its trace hash is reproducible.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crucible_env::{DiskPath, Env, Rng, Root};
use crucible_records::Lsn;
use crucible_wal::{
    AppendError, GcReport, Mutant, RecoveryError, RecoveryReport, SegmentMeta, Wal, WalConfig,
    WalFrame,
};
use crucible_wal_codec::{
    BatchSeal, Checkpoint, Compression, FRAME_HEADER_LEN, FRAME_MAGIC, PreparedFrame, Record,
    RecordType, SEGMENT_HEADER_LEN, Scan, scan_frame,
};
use futures::FutureExt;

use crate::disk::{DiskFaults, SimDisk};
use crate::runtime::{Host, Sim, run};

/// How hostile the disk is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Every fault of contract 2.4 except the fsync lie. I1 must hold.
    Realistic,
    /// The realistic faults plus the fsync lie. The oracle must notice the loss.
    Stress,
}

/// What one run does besides what its seed decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// The fault tier.
    pub tier: Tier,
    /// A seeded defect of `crucible-wal` that the run must catch.
    pub mutant: Option<Mutant>,
}

impl Options {
    /// The realistic tier without a mutant.
    pub const REALISTIC: Self = Self {
        tier: Tier::Realistic,
        mutant: None,
    };

    /// The stress tier without a mutant.
    pub const STRESS: Self = Self {
        tier: Tier::Stress,
        mutant: None,
    };

    /// The realistic tier with a seeded defect.
    pub const fn mutant(mutant: Mutant) -> Self {
        Self {
            tier: Tier::Realistic,
            mutant: Some(mutant),
        }
    }
}

/// An invariant violation found by the oracle.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Violation {
    /// I1: an acknowledged frame above the watermark is not in the recovered log.
    AckedFrameLost {
        /// The frame's LSN.
        lsn: u64,
    },
    /// I1: the recovered log holds something else at an acknowledged frame's LSN.
    AckedFrameChanged {
        /// The frame's LSN.
        lsn: u64,
    },
    /// I1: a frame that an earlier recovery returned is gone or different.
    RecoveredFrameChanged {
        /// The frame's LSN.
        lsn: u64,
    },
    /// I1: the log holds a frame that was not submitted in the incarnation that owns its LSN, or holds
    /// one submission twice.
    PhantomFrame {
        /// The frame's LSN.
        lsn: u64,
    },
    /// I1: after a recovery the last segment holds a valid frame beyond the end of the log, which a later
    /// append can splice into the log.
    StaleFrameBeyondEnd {
        /// The segment.
        segment_no: u64,
        /// Offset of the stale frame.
        offset: u64,
    },
    /// I1: the recovered watermark is below an acknowledged checkpoint or above every submitted one.
    WatermarkWrong {
        /// The recovered watermark.
        recovered: u64,
        /// The highest acknowledged checkpoint.
        acked: u64,
    },
    /// I1: garbage collection removed a segment that holds a frame above every submitted checkpoint.
    RemovedAboveWatermark {
        /// The segment.
        segment_no: u64,
    },
    /// Recovery failed on a healed disk.
    RecoveryFailed(RecoveryError),
    /// The recovered log could not be read back, or is not contiguous.
    LogUnreadable(String),
    /// The recovery report disagrees with the log it recovered.
    ReportWrong(String),
    /// I12: an interrupted recovery ended in another state than the uninterrupted reference.
    RecoveryDiverged,
    /// I12: a second recovery, with no append in between, repaired something or moved the end of the log.
    RecoveryNotIdempotent,
    /// I12: recovery scanned more bytes than the retained segments hold.
    RecoveryUnbounded,
    /// I13: at quiescence a segment wholly at or below the watermark was still there.
    SpaceNotReclaimed {
        /// The segment.
        segment_no: u64,
    },
    /// The run did not finish within its virtual-time deadline.
    Deadline,
}

/// Counters of one run, for reporting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Incarnations of the WAL host that reached the workload.
    pub incarnations: u64,
    /// Crashes applied, including crashes during recovery.
    pub crashes: u64,
    /// Recovery attempts, the reference recoveries not counted.
    pub recovery_attempts: u64,
    /// Recovery attempts that failed with an error or were cut by a crash.
    pub recovery_retries: u64,
    /// Frames submitted.
    pub submitted: u64,
    /// Frames acknowledged.
    pub acked: u64,
    /// Appends that failed.
    pub failed_appends: u64,
    /// Segments removed by garbage collection.
    pub segments_removed: u64,
    /// Recoveries that zeroed a tail.
    pub tail_repairs: u64,
    /// Virtual microseconds the run took.
    pub virtual_micros: u64,
}

/// The result of one run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// The run seed.
    pub seed: u64,
    /// Hash of the run's event trace.
    pub trace_hash: String,
    /// Every violation the oracle found; empty when the invariants held.
    pub violations: Vec<Violation>,
    /// Counters.
    pub stats: Stats,
}

/// What the seed decides about one run (swarm testing: each feature and fault has its own coin).
#[derive(Clone, Debug)]
struct Plan {
    config: WalConfig,
    incarnations: u64,
    clients: u64,
    compressible: bool,
    /// Every workload frame has this encoded length.
    uniform_len: Option<usize>,
    bursts: bool,
    /// Some incarnations crash within a few operations of their recovery.
    crash_loops: bool,
    pin_recent: bool,
    crash_in_recovery: bool,
    double_recovery: bool,
    wall_offset_micros: i64,
    wall_steps: bool,
    faults: DiskFaults,
}

fn pick(rng: &dyn Rng, n: u64) -> u64 {
    rng.next_u64() % n.max(1)
}

fn coin(rng: &dyn Rng) -> bool {
    rng.next_u64().is_multiple_of(2)
}

impl Plan {
    fn draw(rng: &dyn Rng, options: &Options) -> Self {
        let segment_bytes = [8 << 10, 16 << 10, 64 << 10][pick(rng, 3) as usize];
        let max_segments = [6, 12, 48][pick(rng, 3) as usize];
        let retention = [
            Duration::ZERO,
            Duration::from_millis(40),
            Duration::from_secs(900),
        ][pick(rng, 3) as usize];
        let fault = |on: bool, ppm: u32| if on { ppm } else { 0 };
        let mut faults = DiskFaults {
            write_error_ppm: fault(coin(rng), 4_000),
            short_write_ppm: fault(coin(rng), 4_000),
            read_error_ppm: fault(coin(rng), 8_000),
            bit_flip_ppm: fault(coin(rng), 8_000),
            sync_error_ppm: fault(coin(rng), 8_000),
            ..DiskFaults::default()
        };
        if coin(rng) {
            faults.sync_latency_micros = (50, 4_000);
        }
        if coin(rng) {
            faults.capacity_bytes = Some(segment_bytes * (3 + pick(rng, max_segments)));
        }
        // Drawn in every tier, so that both tiers run the same workload for one seed.
        let lie = coin(rng);
        if options.tier == Tier::Stress {
            faults.fsync_lie_ppm = fault(lie, 60_000);
        }
        let crash_loops = coin(rng);
        Self {
            config: WalConfig {
                segment_bytes,
                max_bytes: segment_bytes * max_segments,
                min_free_bytes: if coin(rng) { segment_bytes } else { 0 },
                replay_retention: retention,
                mutant: options.mutant,
                ..WalConfig::default()
            },
            incarnations: if crash_loops {
                10 + pick(rng, 20)
            } else {
                2 + pick(rng, 5)
            },
            clients: 1 + pick(rng, 4),
            compressible: coin(rng),
            uniform_len: coin(rng).then(|| 256 << pick(rng, 4)),
            bursts: coin(rng),
            crash_loops,
            pin_recent: coin(rng),
            crash_in_recovery: coin(rng),
            double_recovery: coin(rng),
            wall_offset_micros: pick(rng, 600_000_000) as i64 - 300_000_000,
            wall_steps: coin(rng),
            faults,
        }
    }
}

/// What the log holds at one LSN, as far as the oracle tells frames apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    /// A workload frame: its submission id.
    Data(u64),
    Seal(BatchSeal),
    Checkpoint(Checkpoint),
}

#[derive(Clone, Copy, Debug)]
struct Submission {
    boot: u64,
    len: usize,
    compressible: bool,
}

/// The oracle ledger: everything submitted and acknowledged, and the log as the last recovery returned it.
#[derive(Debug, Default)]
struct Oracle {
    boot: u64,
    next_id: u64,
    submissions: BTreeMap<u64, Submission>,
    /// Control records submitted, with the incarnation that submitted them.
    controls: Vec<(u64, Entry)>,
    acked: BTreeMap<u64, Entry>,
    acked_watermark: u64,
    submitted_watermark: u64,
    stable: BTreeMap<u64, Entry>,
    stable_last: u64,
    violations: Vec<Violation>,
    stats: Stats,
}

/// Bytes of a workload payload that name its submission.
const HEAD: usize = 16;

/// The payload of submission `id`: a head that names it, then a fill that is a function of the id.
fn data_payload(id: u64, submission: &Submission) -> Vec<u8> {
    let mut out = Vec::with_capacity(submission.len);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&submission.boot.to_le_bytes());
    let mut x = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < submission.len {
        if submission.compressible {
            out.push(id as u8);
        } else {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.push(x as u8);
        }
    }
    out
}

impl Oracle {
    fn submit_data(&mut self, len: usize, compressible: bool) -> (u64, Vec<u8>) {
        self.next_id += 1;
        let id = self.next_id;
        let submission = Submission {
            boot: self.boot,
            len: len.max(HEAD),
            compressible,
        };
        self.submissions.insert(id, submission);
        self.stats.submitted += 1;
        (id, data_payload(id, &submission))
    }

    fn submit_control(&mut self, entry: Entry) {
        if let Entry::Checkpoint(checkpoint) = entry {
            self.submitted_watermark = self.submitted_watermark.max(checkpoint.last_lsn.get());
        }
        self.controls.push((self.boot, entry));
        self.stats.submitted += 1;
    }

    fn ack(&mut self, lsn: Lsn, entry: Entry) {
        if let Entry::Checkpoint(checkpoint) = entry {
            self.acked_watermark = self.acked_watermark.max(checkpoint.last_lsn.get());
        }
        self.acked.insert(lsn.get(), entry);
        self.stats.acked += 1;
    }

    /// What a recovered frame is, or `None` when it is nothing the workload ever submitted.
    fn classify(&self, frame: &WalFrame) -> Option<Entry> {
        let payload = frame.payload().ok()?;
        match frame.record_type() {
            RecordType::Noop => {
                let id = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
                let submission = self.submissions.get(&id)?;
                (*payload == *data_payload(id, submission)).then_some(Entry::Data(id))
            }
            record_type => match Record::decode(record_type, &payload).ok()? {
                Record::BatchSeal(seal) => Some(Entry::Seal(seal)),
                Record::Checkpoint(checkpoint) => Some(Entry::Checkpoint(checkpoint)),
                _ => None,
            },
        }
    }

    /// Checks a garbage collection against I1: nothing above the watermark may be removed.
    fn check_gc(&mut self, before: &[SegmentMeta], report: &GcReport) {
        self.stats.segments_removed += report.removed.len() as u64;
        for removed in &report.removed {
            let last_lsn = before
                .iter()
                .find(|s| s.segment_no > removed.segment_no)
                .map_or(u64::MAX, |next| next.first_lsn.get().saturating_sub(1));
            if last_lsn > self.submitted_watermark {
                self.violations.push(Violation::RemovedAboveWatermark {
                    segment_no: removed.segment_no,
                });
            }
        }
    }

    /// Checks a recovered log against the ledger, then makes it the new stable log.
    fn check_recovery(&mut self, report: &RecoveryReport, log: &BTreeMap<u64, WalFrame>) {
        let mut found = Vec::new();
        let watermark = report.watermark_lsn().get();
        if watermark < self.acked_watermark || watermark > self.submitted_watermark {
            found.push(Violation::WatermarkWrong {
                recovered: watermark,
                acked: self.acked_watermark,
            });
        }
        let first = report.first_retained_lsn().get();
        let last = report.last_lsn.get();
        if last < self.stable_last {
            found.push(Violation::RecoveredFrameChanged {
                lsn: self.stable_last,
            });
        }

        let mut entries = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut seals = Vec::new();
        for (&lsn, frame) in log {
            let entry = self.classify(frame);
            let fresh = lsn > self.stable_last;
            let owned = match entry {
                // A frame above the last recovered LSN was written by the incarnation that just ended.
                Some(Entry::Data(id)) => {
                    let boot = self.submissions.get(&id).map(|s| s.boot);
                    seen.insert(id) && (!fresh || boot == Some(self.boot))
                }
                Some(control) => !fresh || self.controls.contains(&(self.boot, control)),
                None => false,
            };
            if !owned {
                found.push(Violation::PhantomFrame { lsn });
            }
            if let Some(Entry::Seal(seal)) = entry
                && seal.last_lsn.get() > watermark
            {
                seals.push(seal);
            }
            if let Some(entry) = entry {
                entries.insert(lsn, entry);
            }
        }
        for (&lsn, entry) in self.stable.range(first..) {
            if entries.get(&lsn) != Some(entry) {
                found.push(Violation::RecoveredFrameChanged { lsn });
            }
        }
        for (&lsn, entry) in &self.acked {
            match entries.get(&lsn) {
                Some(recovered) if recovered == entry => {}
                Some(_) => found.push(Violation::AckedFrameChanged { lsn }),
                None if lsn > watermark => found.push(Violation::AckedFrameLost { lsn }),
                None => {}
            }
        }
        if seals != report.unchecked_seals {
            found.push(Violation::ReportWrong(format!(
                "unchecked seals {:?}, the log holds {seals:?}",
                report.unchecked_seals
            )));
        }
        let expected = (last + 1).saturating_sub(first);
        let ends_match = log.keys().next().is_none_or(|&lsn| lsn == first)
            && log.keys().next_back().is_none_or(|&lsn| lsn == last);
        if log.len() as u64 != expected || !ends_match {
            found.push(Violation::LogUnreadable(format!(
                "{} frames for lsn {first} to {last}",
                log.len()
            )));
        }
        if report.tail_repair.is_some() {
            self.stats.tail_repairs += 1;
        }
        self.violations.append(&mut found);
        self.stable = entries;
        self.stable_last = last;
    }
}

/// The logical state a recovery ends in. Two recoveries of one crashed disk must agree on it. The epoch
/// and segment creation times are left out: a recovery that initialises an empty directory mints them.
#[derive(Clone, PartialEq, Eq, Debug)]
struct LogState {
    last_lsn: Lsn,
    watermark: Option<Checkpoint>,
    unchecked_seals: Vec<BatchSeal>,
    segments: Vec<(u64, Lsn)>,
    frames: Vec<(u64, u8, i64, u32)>,
}

impl LogState {
    fn new(report: &RecoveryReport, log: &BTreeMap<u64, WalFrame>) -> Self {
        Self {
            last_lsn: report.last_lsn,
            watermark: report.watermark.map(|w| w.checkpoint),
            unchecked_seals: report.unchecked_seals.clone(),
            segments: report
                .segments
                .iter()
                .map(|s| (s.segment_no, s.first_lsn))
                .collect(),
            frames: log
                .values()
                .map(|f| {
                    let header = f.header();
                    (
                        header.lsn.get(),
                        header.record_type.code(),
                        header.received_at.micros(),
                        header.payload_crc32c,
                    )
                })
                .collect(),
        }
    }
}

/// Reads the whole retained log through the replay API.
async fn read_log(wal: &Wal, from: Lsn) -> Result<BTreeMap<u64, WalFrame>, String> {
    let mut replay = wal.replay(from);
    let mut log = BTreeMap::new();
    loop {
        match replay.next().await {
            Ok(Some(frame)) => {
                log.insert(frame.lsn().get(), frame);
            }
            Ok(None) => return Ok(log),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn prepare(record_type: RecordType, payload: Vec<u8>) -> Option<PreparedFrame> {
    PreparedFrame::new(record_type, payload, Compression::Lz4).ok()
}

#[derive(Clone)]
struct Shared {
    oracle: Arc<Mutex<Oracle>>,
    plan: Arc<Plan>,
}

impl Shared {
    fn oracle(&self) -> MutexGuard<'_, Oracle> {
        self.oracle.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One ingest client: submits frames, sometimes several at once, and records each acknowledgement the
/// moment it sees it.
async fn client(shared: Shared, env: Env, wal: Wal, rng: Arc<dyn Rng>) {
    let plan = &shared.plan;
    let capacity = (plan.config.segment_bytes as usize - SEGMENT_HEADER_LEN) / 2;
    loop {
        env.clock
            .sleep(Duration::from_micros(pick(rng.as_ref(), 3_000)))
            .await;
        let admission = wal.admission();
        if admission.failed {
            return;
        }
        if !admission.accepting() {
            // Ingest would answer 503 with Retry-After.
            env.clock.sleep(Duration::from_millis(5)).await;
            continue;
        }
        let burst = if plan.bursts {
            1 + pick(rng.as_ref(), 6)
        } else {
            1
        };
        let mut waiting = Vec::new();
        for _ in 0..burst {
            // A few sizes whose encoded length is a power of two, so that a stale frame left behind a torn
            // tail can line up with new ones.
            let encoded = plan
                .uniform_len
                .unwrap_or_else(|| 64 << pick(rng.as_ref(), 7));
            let len = encoded.min(capacity) - FRAME_HEADER_LEN;
            let compressible =
                plan.uniform_len.is_none() && plan.compressible && pick(rng.as_ref(), 4) == 0;
            let (id, payload) = shared.oracle().submit_data(len, compressible);
            if let Some(frame) = prepare(RecordType::Noop, payload) {
                waiting.push((id, wal.append(frame)));
            }
        }
        for (id, done) in waiting {
            match done.await {
                Ok(lsn) => shared.oracle().ack(lsn, Entry::Data(id)),
                Err(AppendError::Failed | AppendError::Closed) => return,
                Err(_) => shared.oracle().stats.failed_appends += 1,
            }
        }
    }
}

/// Appends a control record and records its acknowledgement. `None` when the WAL is gone.
async fn append_control(shared: &Shared, wal: &Wal, entry: Entry) -> Option<bool> {
    let record = match entry {
        Entry::Seal(seal) => Record::BatchSeal(seal),
        Entry::Checkpoint(checkpoint) => Record::Checkpoint(checkpoint),
        Entry::Data(_) => return Some(false),
    };
    let frame = prepare(record.record_type(), record.encode().ok()?)?;
    shared.oracle().submit_control(entry);
    match wal.append(frame).await {
        Ok(lsn) => {
            shared.oracle().ack(lsn, entry);
            Some(true)
        }
        Err(AppendError::Failed | AppendError::Closed) => None,
        Err(_) => {
            shared.oracle().stats.failed_appends += 1;
            Some(false)
        }
    }
}

/// Seals everything durable above `sealed_through` and checkpoints it. Returns the new `sealed_through`,
/// or `None` when the WAL is gone.
async fn seal_and_checkpoint(shared: &Shared, wal: &Wal, sealed_through: u64) -> Option<u64> {
    let through = wal.durable_lsn().get();
    if through <= sealed_through {
        return Some(sealed_through);
    }
    let seal = BatchSeal {
        first_lsn: Lsn::new(sealed_through + 1),
        last_lsn: Lsn::new(through),
        frame_count: u32::try_from(through - sealed_through).unwrap_or(u32::MAX),
        normalizer_version: 1,
        price_table_version: 1,
        deletion_seq: 0,
    };
    if !append_control(shared, wal, Entry::Seal(seal)).await? {
        return Some(sealed_through);
    }
    let checkpoint = Checkpoint {
        batch_seq: seal.batch_seq(),
        last_lsn: seal.last_lsn,
        table_bitmap: Checkpoint::ALL_TABLES,
    };
    append_control(shared, wal, Entry::Checkpoint(checkpoint)).await?;
    Some(through)
}

async fn collect_garbage(shared: &Shared, wal: &Wal, recent_floor: Option<Lsn>) {
    let before = wal.segments();
    if let Ok(report) = wal.gc(recent_floor).await {
        shared.oracle().check_gc(&before, &report);
    }
}

/// Stands in for the shipper: seals the durable tail, checkpoints it, and collects garbage.
async fn shipper(shared: Shared, env: Env, wal: Wal, rng: Arc<dyn Rng>, mut sealed_through: u64) {
    loop {
        env.clock
            .sleep(Duration::from_millis(2 + pick(rng.as_ref(), 40)))
            .await;
        let Some(through) = seal_and_checkpoint(&shared, &wal, sealed_through).await else {
            return;
        };
        sealed_through = through;
        let recent_floor = (shared.plan.pin_recent && coin(rng.as_ref())).then(|| {
            let durable = wal.durable_lsn().get();
            Lsn::new(durable.saturating_sub(pick(rng.as_ref(), 64)))
        });
        collect_garbage(&shared, &wal, recent_floor).await;
    }
}

struct Run<'a> {
    sim: &'a Sim,
    disk: SimDisk,
    shared: Shared,
    rng: Arc<dyn Rng>,
    boots: u64,
}

impl Run<'_> {
    fn plan(&self) -> &Plan {
        &self.shared.plan
    }

    fn event(&self, label: &str, fields: &[u64]) {
        let micros = self.sim.elapsed().as_micros() as u64;
        self.sim.trace().event("at", &[micros]);
        self.sim.trace().event(label, fields);
    }

    fn violate(&self, violation: Violation) {
        self.shared.oracle().violations.push(violation);
    }

    fn failed(&self) -> bool {
        !self.shared.oracle().violations.is_empty()
    }

    /// Looks for a valid frame after the end of the recovered log in the last segment.
    fn stale_frame_beyond_end(
        &self,
        report: &RecoveryReport,
        log: &BTreeMap<u64, WalFrame>,
    ) -> Option<Violation> {
        let segment_no = report.segments.last()?.segment_no;
        let path = DiskPath::new(Root::Wal, &format!("seg-{segment_no:020}.wal")).ok()?;
        let bytes = self.disk.read_file(&path)?;
        let end = log
            .values()
            .filter(|frame| frame.segment_no() == segment_no)
            .map(WalFrame::end_offset)
            .max()
            .unwrap_or(SEGMENT_HEADER_LEN as u64) as usize;
        (end..bytes.len())
            .filter(|&at| bytes[at..].starts_with(&FRAME_MAGIC))
            .find(|&at| matches!(scan_frame(&bytes, at), Scan::Frame(_)))
            .map(|at| Violation::StaleFrameBeyondEnd {
                segment_no,
                offset: at as u64,
            })
    }

    /// What one uninterrupted recovery of the disk as it is now yields, computed on a fault-free copy.
    async fn reference(&self) -> Result<(LogState, bool), RecoveryError> {
        let label = format!("wal-sim/reference-{}", self.boots);
        let copy = self.disk.fork(self.sim.rng(&label).as_ref());
        let host = self.sim.host(&label, &copy, self.plan().wall_offset_micros);
        let opened = Box::pin(Wal::open(&host.env, self.plan().config.clone())).await;
        let state = match opened {
            Ok((wal, report)) => {
                let log = read_log(&wal, report.first_retained_lsn())
                    .await
                    .unwrap_or_default();
                Ok((LogState::new(&report, &log), report.tail_repair.is_some()))
            }
            Err(e) => Err(e),
        };
        host.stop().await;
        state
    }

    /// Recovers the WAL on a new host, through crashes and faults, then checks the result. Also returns
    /// whether the uninterrupted reference recovery had a tail to repair. `None` when a violation ended
    /// the run.
    async fn boot(&mut self) -> Option<(Host, Wal, RecoveryReport, bool)> {
        self.boots += 1;
        let workload_faults = self.disk.faults();
        let (reference, repaired) = match self.reference().await {
            Ok(reference) => reference,
            Err(e) => {
                self.violate(Violation::RecoveryFailed(e));
                return None;
            }
        };
        let mut attempt = 0u64;
        let (host, wal, report) = loop {
            attempt += 1;
            // After three failures the faults are healed: a recovery must then succeed.
            let healed = attempt > 3;
            if healed {
                self.disk.set_faults(DiskFaults::default());
            }
            let name = format!("wal-sim/boot-{}/attempt-{attempt}", self.boots);
            let host = self
                .sim
                .host(&name, &self.disk, self.plan().wall_offset_micros);
            if self.plan().crash_in_recovery && !healed && coin(self.rng.as_ref()) {
                self.disk
                    .arm_crash_after_ops(1 + pick(self.rng.as_ref(), 60));
            }
            self.shared.oracle().stats.recovery_attempts += 1;
            let opened = {
                let mut open = Box::pin(Wal::open(&host.env, self.plan().config.clone())).fuse();
                let mut crash_point = Box::pin(self.disk.crash_point_reached()).fuse();
                futures::select_biased! {
                    opened = open => Some(opened),
                    () = crash_point => None,
                }
            };
            self.disk.disarm_crash();
            match opened {
                Some(Ok((wal, report))) => break (host, wal, report),
                Some(Err(e)) if healed => {
                    self.violate(Violation::RecoveryFailed(e));
                    host.stop().await;
                    return None;
                }
                failed => {
                    self.event("recovery.retry", &[attempt, u64::from(failed.is_some())]);
                    {
                        let mut oracle = self.shared.oracle();
                        oracle.stats.recovery_retries += 1;
                        oracle.stats.crashes += 1;
                    }
                    drop(failed);
                    host.crash().await;
                }
            }
        };

        // The checks read the log back, so they run on a healed disk.
        self.disk.set_faults(DiskFaults::default());
        self.event(
            "recovery.done",
            &[report.last_lsn.get(), report.watermark_lsn().get()],
        );
        match read_log(&wal, report.first_retained_lsn()).await {
            Ok(log) => {
                if LogState::new(&report, &log) != reference {
                    self.violate(Violation::RecoveryDiverged);
                }
                let retained = report.segments.len() as u64 * self.plan().config.segment_bytes;
                if report.scanned_bytes > retained {
                    self.violate(Violation::RecoveryUnbounded);
                }
                if let Some(violation) = self.stale_frame_beyond_end(&report, &log) {
                    self.violate(violation);
                }
                self.shared.oracle().check_recovery(&report, &log);
            }
            Err(e) => self.violate(Violation::LogUnreadable(e)),
        }
        self.disk.set_faults(workload_faults);
        if self.failed() {
            drop(wal);
            host.stop().await;
            return None;
        }
        Some((host, wal, report, repaired))
    }

    /// Crashes the host and counts it.
    async fn crash(&self, host: Host, wal: Wal) {
        drop(wal);
        self.event("crash", &[self.disk.ops()]);
        self.shared.oracle().stats.crashes += 1;
        host.crash().await;
    }

    /// One incarnation: recover, run the workload until a seeded crash point, crash.
    async fn incarnation(&mut self) -> Option<()> {
        let (mut host, mut wal, mut report, _) = self.boot().await?;
        if self.plan().double_recovery && coin(self.rng.as_ref()) {
            // I12: with no append in between, a second recovery has nothing to repair.
            self.crash(host, wal).await;
            let last_lsn = report.last_lsn;
            let repaired;
            (host, wal, report, repaired) = self.boot().await?;
            if repaired || report.last_lsn != last_lsn {
                self.violate(Violation::RecoveryNotIdempotent);
            }
        }
        {
            let mut oracle = self.shared.oracle();
            oracle.boot = self.boots;
            oracle.stats.incarnations += 1;
        }
        if self.plan().wall_steps {
            host.clock
                .step_wall(pick(self.rng.as_ref(), 400_000) as i64 - 200_000);
        }
        for index in 0..self.plan().clients {
            let label = format!("wal-sim/client-{}-{index}", self.boots);
            let task = client(
                self.shared.clone(),
                host.env.clone(),
                wal.clone(),
                self.sim.rng(&label),
            );
            drop(host.env.spawn("client", task));
        }
        let label = format!("wal-sim/shipper-{}", self.boots);
        let task = shipper(
            self.shared.clone(),
            host.env.clone(),
            wal.clone(),
            self.sim.rng(&label),
            report.sealed_through().get(),
        );
        drop(host.env.spawn("shipper", task));

        // The crash point: a seeded number of disk operations from now, or a seeded virtual time,
        // whichever comes first.
        let short = self.plan().crash_loops && pick(self.rng.as_ref(), 4) > 0;
        let (ops, micros) = if short { (12, 4_000) } else { (400, 250_000) };
        if coin(self.rng.as_ref()) {
            self.disk
                .arm_crash_after_ops(1 + pick(self.rng.as_ref(), ops));
        }
        let run_for = Duration::from_micros(500 + pick(self.rng.as_ref(), micros));
        {
            let mut crash_point = Box::pin(self.disk.crash_point_reached()).fuse();
            let mut timer = host.env.clock.sleep(run_for).fuse();
            futures::select_biased! {
                () = crash_point => {}
                () = timer => {}
            }
        }
        self.crash(host, wal).await;
        Some(())
    }

    /// Quiescence: heal every fault, drain, collect garbage, restart once more and compare.
    async fn quiesce(&mut self) -> Option<()> {
        self.disk.set_faults(DiskFaults::default());
        let (host, wal, report, _) = self.boot().await?;
        self.shared.oracle().boot = self.boots;
        let sealed = seal_and_checkpoint(&self.shared, &wal, report.sealed_through().get()).await;
        // Past replay retention, however the wall clock was stepped.
        let retention = self.plan().config.replay_retention;
        host.env
            .clock
            .sleep(retention + Duration::from_secs(10))
            .await;
        collect_garbage(&self.shared, &wal, None).await;
        let watermark = wal.watermark().map_or(0, |w| w.last_lsn().get());
        let segments = wal.segments();
        if sealed.is_some()
            && let [oldest, next, ..] = segments.as_slice()
            && next.first_lsn.get().saturating_sub(1) <= watermark
        {
            // I13: the oldest segment lies wholly at or below the watermark and is still there.
            self.violate(Violation::SpaceNotReclaimed {
                segment_no: oldest.segment_no,
            });
        }
        self.crash(host, wal).await;
        let (host, wal, _, _) = self.boot().await?;
        drop(wal);
        host.stop().await;
        Some(())
    }
}

/// The virtual time one run may take.
const DEADLINE: Duration = Duration::from_secs(6 * 60 * 60);

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let plan_rng = sim.rng("wal-sim/plan");
    let plan = Arc::new(Plan::draw(plan_rng.as_ref(), &options));
    let disk = sim.disk("wal-sim/disk");
    disk.set_faults(plan.faults.clone());
    let shared = Shared {
        oracle: Arc::default(),
        plan: Arc::clone(&plan),
    };
    let mut run = Run {
        sim: &sim,
        disk,
        shared: shared.clone(),
        rng: sim.rng("wal-sim/driver"),
        boots: 0,
    };
    let body = async {
        for _ in 0..plan.incarnations {
            run.incarnation().await?;
        }
        run.quiesce().await
    };
    let finished = tokio::time::timeout(DEADLINE, Box::pin(body)).await;
    let mut oracle = shared.oracle();
    if finished.is_err() {
        oracle.violations.push(Violation::Deadline);
    }
    oracle.stats.virtual_micros = sim.elapsed().as_micros() as u64;
    Outcome {
        seed: sim.seed(),
        trace_hash: sim.trace().hash(),
        violations: oracle.violations.clone(),
        stats: oracle.stats,
    }
}

/// Runs the WAL simulation for one seed. The whole run, its faults and its crash points are a function of
/// `seed` and `options`.
pub fn run_wal_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
