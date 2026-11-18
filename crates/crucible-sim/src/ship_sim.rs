//! Deterministic simulation of the insert protocol: the WAL suite's disk, crashes and client load, with OTLP
//! requests as frames, the real `crucible-shipper` and the store model of [`crate::ch_model`].
//!
//! Incarnations end with a crash at a seeded disk operation, at a seeded crash barrier of the shipper
//! (contract 5.9), at a seeded virtual time, or with a process stop. Inserts in flight outlive the crash and may
//! land later. The run ends with a quiescence phase: faults healed, the log drained, one more restart.
//!
//! - **I1** at every restart, every accepted span of every acknowledged request at or below the recovered
//!   watermark is in `span_core` and in `span_payload`, or its pair is quarantined; at quiescence this holds
//!   for every acknowledged request.
//! - **I2** no (table, LSN, span) row lands twice; no two blocks of one (batch, table, partition) land and
//!   every aggregating block has exactly one marker (the model checks both on every insert); no two batch
//!   identities over the same frames were ever on the wire; a pair with an attempt in doubt is quarantined only
//!   after a probe that read absent; at quiescence every root span has its head row and the `trace_stats` span
//!   counts sum to the `span_core` rows.
//! - **I12** a restart after quiescence sends nothing and leaves the store and the watermark as they were, and
//!   the recovered log equals the uninterrupted reference recovery although crashes cut the real one.
//! - **I13** at quiescence no segment wholly at or below the watermark is left.
//! - **I14** the run is a function of the seed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crucible_chfmt::Compression;
use crucible_env::{DiskPath, Env, Rng, Root, StoreFlavour, Stores};
use crucible_normalize::{Admission, NormalizedFrame};
use crucible_records::{ApiKeyId, Lsn, PartitionId, ProjectId, TableId, Timestamp};
use crucible_schema::{Granularity, route};
use crucible_shipper::{
    Barrier, Barriers, FRAMES_FILE, Hooks, MANIFEST_FILE, Mutant, RecentIndex, ShipError, Shipper,
    ShipperConfig, ShipperStats, StatsSnapshot,
};
use crucible_wal::{AppendError, RecoveryReport, Wal, WalFrame};
use crucible_wal_codec::{
    ContentOrigin, FRAME_HEADER_LEN, FrameHeader, IngestRequest, IngestTransport, PreparedFrame,
    Record, RecordType, SEGMENT_HEADER_LEN, load_payload,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;
use tokio::sync::Notify;

use crate::boot::{BootCounts, Booted, Recovery, recover};
use crate::ch_model::{
    ChFaults, ChModel, ChStats, LastAnswer, bytes_at, i8_at, u8_at, u64_at, uuid_at,
};
use crate::disk::{DiskFaults, SimDisk};
use crate::runtime::{Host, SIM_EPOCH_MICROS, Sim, run};
use crate::trace::Trace;
use crate::wal_sim::{self, LogState, coin, pick, read_log};

/// What one run does besides what its seed decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Options {
    /// A seeded defect of `crucible-shipper` that the run must catch.
    pub mutant: Option<Mutant>,
}

/// An invariant violation found by the oracle.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Violation {
    /// Recovery of the WAL failed on a healed disk.
    RecoveryFailed(String),
    /// I12: an interrupted WAL recovery ended in another state than the uninterrupted reference.
    RecoveryDiverged,
    /// The recovered log could not be read back.
    LogUnreadable(String),
    /// The shipper stopped with an error that no fault explains.
    ShipperFailed(String),
    /// The store refused a malformed block, or two blocks of one pair landed (I2).
    Store(String),
    /// I2: two different batch identities over the same frames were sent.
    IdentityOverlap {
        /// One LSN range.
        first: (u64, u64),
        /// An overlapping, different LSN range.
        second: (u64, u64),
    },
    /// I2: a row of one span of one frame landed more than once in a table.
    DuplicateRow {
        /// The table.
        table: &'static str,
        /// The frame's LSN.
        lsn: u64,
    },
    /// A row that no submitted request explains.
    UnknownRow {
        /// The table.
        table: &'static str,
        /// The LSN the row carries.
        lsn: u64,
    },
    /// I1: an accepted span of an acknowledged request at or below the watermark is neither in the table nor
    /// in a quarantine.
    AckedSpanLost {
        /// The table.
        table: &'static str,
        /// The frame's LSN.
        lsn: u64,
    },
    /// I2: a root span has no head row, or `trace_stats` counts another number of spans than `span_core` holds.
    DerivedMismatch(String),
    /// Contract 5.6: a pair of which the store had accepted a block was quarantined without a probe that read
    /// absent.
    QuarantinedInDoubt {
        /// The table.
        table: &'static str,
        /// The batch sequence number.
        batch_seq: u64,
    },
    /// A quarantine copy that does not hold the frames of its batch.
    QuarantineCopy(String),
    /// Stage B fed the recent index out of LSN order.
    RecentOutOfOrder,
    /// At quiescence the watermark did not reach the last frame that warrants a seal.
    NotDrained {
        /// The watermark.
        watermark: u64,
        /// The last such frame.
        target: u64,
        /// What the draining shipper did.
        shipper: StatsSnapshot,
    },
    /// I13: at quiescence a segment wholly at or below the watermark was still there.
    SpaceNotReclaimed {
        /// The segment.
        segment_no: u64,
    },
    /// I12: a restart after quiescence sent inserts or changed the store or the watermark.
    RestartNotIdempotent,
    /// The run did not finish within its virtual-time deadline.
    Deadline,
}

/// Counters of one run, for reporting and for the coverage assertions of the suite.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Incarnations that reached the workload.
    pub incarnations: u64,
    /// Crashes, including crashes during WAL recovery.
    pub crashes: u64,
    /// Crashes at a crash barrier of the shipper.
    pub barrier_crashes: u64,
    /// Those crashes by barrier, in the order of [`BARRIERS`].
    pub crashes_at: [u64; 7],
    /// Incarnations that ended with a process stop.
    pub stops: u64,
    /// Requests submitted.
    pub submitted: u64,
    /// Requests acknowledged.
    pub acked: u64,
    /// Requests that repeat an earlier one.
    pub resent: u64,
    /// Store outages.
    pub outages: u64,
    /// Quarantine directories at the end.
    pub quarantines: u64,
    /// The shipper's counters, summed over its incarnations.
    pub shipper: StatsSnapshot,
    /// The store's counters.
    pub store: ChStats,
    /// Virtual microseconds the run took.
    pub virtual_micros: u64,
}

/// The crash barriers, by the names of contract 5.9, in the order of [`Stats::crashes_at`].
pub const BARRIERS: [&str; 7] = [
    "before seal.appended",
    "seal.appended",
    "before insert.sent",
    "insert.sent",
    "quarantine.copied",
    "before checkpoint.appended",
    "checkpoint.appended",
];

/// The index of `quarantine.copied` in [`BARRIERS`].
pub(crate) const QUARANTINE_COPIED: u64 = 4;

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

/// What the seed decides about one run besides the WAL plan (swarm testing: one coin per feature and fault).
#[derive(Clone, Debug)]
struct ShipPlan {
    wal: wal_sim::Plan,
    config: ShipperConfig,
    faults: ChFaults,
    outages: bool,
    long_outages: bool,
    barrier_crashes: bool,
    projects: u64,
    resend: bool,
    reject_some: bool,
    multi_day: bool,
    rootless: bool,
}

impl ShipPlan {
    fn draw(rng: &dyn Rng, options: &Options) -> Self {
        let wal = wal_sim::Plan::draw(rng, &wal_sim::Options::REALISTIC);
        let choose = |values: &[u64]| values[pick(rng, values.len() as u64) as usize];
        let millis = |values: &[u64]| Duration::from_millis(choose(values));
        let seal_rows = choose(&[3, 40, 131_072]) as usize;
        let small = |on: bool| if on { 8 } else { 4_000_000 };
        let config = ShipperConfig {
            linger: millis(&[2, 20, 500]),
            max_linger: Duration::from_secs(2),
            seal_rows,
            max_seal_rows: (seal_rows * 2).min(262_144),
            seal_bytes: choose(&[600, 128 << 20]),
            pressure_latency: millis(&[50, 2_000]),
            max_batches_in_flight: choose(&[1, 2]) as usize,
            connections: choose(&[1, 2, 8]) as usize,
            compression: if coin(rng) {
                Compression::Lz4
            } else {
                Compression::Zstd
            },
            flavour: if coin(rng) {
                StoreFlavour::SingleNode
            } else {
                StoreFlavour::Clustered
            },
            granularity: if coin(rng) {
                Granularity::Day
            } else {
                Granularity::Week
            },
            probe_valid_for: millis(&[2_000, 60_000]),
            insert_timeout: millis(&[300, 90_000]),
            backoff_min: millis(&[1, 100]),
            backoff_max: millis(&[200, 30_000]),
            tail_poll: millis(&[2, 20]),
            gc_interval: millis(&[20, 1_000]),
            duplicate_window_keys: small(coin(rng)),
            first_seen_traces: small(coin(rng)),
            mutant: options.mutant,
            ..ShipperConfig::default()
        };
        let fault = |on: bool, rate: u32| if on { rate } else { 0 };
        let storm = coin(rng) && coin(rng);
        let faults = ChFaults {
            timeout_lost_ppm: fault(coin(rng), 30_000),
            timeout_late_ppm: fault(coin(rng), 80_000),
            ack_lost_ppm: fault(coin(rng), 50_000),
            server_error_ppm: fault(coin(rng), 20_000),
            too_many_parts_ppm: fault(coin(rng), if storm { 450_000 } else { 50_000 }),
            memory_ppm: fault(coin(rng), 30_000),
            too_many_queries_ppm: fault(coin(rng), 30_000),
            hang_ppm: fault(coin(rng), 150_000),
            overload_ppm: fault(coin(rng), 500_000),
            overload_micros: (1_000_000, 90_000_000),
            probe_error_ppm: fault(coin(rng), 50_000),
            poison_pair_ppm: fault(coin(rng), 60_000),
            poison_first_in_flight_ppm: 500_000,
            expire_ppm: fault(coin(rng), 700_000),
            latency_micros: if coin(rng) {
                (100, 5_000)
            } else {
                (1_000, 300_000)
            },
            late_apply_micros: if coin(rng) {
                (10_000, 2_000_000)
            } else {
                (1_000_000, 120_000_000)
            },
            read_error_ppm: 0,
        };
        Self {
            wal,
            config,
            faults,
            outages: coin(rng),
            long_outages: coin(rng),
            barrier_crashes: coin(rng),
            projects: 1 + pick(rng, 3),
            resend: coin(rng),
            reject_some: coin(rng),
            multi_day: coin(rng),
            rootless: coin(rng),
        }
    }
}

type SpanKey = ([u8; 16], [u8; 16], [u8; 8]);

#[derive(Clone, Copy, Debug)]
struct SpanInfo {
    trace: [u8; 16],
    span: [u8; 8],
    start: Timestamp,
    root: bool,
    rejected: bool,
}

/// One submitted request: what the oracle expects of it, and its frame payload for a resend.
#[derive(Clone, Debug)]
struct Request {
    project: [u8; 16],
    spans: Arc<Vec<SpanInfo>>,
    payload: Arc<Vec<u8>>,
}

/// The oracle ledger.
#[derive(Debug, Default)]
struct Ledger {
    requests: Vec<Request>,
    /// Acknowledged frames: LSN to request.
    acked: BTreeMap<u64, usize>,
    /// How many submitted requests carry each accepted span key.
    keys: BTreeMap<SpanKey, u32>,
    violations: Vec<Violation>,
    stats: Stats,
}

impl Ledger {
    fn submit(&mut self, request: Request) -> usize {
        for span in request.spans.iter().filter(|s| !s.rejected) {
            *self
                .keys
                .entry((request.project, span.trace, span.span))
                .or_default() += 1;
        }
        self.stats.submitted += 1;
        self.requests.push(request);
        self.requests.len() - 1
    }

    fn violate(&mut self, violation: Violation) {
        // A few of each kind tell the story; thousands of lost rows would bury the first cause.
        let kind = std::mem::discriminant(&violation);
        let same = self
            .violations
            .iter()
            .filter(|v| std::mem::discriminant(*v) == kind)
            .count();
        if same < 3 && !self.violations.contains(&violation) {
            self.violations.push(violation);
        }
    }
}

#[derive(Clone)]
struct Shared {
    ledger: Arc<Mutex<Ledger>>,
    plan: Arc<ShipPlan>,
}

impl Shared {
    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) fn random<const N: usize>(rng: &dyn Rng) -> [u8; N] {
    let mut bytes = [0u8; N];
    rng.fill_bytes(&mut bytes);
    // Never all-zero: ingest rejects such ids before the append.
    bytes[0] |= 1;
    bytes
}

/// One span of a request: its trace, its id, its start in microseconds, and whether it is a root.
pub type SpanSpec = ([u8; 16], [u8; 8], i64, bool);

/// The payload of the `IngestRequest` frame that ingest would append for a request with these spans. Bit
/// `i` of `rejected` marks span `i` as rejected before the ack.
pub fn request_payload(project: [u8; 16], spans: &[SpanSpec], rejected: u8) -> Option<Vec<u8>> {
    let wire = spans
        .iter()
        .enumerate()
        .map(|(i, &(trace, span, start_micros, root))| {
            let start_nanos = (start_micros as u64) * 1_000;
            Span {
                trace_id: trace.to_vec(),
                span_id: span.to_vec(),
                parent_span_id: if root { Vec::new() } else { vec![7; 8] },
                name: format!("op-{}", i % 4),
                kind: 1,
                start_time_unix_nano: start_nanos,
                end_time_unix_nano: start_nanos + 1_500_000 * (1 + i as u64),
                ..Span::default()
            }
        })
        .collect();
    let otlp = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: None,
            scope_spans: vec![ScopeSpans {
                scope: None,
                spans: wire,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
    .encode_to_vec();
    Record::IngestRequest(IngestRequest {
        project_id: ProjectId::from_bytes(project),
        api_key_id: ApiKeyId::from_bytes([9; 16]),
        compat_dialect: 0,
        transport: IngestTransport::Http,
        content_origin: ContentOrigin::Protobuf,
        admin_import: false,
        sdk_name: "sim",
        sdk_version: "1",
        idempotency_key: &[],
        span_count: spans.len() as u32,
        rejected: &[rejected],
        otlp: &otlp,
    })
    .encode()
    .ok()
}

/// Builds one request: a few spans of new or recent traces.
fn generate(plan: &ShipPlan, rng: &dyn Rng, traces: &mut Vec<[u8; 16]>) -> Option<Request> {
    let project = [1 + pick(rng, plan.projects) as u8; 16];
    let count = 1 + pick(rng, 5);
    let mut spans = Vec::new();
    let mut wire = Vec::new();
    let mut rejected_bits = 0u8;
    for ordinal in 0..count {
        let reuse = !traces.is_empty() && pick(rng, 3) == 0;
        let trace = if reuse {
            traces[pick(rng, traces.len() as u64) as usize]
        } else {
            let trace = random::<16>(rng);
            traces.push(trace);
            if traces.len() > 32 {
                traces.remove(0);
            }
            trace
        };
        let span = random::<8>(rng);
        let root = !reuse && !(plan.rootless && coin(rng));
        let day = if plan.multi_day { pick(rng, 3) } else { 0 };
        let start_micros =
            SIM_EPOCH_MICROS + (day * 86_400_000_000 + pick(rng, 3_600_000_000)) as i64;
        let rejected = plan.reject_some && pick(rng, 6) == 0;
        if rejected {
            rejected_bits |= 1 << ordinal;
        }
        spans.push(SpanInfo {
            trace,
            span,
            start: Timestamp::from_micros(start_micros),
            root,
            rejected,
        });
        wire.push((trace, span, start_micros, root));
    }
    Some(Request {
        project,
        spans: Arc::new(spans),
        payload: Arc::new(request_payload(project, &wire, rejected_bits)?),
    })
}

/// One ingest client: submits a bounded number of requests, sometimes several at once and sometimes a repeat
/// of an earlier one, and records each acknowledgement the moment it sees it.
async fn client(shared: Shared, env: Env, wal: Wal, rng: Arc<dyn Rng>, budget: u64) {
    let plan = &shared.plan;
    let capacity = (plan.wal.config.segment_bytes as usize - SEGMENT_HEADER_LEN) / 2;
    let mut traces = Vec::new();
    let mut sent = 0;
    let mut refused = 0u32;
    while sent < budget {
        env.clock
            .sleep(Duration::from_micros(pick(rng.as_ref(), 3_000)))
            .await;
        let admission = wal.admission();
        if admission.failed {
            return;
        }
        if !admission.accepting() {
            // Ingest would answer 503 with Retry-After.
            env.clock
                .sleep(Duration::from_millis(5 << refused.min(7)))
                .await;
            refused += 1;
            continue;
        }
        refused = 0;
        let burst = if plan.wal.bursts {
            1 + pick(rng.as_ref(), 6)
        } else {
            1
        };
        let mut waiting = Vec::new();
        for _ in 0..burst {
            sent += 1;
            let earlier = shared.ledger().requests.len() as u64;
            let resend = plan.resend && earlier > 0 && pick(rng.as_ref(), 8) == 0;
            let request = if resend {
                let index = pick(rng.as_ref(), earlier) as usize;
                let mut ledger = shared.ledger();
                ledger.stats.resent += 1;
                Some(ledger.requests[index].clone())
            } else {
                generate(plan, rng.as_ref(), &mut traces)
            };
            let Some(request) = request else { continue };
            let frame = PreparedFrame::new(
                RecordType::IngestRequest,
                request.payload.to_vec(),
                crucible_wal_codec::Compression::Lz4,
            );
            let Ok(frame) = frame else { continue };
            if frame.encoded_len() > capacity {
                continue;
            }
            let index = shared.ledger().submit(request);
            waiting.push((index, wal.append(frame)));
        }
        for (index, done) in waiting {
            match done.await {
                Ok(lsn) => {
                    let mut ledger = shared.ledger();
                    ledger.acked.insert(lsn.get(), index);
                    ledger.stats.acked += 1;
                }
                Err(AppendError::Failed | AppendError::Closed) => return,
                Err(_) => {}
            }
        }
    }
}

/// The crash barriers of one incarnation: the armed one freezes the disk and never completes, so the crash
/// falls exactly between the two steps the barrier separates.
#[derive(Debug)]
pub(crate) struct SimBarriers {
    disk: SimDisk,
    trace: Arc<Trace>,
    /// Barriers still to pass before the crash; zero when not armed.
    countdown: AtomicU64,
    /// One plus the index in [`BARRIERS`] of the only barrier that counts; zero when every barrier does.
    only: AtomicU64,
    /// One plus the index in [`BARRIERS`] of the barrier the crash fell at; zero until then.
    tripped: AtomicU64,
    signal: Notify,
}

impl SimBarriers {
    pub(crate) fn new(disk: &SimDisk, trace: &Arc<Trace>) -> Self {
        Self {
            disk: disk.clone(),
            trace: Arc::clone(trace),
            countdown: AtomicU64::new(0),
            only: AtomicU64::new(0),
            tripped: AtomicU64::new(0),
            signal: Notify::new(),
        }
    }

    /// Arms the crash at the `after`-th barrier from now; with `only`, at the `after`-th barrier of that index
    /// in [`BARRIERS`], so that the rare barriers get their share of crashes.
    pub(crate) fn arm(&self, only: Option<u64>, after: u64) {
        self.only
            .store(only.map_or(0, |index| index + 1), Ordering::SeqCst);
        self.countdown.store(after.max(1), Ordering::SeqCst);
    }

    /// Resolves with the index in [`BARRIERS`] of the barrier the crash fell at.
    pub(crate) async fn tripped(&self) -> usize {
        loop {
            match self.tripped.load(Ordering::SeqCst) {
                0 => self.signal.notified().await,
                code => return (code - 1) as usize,
            }
        }
    }
}

impl Barriers for SimBarriers {
    fn reached(&self, barrier: Barrier) -> BoxFuture<'static, ()> {
        let code = match barrier {
            Barrier::BeforeSealAppend => 1,
            Barrier::SealAppended => 2,
            Barrier::BeforeInsertSend(..) => 3,
            Barrier::InsertSent(..) => 4,
            Barrier::QuarantineCopied => 5,
            Barrier::BeforeCheckpointAppend => 6,
            Barrier::CheckpointAppended => 7,
        };
        if self.tripped.load(Ordering::SeqCst) != 0 {
            return Box::pin(futures::future::pending());
        }
        let only = self.only.load(Ordering::SeqCst);
        if only != 0 && only != code {
            return Box::pin(futures::future::ready(()));
        }
        let left = self.countdown.load(Ordering::SeqCst);
        if left == 1 {
            self.trace.event("barrier.crash", &[code]);
            self.disk.freeze();
            self.tripped.store(code, Ordering::SeqCst);
            self.signal.notify_one();
            return Box::pin(futures::future::pending());
        }
        if left > 1 {
            self.countdown.store(left - 1, Ordering::SeqCst);
        }
        Box::pin(futures::future::ready(()))
    }
}

/// Stands in for the recent-span index: checks that stage B feeds it in LSN order, and pins the WAL a seeded
/// number of frames behind.
#[derive(Debug)]
struct SimRecent {
    last: AtomicU64,
    out_of_order: AtomicBool,
    pin_behind: Option<u64>,
}

impl RecentIndex for SimRecent {
    fn applied(&self, _source: &WalFrame, frame: &NormalizedFrame, _admission: &Admission) {
        let lsn = frame.lsn.get();
        if self.last.swap(lsn, Ordering::SeqCst) >= lsn {
            self.out_of_order.store(true, Ordering::SeqCst);
        }
    }

    fn passed(&self, _source: &WalFrame) {}

    fn floor(&self) -> Option<Lsn> {
        let behind = self.pin_behind?;
        let last = self.last.load(Ordering::SeqCst);
        Some(Lsn::new(last.saturating_sub(behind).max(1)))
    }
}

/// A running shipper: where its task leaves its result, and its counters.
struct Shipping {
    result: Arc<Mutex<Option<Result<(), ShipError>>>>,
    stats: Arc<ShipperStats>,
    recent: Arc<SimRecent>,
    barriers: Arc<SimBarriers>,
}

/// A quarantine directory whose pair is in fact quarantined.
#[derive(Clone, Debug)]
pub(crate) struct Quarantined {
    pub(crate) table: TableId,
    pub(crate) partition: PartitionId,
    pub(crate) first: u64,
    pub(crate) last: u64,
    /// The payloads of the copied frames.
    payloads: Vec<Vec<u8>>,
}

/// What is wrong with a quarantine.
#[derive(Clone, Debug)]
pub(crate) enum QuarantineFault {
    /// Contract 5.6: a pair of which the store had accepted a block was quarantined without a probe that read
    /// absent.
    InDoubt { table: &'static str, batch_seq: u64 },
    /// The copy does not hold the frames of its batch; the directory is named.
    Copy(String),
}

impl Quarantined {
    /// Whether a copied frame carries the span. A repeat of a request may have been dropped for a frame that
    /// was never acknowledged, whose LSN the ledger does not know.
    pub(crate) fn holds(&self, trace: &[u8; 16], span: &[u8; 8]) -> bool {
        let has = |payload: &[u8], id: &[u8]| payload.windows(id.len()).any(|w| w == id);
        self.payloads
            .iter()
            .any(|payload| has(payload, trace) && has(payload, span))
    }

    /// Whether a copied frame carries these bytes: the id of a fact.
    pub(crate) fn holds_id(&self, id: &[u8]) -> bool {
        self.payloads
            .iter()
            .any(|payload| payload.windows(id.len()).any(|w| w == id))
    }
}

pub(crate) fn add(total: &mut StatsSnapshot, s: &StatsSnapshot) {
    total.sealed += s.sealed;
    total.reshipped += s.reshipped;
    total.checkpoints += s.checkpoints;
    total.inserts += s.inserts;
    total.unknown_outcomes += s.unknown_outcomes;
    total.probes += s.probes;
    total.probes_present += s.probes_present;
    total.probes_expired += s.probes_expired;
    total.quarantined += s.quarantined;
    total.quarantined_frames += s.quarantined_frames;
    total.mixed_version_batches += s.mixed_version_batches;
    total.encoded_bytes_peak = total.encoded_bytes_peak.max(s.encoded_bytes_peak);
    total.largest_frame_bytes = total.largest_frame_bytes.max(s.largest_frame_bytes);
}

/// The payloads of a frames file, when it holds exactly the frames `first..=last`, in order and intact.
pub(crate) fn copied_payloads(bytes: &[u8], first: u64, last: u64) -> Option<Vec<Vec<u8>>> {
    let mut at = 0;
    let mut next = first;
    let mut payloads = Vec::new();
    while at < bytes.len() {
        let header: &[u8; FRAME_HEADER_LEN] =
            bytes.get(at..at + FRAME_HEADER_LEN)?.try_into().ok()?;
        let header = FrameHeader::decode(header).ok()?;
        let end = at + FRAME_HEADER_LEN + header.payload_len as usize;
        let stored = bytes.get(at + FRAME_HEADER_LEN..end)?;
        if header.lsn.get() != next || !header.payload_matches(stored) {
            return None;
        }
        payloads.push(load_payload(header.compressed, stored).ok()?.into_owned());
        next += 1;
        at = end;
    }
    (next == last + 1).then_some(payloads)
}

/// The quarantine directories on `disk`: the directory's name and, when its manifest parses, the pair and the
/// declared length of the copy. A crash can cut the manifest of a copy that was never announced.
pub(crate) fn manifests(disk: &SimDisk) -> Vec<(String, Option<(Quarantined, usize)>)> {
    let mut found = Vec::new();
    for path in disk.files(Root::Quarantine) {
        let Some(dir) = path.as_str().strip_suffix(&format!("/{MANIFEST_FILE}")) else {
            continue;
        };
        let manifest = disk.read_file(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&manifest);
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        };
        let number = |name: &str| field(name).and_then(|v| v.parse::<u64>().ok());
        let parsed = (|| {
            Some(Quarantined {
                table: TableId::from_id(u8::try_from(number("table_id")?).ok()?)?,
                partition: PartitionId::parse(field("partition")?).ok()?,
                first: number("first_lsn")?,
                last: number("last_lsn")?,
                payloads: Vec::new(),
            })
        })();
        let declared = number("bytes").unwrap_or(u64::MAX) as usize;
        found.push((dir.to_owned(), parsed.map(|q| (q, declared))));
    }
    found
}

/// The number of quarantine directories with a manifest, those whose pair is quarantined, and what is wrong
/// with them (contract 5.6). A directory whose pair landed after all is what a crash between the copy and the
/// `Quarantine` frame leaves behind.
pub(crate) fn read_quarantines(
    disk: &SimDisk,
    model: &ChModel,
    through: u64,
) -> (u64, Vec<Quarantined>, Vec<QuarantineFault>) {
    let mut found = Vec::new();
    let mut faults = Vec::new();
    let mut directories = 0;
    for (dir, parsed) in manifests(disk) {
        directories += 1;
        let Some((mut q, declared)) = parsed else {
            continue;
        };
        // Above the watermark the pair may still be shipped: the copy is not a quarantine yet.
        if q.last > through {
            continue;
        }
        let (accepted, last) = model.pair_history(&(q.table, q.partition, q.first));
        match last {
            Some(LastAnswer::ProbeAbsent) => {}
            Some(LastAnswer::InsertPoison) if !accepted => {}
            Some(LastAnswer::InsertPoison) => faults.push(QuarantineFault::InDoubt {
                table: q.table.name(),
                batch_seq: q.first,
            }),
            // The pair landed after all: the copy is what a crash before the `Quarantine` frame left.
            _ => continue,
        }
        let frames = DiskPath::new(Root::Quarantine, &format!("{dir}/{FRAMES_FILE}"))
            .ok()
            .and_then(|p| disk.read_file(&p))
            .unwrap_or_default();
        match frames
            .get(..declared)
            .and_then(|bytes| copied_payloads(bytes, q.first, q.last))
        {
            Some(payloads) => q.payloads = payloads,
            None => faults.push(QuarantineFault::Copy(dir)),
        }
        found.push(q);
    }
    (directories, found, faults)
}

struct Run<'a> {
    sim: &'a Sim,
    disk: SimDisk,
    model: ChModel,
    shared: Shared,
    rng: Arc<dyn Rng>,
    boots: u64,
    stopped: bool,
}

impl Run<'_> {
    fn plan(&self) -> &ShipPlan {
        &self.shared.plan
    }

    fn event(&self, label: &str, fields: &[u64]) {
        let micros = self.sim.elapsed().as_micros() as u64;
        self.sim.trace().event("at", &[micros]);
        self.sim.trace().event(label, fields);
    }

    fn violate(&self, violation: Violation) {
        self.shared.ledger().violate(violation);
    }

    fn failed(&self) -> bool {
        !self.shared.ledger().violations.is_empty()
    }

    /// Recovers the WAL on a new host through crashes and faults, and compares the result with the
    /// uninterrupted reference recovery. `None` when a violation ended the run.
    async fn boot(&mut self) -> Option<(Host, Wal, RecoveryReport)> {
        self.boots += 1;
        let workload_faults = self.disk.faults();
        let recovery = Recovery {
            sim: self.sim,
            disk: &self.disk,
            plan: &self.shared.plan.wal,
            rng: self.rng.as_ref(),
            boots: self.boots,
        };
        let mut counts = BootCounts::default();
        let booted = recover(&recovery, &mut self.stopped, &mut counts).await;
        {
            let stats = &mut self.shared.ledger().stats;
            stats.stops += counts.stops;
            stats.crashes += counts.crashes;
        }
        let Booted {
            host,
            wal,
            report,
            reference,
            ..
        } = match booted {
            Ok(booted) => booted,
            Err(e) => {
                self.violate(Violation::RecoveryFailed(e.to_string()));
                return None;
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
            }
            Err(e) => self.violate(Violation::LogUnreadable(e)),
        }
        self.disk.set_faults(workload_faults);
        self.check_store(report.watermark_lsn().get(), false);
        if self.failed() {
            drop(wal);
            host.stop().await;
            return None;
        }
        Some((host, wal, report))
    }

    /// The quarantine directories whose pair is quarantined, checked against contract 5.6.
    fn quarantines(&self, through: u64) -> Vec<Quarantined> {
        let (directories, found, faults) = read_quarantines(&self.disk, &self.model, through);
        for fault in faults {
            self.violate(match fault {
                QuarantineFault::InDoubt { table, batch_seq } => {
                    Violation::QuarantinedInDoubt { table, batch_seq }
                }
                QuarantineFault::Copy(dir) => Violation::QuarantineCopy(dir),
            });
        }
        self.shared.ledger().stats.quarantines = directories;
        found
    }

    /// Checks the store against the ledger. Every acknowledged request at or below `through` must have landed
    /// (I1); with `quiescent`, the derived tables are compared too.
    fn check_store(&self, through: u64, quiescent: bool) {
        let model = &self.model;
        for what in model.violations() {
            self.violate(Violation::Store(what));
        }
        if let Some((first, second)) = model.overlapping_ranges() {
            self.violate(Violation::IdentityOverlap { first, second });
        }
        let quarantined = self.quarantines(through);
        let granularity = self.plan().config.granularity;
        let in_quarantine = |table: TableId, lsn: u64, start: Timestamp| {
            route(table, granularity, start).is_ok_and(|partition| {
                quarantined.iter().any(|q| {
                    q.table == table && q.partition == partition && q.first <= lsn && lsn <= q.last
                })
            })
        };
        let mut ledger = self.shared.ledger();
        let mut found = Vec::new();
        let mut core_rows = 0u64;
        let mut core_roots: BTreeSet<(u64, [u8; 8])> = BTreeSet::new();
        for table in [TableId::SpanCore, TableId::SpanPayload] {
            let name = table.name();
            let mut by_frame: BTreeMap<(u64, [u8; 8]), u32> = BTreeMap::new();
            let mut by_key: BTreeSet<SpanKey> = BTreeSet::new();
            for part in model.parts(table) {
                let block = &part.block;
                for row in 0..block.rows {
                    let cells = (|| {
                        let lsn = u64_at(block, "lsn", row)?;
                        let span: [u8; 8] = bytes_at(block, "span_id", row)?.try_into().ok()?;
                        let trace: [u8; 16] = bytes_at(block, "trace_id", row)?.try_into().ok()?;
                        Some((lsn, uuid_at(block, "project_id", row)?, trace, span))
                    })();
                    let Some((lsn, project, trace, span)) = cells else {
                        found.push(Violation::UnknownRow {
                            table: name,
                            lsn: 0,
                        });
                        continue;
                    };
                    if !ledger.keys.contains_key(&(project, trace, span)) {
                        found.push(Violation::UnknownRow { table: name, lsn });
                    }
                    let count = by_frame.entry((lsn, span)).or_default();
                    *count += 1;
                    if *count > 1 {
                        found.push(Violation::DuplicateRow { table: name, lsn });
                    }
                    by_key.insert((project, trace, span));
                    if table == TableId::SpanCore {
                        core_rows += 1;
                        if u8_at(block, "is_root", row) == Some(1) {
                            core_roots.insert((lsn, span));
                        }
                    }
                }
            }
            for (&lsn, &index) in ledger.acked.range(..=through) {
                let request = &ledger.requests[index];
                for span in request.spans.iter().filter(|s| !s.rejected) {
                    let key = (request.project, span.trace, span.span);
                    let landed = if ledger.keys.get(&key) == Some(&1) {
                        by_frame.contains_key(&(lsn, span.span))
                    } else {
                        by_key.contains(&key)
                    };
                    let parked = in_quarantine(table, lsn, span.start)
                        || quarantined
                            .iter()
                            .any(|q| q.table == table && q.holds(&span.trace, &span.span));
                    if !landed && !parked {
                        found.push(Violation::AckedSpanLost { table: name, lsn });
                    }
                }
            }
        }

        if quiescent {
            let mut heads: BTreeMap<(u64, [u8; 8], i8), u32> = BTreeMap::new();
            for part in model.parts(TableId::TraceHeads) {
                let block = &part.block;
                for row in 0..block.rows {
                    let lsn = u64_at(block, "lsn", row).unwrap_or(0);
                    let span: [u8; 8] = bytes_at(block, "span_id", row)
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or_default();
                    let kind = i8_at(block, "head_kind", row).unwrap_or(0);
                    let count = heads.entry((lsn, span, kind)).or_default();
                    *count += 1;
                    if *count > 1 {
                        found.push(Violation::DuplicateRow {
                            table: TableId::TraceHeads.name(),
                            lsn,
                        });
                    }
                }
            }
            let mut stats_spans = 0u64;
            for part in model.parts(TableId::TraceStats) {
                let block = &part.block;
                for row in 0..block.rows {
                    stats_spans += u64_at(block, "span_count", row).unwrap_or(0);
                }
            }
            // Repeats and quarantines make the derived tables depend on what each restart remembered; without
            // them the tables must agree exactly.
            let exact = ledger.stats.resent == 0 && ledger.stats.quarantines == 0;
            if exact {
                for &(lsn, span) in &core_roots {
                    if !heads.contains_key(&(lsn, span, 1)) {
                        found.push(Violation::DerivedMismatch(format!(
                            "the root span of frame {lsn} has no head row"
                        )));
                    }
                }
                for (&lsn, &index) in &ledger.acked {
                    for span in ledger.requests[index].spans.iter() {
                        if span.root && !span.rejected && !core_roots.contains(&(lsn, span.span)) {
                            found.push(Violation::DerivedMismatch(format!(
                                "the root span of frame {lsn} is not a root in span_core"
                            )));
                        }
                    }
                }
                if stats_spans != core_rows {
                    found.push(Violation::DerivedMismatch(format!(
                        "trace_stats counts {stats_spans} spans, span_core holds {core_rows}"
                    )));
                }
            }
        }
        for violation in found {
            ledger.violate(violation);
        }
    }

    /// Starts the shipper of this incarnation. `None` when it refuses to start, which is a violation.
    fn start_shipper(
        &self,
        host: &Host,
        wal: &Wal,
        report: &RecoveryReport,
        pin_behind: Option<u64>,
    ) -> Option<Shipping> {
        let barriers = Arc::new(SimBarriers::new(&self.disk, self.sim.trace()));
        let recent = Arc::new(SimRecent {
            last: AtomicU64::new(0),
            out_of_order: AtomicBool::new(false),
            pin_behind,
        });
        let hooks = Hooks {
            barriers: barriers.clone(),
            recent: recent.clone(),
        };
        let stores = Stores::new(Arc::new(self.model.clone()));
        let config = self.plan().config.clone();
        let shipper = match Shipper::new(&host.env, &stores, wal.clone(), report, config, hooks) {
            Ok(shipper) => shipper,
            Err(e) => {
                self.violate(Violation::ShipperFailed(e.to_string()));
                return None;
            }
        };
        let stats = shipper.stats();
        let result = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&result);
        drop(host.env.spawn("shipper", async move {
            let ended = Box::pin(shipper.run()).await;
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(ended);
        }));
        Some(Shipping {
            result,
            stats,
            recent,
            barriers,
        })
    }

    /// Folds what a shipper did into the ledger. A shipper may stop because the WAL failed under it; with
    /// `healed`, and for every other error, a stop is a violation.
    fn account(&self, shipping: &Shipping, healed: bool) {
        let ended = shipping
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut ledger = self.shared.ledger();
        add(&mut ledger.stats.shipper, &shipping.stats.snapshot());
        match ended {
            None | Some(Ok(())) => {}
            Some(Err(ShipError::WalFailed)) if !healed => {}
            Some(Err(e)) => ledger.violate(Violation::ShipperFailed(e.to_string())),
        }
        if shipping.recent.out_of_order.load(Ordering::SeqCst) {
            ledger.violate(Violation::RecentOutOfOrder);
        }
    }

    async fn crash(&self, host: Host, wal: Wal) {
        drop(wal);
        self.event("crash", &[self.disk.ops()]);
        self.shared.ledger().stats.crashes += 1;
        host.crash().await;
    }

    /// One incarnation: recover, ship and take load until a seeded crash point, crash.
    async fn incarnation(&mut self) -> Option<()> {
        let (host, wal, report) = self.boot().await?;
        self.shared.ledger().stats.incarnations += 1;
        let rng = Arc::clone(&self.rng);
        let rng = rng.as_ref();
        if self.plan().wal.wall_steps {
            host.clock.step_wall(pick(rng, 400_000) as i64 - 200_000);
        }
        let pin_behind = (self.plan().wal.pin_recent && coin(rng)).then(|| pick(rng, 64));
        let shipping = self.start_shipper(&host, &wal, &report, pin_behind)?;
        for index in 0..self.plan().wal.clients {
            let label = format!("ship-sim/client-{}-{index}", self.boots);
            let budget = 3 + pick(rng, 40);
            let task = client(
                self.shared.clone(),
                host.env.clone(),
                wal.clone(),
                self.sim.rng(&label),
                budget,
            );
            drop(host.env.spawn("client", task));
        }

        // The crash point: a seeded number of disk operations or of crash barriers from now, or a seeded
        // virtual time, whichever comes first.
        let short = self.plan().wal.crash_loops && pick(rng, 4) > 0;
        let (ops, barriers, micros) = if short {
            (12, 4, 4_000)
        } else {
            match pick(rng, 5) {
                0 | 1 => (400, 12, 250_000),
                2 | 3 => (4_000, 60, 5_000_000),
                _ => (20_000, 200, 150_000_000),
            }
        };
        if coin(rng) {
            self.disk.arm_crash_after_ops(1 + pick(rng, ops));
        }
        if self.plan().barrier_crashes && coin(rng) {
            if coin(rng) {
                shipping.barriers.arm(None, 1 + pick(rng, barriers));
            } else if self.plan().faults.poison_pair_ppm > 0 && coin(rng) {
                // The rarest barrier: it needs a pair that is being quarantined.
                shipping.barriers.arm(Some(QUARANTINE_COPIED), 1);
            } else {
                // A batch has to resolve every pair before its checkpoint, so the two checkpoint barriers
                // are reached least often in an incarnation: they get a double share of the draws.
                let last = BARRIERS.len() as u64 - 1;
                let only = [0, 1, 2, 3, 4, last - 1, last, last - 1, last][pick(rng, 9) as usize];
                shipping.barriers.arm(Some(only), 1 + pick(rng, 3));
            }
        }
        let run_for = 500 + pick(rng, micros);
        let outage = (self.plan().outages && coin(rng)).then(|| {
            let length = if self.plan().long_outages && coin(rng) {
                30_000_000 + pick(rng, 270_000_000)
            } else {
                200_000 + pick(rng, 5_000_000)
            };
            (pick(rng, run_for), Duration::from_micros(length))
        });
        let at_crash_point = {
            let mut crash_point = Box::pin(self.disk.crash_point_reached()).fuse();
            let mut barrier = Box::pin(shipping.barriers.tripped()).fuse();
            let mut timer = host.env.clock.sleep(Duration::from_micros(run_for)).fuse();
            let mut outage_start = match outage {
                Some((at, _)) => host.env.clock.sleep(Duration::from_micros(at)),
                None => Box::pin(futures::future::pending()),
            }
            .fuse();
            loop {
                futures::select_biased! {
                    () = crash_point => break true,
                    at = barrier => {
                        let stats = &mut self.shared.ledger().stats;
                        stats.barrier_crashes += 1;
                        stats.crashes_at[at] += 1;
                        break true;
                    }
                    () = outage_start => {
                        if let Some((_, length)) = outage {
                            self.event("outage", &[length.as_micros() as u64]);
                            self.shared.ledger().stats.outages += 1;
                            self.model.start_outage(length);
                        }
                    }
                    () = timer => break false,
                }
            }
        };
        self.account(&shipping, false);
        if !at_crash_point && self.plan().wal.clean_stops && coin(rng) {
            self.disk.disarm_crash();
            self.event("stop", &[self.disk.ops()]);
            self.shared.ledger().stats.stops += 1;
            self.stopped = true;
            drop(wal);
            host.stop().await;
        } else {
            self.crash(host, wal).await;
        }
        (!self.failed()).then_some(())
    }

    /// Quiescence: heal every fault, drain the log, check everything, restart once more and compare.
    async fn quiesce(&mut self) -> Option<()> {
        self.disk.set_faults(DiskFaults::default());
        self.model.set_faults(ChFaults::default());
        self.model.end_outage();
        let (host, wal, report) = self.boot().await?;
        // The last frame that warrants a seal: the shipper's own control frames after it stay unsealed.
        let target = match read_log(&wal, report.first_retained_lsn()).await {
            Ok(log) => log
                .values()
                .filter(|f| {
                    !matches!(
                        f.record_type(),
                        RecordType::BatchSeal | RecordType::Checkpoint | RecordType::Quarantine
                    )
                })
                .map(|f| f.lsn().get())
                .max()
                .unwrap_or(0),
            Err(e) => {
                self.violate(Violation::LogUnreadable(e));
                0
            }
        };
        let shipping = self.start_shipper(&host, &wal, &report, None)?;
        let clock = &host.env.clock;
        let watermark = |wal: &Wal| wal.watermark().map_or(0, |w| w.last_lsn().get());
        let mut waited = 0;
        while watermark(&wal) < target && waited < 200_000 {
            clock.sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        if watermark(&wal) < target {
            self.violate(Violation::NotDrained {
                watermark: watermark(&wal),
                target,
                shipper: shipping.stats.snapshot(),
            });
        }
        // Past replay retention, however the wall clock was stepped, and past the last insert in flight.
        let retention = self.plan().wal.config.replay_retention;
        clock.sleep(retention + Duration::from_secs(10)).await;
        let in_flight = self.model.in_flight_until();
        let elapsed = self.sim.elapsed().as_micros() as u64;
        clock
            .sleep(Duration::from_micros(in_flight.saturating_sub(elapsed) + 1))
            .await;
        let segments = wal.segments();
        if let [oldest, next, ..] = segments.as_slice()
            && next.first_lsn.get().saturating_sub(1) <= watermark(&wal)
        {
            self.violate(Violation::SpaceNotReclaimed {
                segment_no: oldest.segment_no,
            });
        }
        self.account(&shipping, true);
        self.check_store(u64::MAX, true);

        // I12: a restart of a drained node ships nothing and changes nothing.
        let before = (
            self.model.digest(),
            self.model.stats().inserts,
            watermark(&wal),
        );
        self.crash(host, wal).await;
        let (host, wal, report) = self.boot().await?;
        let shipping = self.start_shipper(&host, &wal, &report, None)?;
        host.env.clock.sleep(Duration::from_secs(5)).await;
        self.account(&shipping, true);
        let after = (
            self.model.digest(),
            self.model.stats().inserts,
            watermark(&wal),
        );
        if before != after {
            self.violate(Violation::RestartNotIdempotent);
        }
        drop(wal);
        host.stop().await;
        Some(())
    }
}

/// The virtual time one run may take.
const DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let plan_rng = sim.rng("ship-sim/plan");
    let plan = Arc::new(ShipPlan::draw(plan_rng.as_ref(), &options));
    let disk = sim.disk("ship-sim/disk");
    disk.set_faults(plan.wal.faults.clone());
    let model = ChModel::new(
        sim.rng("ship-sim/store").as_ref(),
        plan.config.flavour,
        plan.config.granularity,
        plan.config.probe_valid_for,
    );
    model.set_faults(plan.faults.clone());
    let shared = Shared {
        ledger: Arc::default(),
        plan: Arc::clone(&plan),
    };
    let mut run = Run {
        sim: &sim,
        disk,
        model: model.clone(),
        shared: shared.clone(),
        rng: sim.rng("ship-sim/driver"),
        boots: 0,
        stopped: false,
    };
    let body = async {
        for _ in 0..plan.wal.incarnations {
            run.incarnation().await?;
        }
        run.quiesce().await
    };
    let finished = tokio::time::timeout(DEADLINE, Box::pin(body)).await;
    let mut ledger = shared.ledger();
    if finished.is_err() {
        ledger.violate(Violation::Deadline);
    }
    ledger.stats.store = model.stats();
    ledger.stats.virtual_micros = sim.elapsed().as_micros() as u64;
    Outcome {
        seed: sim.seed(),
        trace_hash: sim.trace().hash(),
        violations: ledger.violations.clone(),
        stats: ledger.stats,
    }
}

/// Runs the shipper simulation for one seed. The whole run, its faults and its crash points are a function of
/// `seed` and `options`.
pub fn run_ship_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
