//! The whole-system deterministic simulation of milestone 1, on one simulated node.
//!
//! Seeded OTLP clients send protobuf and JSON requests through the real ingest router into the real WAL on
//! [`SimDisk`]; the real shipper turns the durable frames into inserts into [`ChModel`]; stage B feeds a real
//! recent-span index through the shipper's hook; and reads go through the real `QueryService`. A run is a
//! function of its seed, its configuration mask ([`crate::swarm`]) and its tier.
//!
//! Incarnations end with a crash at a seeded disk operation, at a crash barrier of the shipper (contract 5.9),
//! at a seeded virtual time (which aborts every task at whatever await point it is at), or with a process stop.
//! The run ends with a quiescence phase: faults healed, the log drained, every read checked, one more restart.
//!
//! - **I1** at every restart and at quiescence, every accepted span of every acknowledged request is in
//!   `span_core` and in `span_payload`, or in a durable frame above the watermark, or in a quarantine.
//! - **I2** no (table, LSN, span) row lands twice, no span has more rows than requests that carried it, every
//!   row belongs to a submitted request, no two blocks of one (batch, table, partition) land, no two batch
//!   identities cover the same frames; at quiescence the derived tables agree with `span_core`.
//! - **I3** a trace-by-id read through the query service after every acknowledgement, from concurrent readers
//!   and at quiescence returns the acknowledged spans. A response that omits one must carry `partial` or
//!   `lagging`, and each marker must be warranted by the run's configuration: `store_unavailable` only with
//!   store outages, read faults or the tiny index bound, `quarantined` only with poison pairs. At quiescence
//!   no marker is accepted except `quarantined` when a quarantine exists.
//! - **I4** the fact rows in the store resolve, through the reference fold, to what the facts as they were
//!   written resolve to, in any order of the rows. Every acknowledged fact is in its table exactly once, in a
//!   durable frame above the watermark, or in a quarantine (I1, I2).
//! - **I5** a refusal for load is a 429 or a 503 with `Retry-After`, and the rows the shipper holds stay within
//!   `shipper.encoded_bytes`.
//! - **I6** no read returns a span or a trace of another project (the projects share trace ids).
//! - **I7** every `span_core` row carries the tokens, the cost and the error status its span reported, and at
//!   quiescence `trace_stats`, `session_rollup`, `user_rollup` and `span_metrics` equal the reference
//!   aggregation over `span_core` exactly: counts, error counts, token and cost sums, first and last seen,
//!   and the duration histograms.
//! - **I12** the recovered log equals the uninterrupted reference recovery, and a restart after quiescence
//!   ships nothing and changes nothing.
//! - **I13** at quiescence no segment wholly at or below the watermark is left and the index is empty.
//! - **I14** the run's trace hash, which includes every read's result, is a function of the inputs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use crucible_auth::{PublicKey, SecretKey, StaticKey, StaticKeys};
use crucible_chfmt::Compression;
use crucible_env::{DiskPath, Env, QueryError, Rng, Root, StoreFlavour, Stores};
use crucible_ingest::{AcceptedFact, BucketConfig, FactAuthor, Ingest, IngestConfig, NewFact};
use crucible_normalize::{IngestFrame, NormCtx, PriceTable, normalize};
use crucible_plan::{Caller, Filter};
use crucible_query::{
    Consistency, Meta, PartialReason, QueryService, QueryServiceError, ReadContext, SpanSource,
    TraceListQuery,
};
use crucible_recent::{RecentConfig, RecentIndex, SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{
    ApiKeyId, AuthorKind, CorrectionBody, FactBody, FactTargetKind, FlagBody, Lsn, PayloadCaps,
    ProjectId, ScoreBody, ScoreDataType, ScoreSource, SpanId, TableId, TagBody, TagOp, TimeWindow,
    Timestamp, TraceId, Uuid,
};
use crucible_schema::{Granularity, RetentionSettings};
use crucible_shipper::{
    FRAMES_FILE, FrameFault, FrameManifest, Hooks, MANIFEST_FILE, RecentHook, ShipError, Shipper,
    ShipperConfig, ShipperStats, StatsSnapshot,
};
use crucible_wal::{RecoveryReport, Wal, WalConfig, WalFrame};
use crucible_wal_codec::{Record, RecordType};
use futures::FutureExt;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, KeyValueList, any_value};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message;
use tower::ServiceExt;

use crate::boot::{BootCounts, Booted, Recovery, recover};
use crate::ch_model::{ChFaults, ChModel, ChStats, bytes_at, i8_at, u8_at, u64_at, uuid_at};
use crate::disk::{DiskFaults, SimDisk};
use crate::runtime::{Host, SIM_EPOCH_MICROS, Sim, run};
use crate::ship_sim::{
    BARRIERS, QUARANTINE_COPIED, QuarantineFault, SimBarriers, add, copied_payloads, manifests,
    random, read_quarantines,
};
use crate::swarm::{ConfigMask, Dim, RunRecord};
use crate::system_oracle::{self, FactRow};
use crate::trace::Trace;
use crate::wal_sim::{self, LogState, coin, pick, read_log};

mod lists;

/// How hostile a run is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Faults at rates and lengths a deployment can meet. Every invariant must hold.
    Realistic,
    /// Higher fault rates, more incarnations and, with [`Dim::FsyncLie`], a disk that lies about syncs. Under
    /// the lie the losses that follow from it are labelled, not failures.
    Stress,
}

impl Tier {
    /// The name used in records and on the command line.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Realistic => "realistic",
            Self::Stress => "stress",
        }
    }

    /// The tier with this name.
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Realistic, Self::Stress]
            .into_iter()
            .find(|tier| tier.name() == name)
    }
}

/// A seeded defect of a product crate that the simulation must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// `crucible-wal` completes a group's waiters before its `sync_data`.
    SkipWalSync,
    /// `crucible-shipper` gives every insert attempt a new deduplication identity.
    DropToken,
    /// `crucible-wal` lets garbage collection remove segments above the checkpoint watermark.
    GcAboveCheckpoint,
    /// `crucible-recent` drops entries at its bound without recording a gap.
    SilentEviction,
    /// `crucible-shipper` retries after an unknown outcome without the existence probe.
    SkipProbeAfterUnknown,
    /// `crucible-ingest` answers 200 before the WAL append resolves.
    AckBeforeAppend,
    /// `crucible-shipper` counts one row of every rollup block twice.
    DoubleCountRollup,
    /// `crucible-shipper` counts a pair of a fact table as landed when its insert is retried.
    DropFactOnRetry,
    /// `crucible-shipper` quarantines a frame that does not normalize without copying it.
    FrameQuarantineWithoutCopy,
}

impl Mutant {
    /// Every mutant.
    pub const ALL: [Self; 9] = [
        Self::SkipWalSync,
        Self::DropToken,
        Self::GcAboveCheckpoint,
        Self::SilentEviction,
        Self::SkipProbeAfterUnknown,
        Self::AckBeforeAppend,
        Self::DoubleCountRollup,
        Self::DropFactOnRetry,
        Self::FrameQuarantineWithoutCopy,
    ];

    /// The name used on the command line.
    pub const fn name(self) -> &'static str {
        match self {
            Self::SkipWalSync => "skip-wal-sync",
            Self::DropToken => "drop-token",
            Self::GcAboveCheckpoint => "gc-above-checkpoint",
            Self::SilentEviction => "silent-eviction",
            Self::SkipProbeAfterUnknown => "skip-probe-after-unknown",
            Self::AckBeforeAppend => "ack-before-append",
            Self::DoubleCountRollup => "double-count-rollup",
            Self::DropFactOnRetry => "drop-fact-on-retry",
            Self::FrameQuarantineWithoutCopy => "frame-quarantine-without-copy",
        }
    }

    /// The mutant with this name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mutant| mutant.name() == name)
    }
}

/// The inputs of a run besides its seed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// The configuration: which workload features, faults and config flags the run has.
    pub mask: ConfigMask,
    /// The tier.
    pub tier: Tier,
    /// A seeded defect that the run must catch.
    pub mutant: Option<Mutant>,
}

impl Options {
    /// A realistic run of `mask` without a mutant.
    pub const fn realistic(mask: ConfigMask) -> Self {
        Self {
            mask,
            tier: Tier::Realistic,
            mutant: None,
        }
    }
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
    /// I2: a span has more rows in a table than requests carried it.
    TooManyRows {
        /// The table.
        table: &'static str,
    },
    /// A row that no submitted request explains.
    UnknownRow {
        /// The table.
        table: &'static str,
    },
    /// I1: an accepted span of an acknowledged request is neither in the table, nor in a durable frame above
    /// the watermark, nor in a quarantine.
    AckedSpanLost {
        /// The table.
        table: &'static str,
        /// The request, in submission order.
        request: usize,
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
    /// I3: a read omitted an acknowledged span and carried neither `partial` nor `lagging`.
    SilentMiss {
        /// The read: `trace` or `list`, with `quiescent` at quiescence, or `not_found`.
        read: &'static str,
        /// The request, in submission order.
        request: usize,
    },
    /// I3: a read carried a marker that nothing in the run's configuration warrants.
    UnwarrantedMarker(&'static str),
    /// A read failed with an error that no fault explains.
    ReadFailed(String),
    /// I6: a read returned a span or a trace that the project never sent.
    ForeignData(&'static str),
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
    /// I13: at quiescence, past the visibility grace, the index still held entries.
    IndexNotDrained {
        /// The entries left.
        entries: u64,
    },
    /// I12: a restart after quiescence sent inserts or changed the store or the watermark.
    RestartNotIdempotent,
    /// The run did not finish within its virtual-time deadline.
    Deadline,
    /// I1: an acknowledged fact is neither in its fact table, nor in a durable frame above the watermark, nor
    /// in a quarantine.
    AckedFactLost {
        /// The table.
        table: &'static str,
        /// The fact write, in submission order.
        write: usize,
    },
    /// I2: a fact row landed twice, or a fact row that no write explains.
    FactRow(String),
    /// I4: the facts in the store do not resolve to what the reference fold gives for the facts written, or
    /// the result depends on the order of the rows.
    FactResolution(String),
    /// I7: a count, an error count, a token sum or a cost sum differs from the reference aggregation.
    Aggregate(String),
    /// I5: a tracked budget went above its cap, or a refusal was not a 429 or 503 with `Retry-After`.
    Backpressure(String),
    /// Contract 8.6: the WAL locator of an index entry does not lead back to its span.
    Locator {
        /// The request, in submission order.
        request: usize,
    },
}

impl Violation {
    /// The invariant of `docs/testing/invariants.md` that the violation breaks; `liveness` for a run that did
    /// not drain or finish.
    pub const fn invariant(&self) -> &'static str {
        match self {
            Self::AckedSpanLost { .. } | Self::AckedFactLost { .. } | Self::LogUnreadable(_) => {
                "I1"
            }
            Self::FactRow(_) => "I2",
            Self::FactResolution(_) => "I4",
            Self::Aggregate(_) => "I7",
            Self::Backpressure(_) => "I5",
            Self::Locator { .. } => "I3",
            Self::Store(_)
            | Self::IdentityOverlap { .. }
            | Self::DuplicateRow { .. }
            | Self::TooManyRows { .. }
            | Self::UnknownRow { .. }
            | Self::DerivedMismatch(_)
            | Self::QuarantinedInDoubt { .. }
            | Self::QuarantineCopy(_) => "I2",
            Self::SilentMiss { .. } | Self::UnwarrantedMarker(_) | Self::ReadFailed(_) => "I3",
            Self::ForeignData(_) => "I6",
            Self::RecoveryFailed(_) | Self::RecoveryDiverged | Self::RestartNotIdempotent => "I12",
            Self::SpaceNotReclaimed { .. } | Self::IndexNotDrained { .. } => "I13",
            Self::ShipperFailed(_) | Self::NotDrained { .. } | Self::Deadline => "liveness",
        }
    }
}

/// Counters of one run, for the records, the report and the coverage assertions of the suite.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Incarnations that reached the workload.
    pub incarnations: u64,
    /// Crashes, including crashes during WAL recovery.
    pub crashes: u64,
    /// Crashes while the WAL recovered.
    pub recovery_crashes: u64,
    /// Crashes at a crash barrier of the shipper, by barrier, in the order of [`BARRIERS`].
    pub crashes_at: [u64; 7],
    /// Incarnations that ended with a process stop.
    pub stops: u64,
    /// Requests sent, repeats included.
    pub sent: u64,
    /// Requests acknowledged with 200 and no rejected span.
    pub acked: u64,
    /// Of those, JSON requests.
    pub acked_json: u64,
    /// Of those, repeats of an earlier request.
    pub acked_repeats: u64,
    /// Requests the node refused or did not answer cleanly.
    pub refused: u64,
    /// Trace-by-id reads checked.
    pub trace_reads: u64,
    /// Of those, reads that returned every acknowledged span, whole and strong.
    pub reads_whole: u64,
    /// Trace-by-id reads while the workload ran, where a marker may stand in for a span.
    pub live_reads: u64,
    /// Of those, reads that returned every acknowledged span, whole and strong.
    pub live_whole: u64,
    /// Acknowledged spans that a read returned from the recent-span index.
    pub spans_from_index: u64,
    /// Acknowledged spans that a read returned from the store.
    pub spans_from_store: u64,
    /// Reads answered `consistency: lagging`.
    pub reads_lagging: u64,
    /// Reads answered `partial` with `store_unavailable`.
    pub reads_gap: u64,
    /// Reads answered `partial` with `quarantined`.
    pub reads_quarantined: u64,
    /// Reads the store did not answer and the index could not serve.
    pub reads_unavailable: u64,
    /// Trace-list pages checked.
    pub list_pages: u64,
    /// Store outages.
    pub outages: u64,
    /// Quarantine directories at the end.
    pub quarantines: u64,
    /// Of those, frames quarantined because they do not normalize.
    pub frame_quarantines: u64,
    /// Fact writes acknowledged.
    pub fact_writes: u64,
    /// Fact writes refused.
    pub fact_writes_refused: u64,
    /// Fact rows in the store at the end.
    pub fact_rows: u64,
    /// Keys the facts in the store resolve to a value for, at the end.
    pub fact_keys: u64,
    /// Rollup keys compared with the reference aggregation at the end.
    pub rollup_keys: u64,
    /// Requests refused with 429.
    pub throttled: u64,
    /// Spans re-read through the WAL locator of their index entry.
    pub locator_reads: u64,
    /// Requests that carry a span of an earlier request beside their own.
    pub partial_repeats: u64,
    /// The shipper's counters, summed over its incarnations.
    pub shipper: StatsSnapshot,
    /// The store's counters.
    pub store: ChStats,
    /// Virtual microseconds the run took.
    pub virtual_micros: u64,
}

impl Stats {
    /// The coverage targets the run reached, for the swarm report.
    pub fn targets(&self) -> Vec<String> {
        let mut targets: Vec<String> = Vec::new();
        for (name, count) in BARRIERS.iter().zip(self.crashes_at) {
            if count > 0 {
                targets.push(format!("crash at {name}"));
            }
        }
        let simple = [
            (self.recovery_crashes, "crash during wal recovery"),
            (self.stops, "process stop"),
            (self.acked_json, "json request acknowledged"),
            (self.acked_repeats, "repeated request acknowledged"),
            (self.refused, "request refused"),
            (self.spans_from_index, "span read from the recent index"),
            (self.spans_from_store, "span read from the store"),
            (self.reads_lagging, "read answered lagging"),
            (self.reads_gap, "read answered partial (store_unavailable)"),
            (
                self.reads_quarantined,
                "read answered partial (quarantined)",
            ),
            (self.reads_unavailable, "read refused (store unavailable)"),
            (self.outages, "store outage"),
            (self.quarantines, "quarantine"),
            (self.frame_quarantines, "frame quarantined"),
            (self.fact_writes, "fact write acknowledged"),
            (self.fact_writes_refused, "fact write refused"),
            (self.fact_keys, "facts resolved against the reference fold"),
            (self.rollup_keys, "rollups compared with the reference"),
            (self.throttled, "request throttled (429)"),
            (self.locator_reads, "span re-read through its locator"),
            (
                self.partial_repeats,
                "request with a repeated span acknowledged",
            ),
            (
                self.shipper.quarantined_frames,
                "shipper quarantined a frame",
            ),
            (
                self.shipper.reshipped,
                "sealed batch shipped again after a restart",
            ),
            (self.shipper.probes_present, "probe read present"),
            (
                self.shipper.probes_expired,
                "probe answer expired before its send",
            ),
            (self.store.deduplicated, "insert deduplicated"),
            (self.store.late, "insert landed late"),
            (self.store.expired, "deduplication entry expired early"),
            (self.store.merges, "parts merged in the store"),
            (self.store.read_errors, "store read fault"),
        ];
        for (count, name) in simple {
            if count > 0 {
                targets.push(name.to_owned());
            }
        }
        targets
    }
}

/// The result of one run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// The run seed.
    pub seed: u64,
    /// The inputs besides the seed.
    pub options: Options,
    /// Hash of the run's event trace.
    pub trace_hash: String,
    /// Every violation the oracle found; empty when the invariants held.
    pub violations: Vec<Violation>,
    /// Whether the run had the fsync lie, under which acknowledged data can be lost by construction.
    pub fsync_lie: bool,
    /// Counters.
    pub stats: Stats,
}

impl Outcome {
    /// Whether the run counts as a failure: a violation outside a run with the fsync lie, or a run that did not
    /// finish.
    pub fn failed(&self) -> bool {
        if self.fsync_lie {
            self.violations.contains(&Violation::Deadline)
        } else {
            !self.violations.is_empty()
        }
    }

    /// `pass`, `fail` or `loss-expected`.
    pub fn outcome(&self) -> &'static str {
        if self.failed() {
            "fail"
        } else if self.violations.is_empty() {
            "pass"
        } else {
            "loss-expected"
        }
    }

    /// The run as a line of the run log or of the corpus.
    pub fn record(&self, note: &str) -> RunRecord {
        RunRecord {
            config: self.options.mask.0,
            seed: self.seed,
            tier: self.options.tier.name().to_owned(),
            outcome: self.outcome().to_owned(),
            invariant: self
                .violations
                .first()
                .map(|violation| violation.invariant().to_owned()),
            trace_hash: self.trace_hash.clone(),
            targets: self.stats.targets(),
            note: note.to_owned(),
        }
    }
}

/// What the mask and the seed decide about one run.
#[derive(Debug)]
struct Plan {
    mask: ConfigMask,
    wal: wal_sim::Plan,
    ship: ShipperConfig,
    ingest: IngestConfig,
    recent: RecentConfig,
    faults: ChFaults,
    projects: usize,
    fsync_lie: bool,
    stress: bool,
    mutant: Option<Mutant>,
}

/// The first instant of the span start times: a few days before the simulated wall clock, as real spans are.
const START_BASE_MICROS: i64 = SIM_EPOCH_MICROS - 4 * 86_400_000_000;

impl Plan {
    fn draw(rng: &dyn Rng, options: &Options) -> Self {
        let mask = options.mask;
        let has = |dim| mask.has(dim);
        let stress = options.tier == Tier::Stress;
        let choose = |values: &[u64]| values[pick(rng, values.len() as u64) as usize];
        let millis = |values: &[u64]| Duration::from_millis(choose(values));
        let scale = if stress { 3 } else { 1 };
        let fault = |dim: Dim, ppm: u32| if has(dim) { ppm * scale } else { 0 };

        let segment_bytes = if has(Dim::SmallSegments) {
            choose(&[8 << 10, 16 << 10])
        } else {
            256 << 10
        };
        let max_segments = choose(&[6, 12, 48]);
        let mut disk = DiskFaults {
            write_error_ppm: fault(Dim::DiskWriteErrors, 4_000),
            short_write_ppm: fault(Dim::DiskWriteErrors, 4_000),
            read_error_ppm: fault(Dim::DiskReadFaults, 8_000),
            bit_flip_ppm: fault(Dim::DiskReadFaults, 8_000),
            sync_error_ppm: fault(Dim::DiskSyncErrors, 8_000),
            ..DiskFaults::default()
        };
        if has(Dim::SlowSync) {
            disk.sync_latency_micros = (50, 4_000);
        }
        if has(Dim::DiskFull) {
            disk.capacity_bytes = Some(segment_bytes * (3 + pick(rng, max_segments)));
        }
        let fsync_lie = stress && has(Dim::FsyncLie);
        if fsync_lie {
            disk.fsync_lie_ppm = 60_000;
        }
        let wal_mutant = match options.mutant {
            Some(Mutant::SkipWalSync) => Some(crucible_wal::Mutant::AckBeforeSync),
            Some(Mutant::GcAboveCheckpoint) => Some(crucible_wal::Mutant::GcAboveCheckpoint),
            _ => None,
        };
        let crash_loops = has(Dim::CrashLoops);
        let incarnations = if crash_loops {
            6 + pick(rng, 8)
        } else {
            2 + pick(rng, 3)
        };
        let wal = wal_sim::Plan {
            config: WalConfig {
                segment_bytes,
                max_bytes: segment_bytes * max_segments,
                min_free_bytes: if coin(rng) { segment_bytes } else { 0 },
                replay_retention: if has(Dim::ReplayRetention) {
                    Duration::from_secs(900)
                } else {
                    millis(&[0, 40])
                },
                mutant: wal_mutant,
                ..WalConfig::default()
            },
            incarnations: incarnations * if stress { 2 } else { 1 },
            clients: 1 + pick(rng, 3),
            compressible: false,
            uniform_len: None,
            bursts: has(Dim::Bursts),
            crash_loops,
            pin_recent: false,
            tail_reader: false,
            clean_stops: has(Dim::CleanStops),
            crash_in_recovery: has(Dim::CrashInRecovery),
            double_recovery: false,
            wall_offset_micros: if has(Dim::WallClockSteps) {
                pick(rng, 600_000_000) as i64 - 300_000_000
            } else {
                0
            },
            wall_steps: has(Dim::WallClockSteps),
            faults: disk,
        };

        let small = has(Dim::SmallBatches);
        let seal_rows = if small {
            choose(&[3, 40]) as usize
        } else {
            131_072
        };
        let one = has(Dim::OneConnection);
        let flavour = if has(Dim::Clustered) {
            StoreFlavour::Clustered
        } else {
            StoreFlavour::SingleNode
        };
        let window = |on: bool| if on { 8 } else { 4_000_000 };
        let mut ship = ShipperConfig {
            linger: if small {
                millis(&[2, 20])
            } else {
                millis(&[20, 500])
            },
            max_linger: Duration::from_secs(2),
            seal_rows,
            max_seal_rows: (seal_rows * 2).min(262_144),
            seal_bytes: if small { 600 } else { 128 << 20 },
            pressure_latency: millis(&[50, 2_000]),
            max_batches_in_flight: if one { 1 } else { 2 },
            connections: if one { 1 } else { choose(&[2, 8]) as usize },
            compression: if coin(rng) {
                Compression::Lz4
            } else {
                Compression::Zstd
            },
            flavour,
            granularity: if has(Dim::WeekPartitions) {
                Granularity::Week
            } else {
                Granularity::Day
            },
            probe_valid_for: millis(&[2_000, 60_000]),
            insert_timeout: millis(&[300, 90_000]),
            backoff_min: millis(&[1, 100]),
            backoff_max: millis(&[200, 30_000]),
            tail_poll: millis(&[2, 20]),
            gc_interval: millis(&[20, 1_000]),
            duplicate_window_keys: window(coin(rng)),
            first_seen_traces: window(coin(rng)),
            mutant: match options.mutant {
                Some(Mutant::DropToken) => Some(crucible_shipper::Mutant::DropToken),
                Some(Mutant::SkipProbeAfterUnknown) => {
                    Some(crucible_shipper::Mutant::SkipProbeAfterUnknown)
                }
                Some(Mutant::DoubleCountRollup) => {
                    Some(crucible_shipper::Mutant::DoubleCountRollup)
                }
                Some(Mutant::DropFactOnRetry) => Some(crucible_shipper::Mutant::DropFactOnRetry),
                Some(Mutant::FrameQuarantineWithoutCopy) => {
                    Some(crucible_shipper::Mutant::FrameQuarantineWithoutCopy)
                }
                _ => None,
            },
            ..ShipperConfig::default()
        };
        let mut ingest = IngestConfig {
            compression: if has(Dim::WalCompressionOff) {
                crucible_wal_codec::Compression::None
            } else {
                crucible_wal_codec::Compression::Lz4
            },
            mutant: (options.mutant == Some(Mutant::AckBeforeAppend))
                .then_some(crucible_ingest::Mutant::AckBeforeAppend),
            ..IngestConfig::default()
        };
        let recent_bytes = if has(Dim::TinyRecent) {
            2_000 + pick(rng, 6_000)
        } else {
            64 << 20
        };
        let storm = coin(rng) && coin(rng);
        let faults = ChFaults {
            timeout_lost_ppm: fault(Dim::ChTimeouts, 30_000),
            timeout_late_ppm: fault(Dim::ChTimeouts, 80_000),
            ack_lost_ppm: fault(Dim::ChAckLost, 50_000),
            server_error_ppm: fault(Dim::ChServerErrors, 20_000),
            too_many_parts_ppm: fault(Dim::ChRejects, if storm { 150_000 } else { 50_000 }),
            memory_ppm: fault(Dim::ChRejects, 30_000),
            too_many_queries_ppm: fault(Dim::ChRejects, 30_000),
            hang_ppm: fault(Dim::ChHangOverload, 150_000),
            overload_ppm: fault(Dim::ChHangOverload, 300_000),
            overload_micros: (1_000_000, 90_000_000),
            probe_error_ppm: fault(Dim::ChProbeErrors, 50_000),
            read_error_ppm: fault(Dim::ChReadErrors, 100_000),
            poison_pair_ppm: fault(Dim::ChPoison, 60_000),
            poison_first_in_flight_ppm: 500_000,
            expire_ppm: fault(Dim::WindowExpiry, 700_000) / scale,
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
        };
        // The settings of the milestone 2 dimensions are drawn last, so that a mask without them draws what it
        // drew before they existed.
        if has(Dim::SmallBudget) {
            ship.encoded_bytes = choose(&[20_000, 200_000]);
        }
        if has(Dim::FrameFaults) {
            let ppm = fault(Dim::FrameFaults, 40_000);
            ship.frame_fault = Some(FrameFault::new(rng.next_u64(), ppm));
        }
        if has(Dim::ProjectRate) {
            ingest.project_rate = Some(BucketConfig {
                rate_per_s: choose(&[60_000, 2_000_000]),
                burst: 128 << 10,
            });
        }
        Self {
            mask,
            wal,
            ship,
            ingest,
            recent: RecentConfig::new(recent_bytes, flavour),
            faults,
            projects: if has(Dim::MultiProject) { 3 } else { 1 },
            fsync_lie,
            stress,
            mutant: options.mutant,
        }
    }
}

fn project_id(project: usize) -> ProjectId {
    ProjectId::from_bytes([project as u8 + 1; 16])
}

fn secret(project: usize) -> String {
    let digit = char::from(b'1' + project as u8);
    format!("csk_{}", digit.to_string().repeat(43))
}

type SpanKey = (usize, [u8; 16], [u8; 8]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SpanInfo {
    trace: [u8; 16],
    span: [u8; 8],
    root: bool,
    /// What the span reports: tokens, the provided cost in nano-USD, and an error status.
    sent: system_oracle::Sent,
}

/// A span a client sent, kept to send it again inside a later request.
#[derive(Clone, Debug)]
struct SentSpan {
    project: usize,
    info: SpanInfo,
    wire: Span,
}

/// One fact write: what was handed in and, once acknowledged, what the server assigned.
#[derive(Clone, Debug)]
struct FactWrite {
    project: usize,
    facts: Arc<Vec<NewFact>>,
    accepted: Option<Vec<AcceptedFact>>,
}

/// One request body: what the oracle expects of it, and its bytes for a repeat.
#[derive(Clone, Debug)]
struct Request {
    project: usize,
    spans: Arc<Vec<SpanInfo>>,
    body: Arc<Vec<u8>>,
    json: bool,
    acked: bool,
    /// Whether the request carries a span of an earlier request beside its own.
    partial_repeat: bool,
}

/// The oracle ledger.
#[derive(Debug, Default)]
struct Ledger {
    requests: Vec<Request>,
    /// The acknowledged requests, in the order of their first acknowledgement.
    acked: Vec<usize>,
    /// How many requests sent carry each span key, repeats included.
    keys: BTreeMap<SpanKey, u32>,
    /// Every span sent of each trace of each project.
    traces: BTreeMap<(usize, [u8; 16]), BTreeSet<[u8; 8]>>,
    /// What each span key reports, as every request that carries it sends it.
    sent: BTreeMap<SpanKey, system_oracle::Sent>,
    /// Every fact write, in submission order.
    fact_writes: Vec<FactWrite>,
    repeats: u64,
    violations: Vec<Violation>,
    stats: Stats,
}

impl Ledger {
    fn sending(&mut self, index: usize) {
        let request = self.requests[index].clone();
        for span in request.spans.iter() {
            *self
                .keys
                .entry((request.project, span.trace, span.span))
                .or_default() += 1;
            self.traces
                .entry((request.project, span.trace))
                .or_default()
                .insert(span.span);
            self.sent
                .insert((request.project, span.trace, span.span), span.sent);
        }
        self.stats.sent += 1;
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

struct Shared {
    ledger: Mutex<Ledger>,
    plan: Plan,
    trace: Arc<Trace>,
}

impl Shared {
    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn context(&self, project: usize) -> Option<ReadContext> {
        let scope = crucible_auth::test_support::scope(project_id(project)).ok()?;
        Some(ReadContext::new(
            scope,
            RetentionSettings::default(),
            Caller::Session,
        ))
    }
}

/// Eight bytes that stand for text that is not valid UTF-8 until the body is encoded.
const MARKER: &[u8; 8] = b"BADUTF8!";
/// What replaces the marker in a protobuf body: overlong, surrogate and stray bytes.
const BAD_PROTOBUF: &[u8; 8] = &[0xff, 0xfe, 0xc0, 0xaf, 0xed, 0xa0, 0x80, 0xff];
/// What replaces the marker in a JSON body: a lone surrogate escape.
const BAD_JSON: &[u8; 8] = b"\\ud800xx";

fn text(value: String) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(value)),
    }
}

fn attribute(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(value),
        ..KeyValue::default()
    }
}

fn int(value: u64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::IntValue(value as i64)),
    }
}

/// What a client remembers between its requests.
#[derive(Default)]
struct ClientState {
    /// Recent trace ids, which later requests and fact writes reuse.
    traces: Vec<[u8; 16]>,
    /// Recent spans, which later requests send again beside their own.
    spans: Vec<SentSpan>,
}

/// Builds one request: a few spans, mostly of one new or recent trace. Spans report a session, a user, an
/// environment, a release, a model, usage, a provided cost and an error status from small seeded domains, so
/// that the rollups have keys that collide and measures to sum.
fn generate(plan: &Plan, rng: &dyn Rng, state: &mut ClientState) -> Option<Request> {
    let traces = &mut state.traces;
    let has = |dim| plan.mask.has(dim);
    let project = pick(rng, plan.projects as u64) as usize;
    let json = has(Dim::Json) && coin(rng);
    let bad = has(Dim::BadUnicode) && coin(rng);
    let count = 1 + pick(rng, 4);
    let new_trace = |traces: &mut Vec<[u8; 16]>| {
        let trace = random::<16>(rng);
        traces.push(trace);
        if traces.len() > 24 {
            traces.remove(0);
        }
        trace
    };
    let reuse_main = !traces.is_empty() && pick(rng, 3) == 0;
    let main = if reuse_main {
        traces[pick(rng, traces.len() as u64) as usize]
    } else {
        new_trace(traces)
    };
    let mut rootless = reuse_main || (has(Dim::Rootless) && coin(rng));
    let mut spans = Vec::new();
    let mut wire = Vec::new();
    // A span of an earlier request of the project, sent again as it was: stage B drops it when the duplicate
    // window still holds it, which shifts the spans after it in the frame.
    let again = state
        .spans
        .iter()
        .filter(|sent| sent.project == project)
        .nth(pick(rng, 4) as usize)
        .filter(|_| has(Dim::ClientRetries) && pick(rng, 4) == 0)
        .cloned();
    let partial_repeat = again.is_some();
    if let Some(sent) = again {
        spans.push(sent.info);
        wire.push(sent.wire);
    }
    let session = (pick(rng, 3) > 0).then(|| format!("session-{}", pick(rng, 3)));
    for ordinal in 0..count {
        let other = traces.len() > 1 && pick(rng, 5) == 0;
        let trace = if other {
            traces[pick(rng, traces.len() as u64) as usize]
        } else {
            main
        };
        let span = random::<8>(rng);
        let root = !other && !rootless;
        // One root per trace.
        rootless = true;
        let day = if has(Dim::MultiDay) { pick(rng, 3) } else { 1 };
        let start_micros =
            START_BASE_MICROS + (day * 86_400_000_000 + pick(rng, 3_600_000_000)) as i64;
        let start_nanos = (start_micros as u64) * 1_000;
        let mut attributes = vec![attribute("sim.ordinal", text(format!("{ordinal}")))];
        if has(Dim::LargePayloads) {
            let len = [0, 300, 3_000, 6_000][pick(rng, 4) as usize];
            attributes.push(attribute("sim.blob", text("x".repeat(len))));
        }
        if has(Dim::DeepNesting) {
            let depth = [4, 30, 90][pick(rng, 3) as usize];
            let mut value = text("leaf".to_owned());
            for _ in 0..depth {
                value = AnyValue {
                    value: Some(any_value::Value::KvlistValue(KeyValueList {
                        values: vec![attribute("n", value)],
                    })),
                };
            }
            attributes.push(attribute("sim.nested", value));
        }
        let mut name = format!("op-{}", ordinal % 4);
        if bad {
            name.push_str("BADUTF8!");
            attributes.push(attribute("sim.text", text("a BADUTF8! b".to_owned())));
        }
        if let Some(session) = &session {
            attributes.push(attribute("crucible.session.id", text(session.clone())));
        }
        if coin(rng) {
            attributes.push(attribute("user.id", text(format!("user-{}", pick(rng, 3)))));
        }
        if pick(rng, 3) == 0 {
            let environment = ["prod", "staging"][pick(rng, 2) as usize];
            attributes.push(attribute(
                "crucible.environment",
                text(environment.to_owned()),
            ));
        }
        if pick(rng, 4) == 0 {
            attributes.push(attribute("crucible.release", text("r1".to_owned())));
        }
        let mut sent = system_oracle::Sent::default();
        if coin(rng) {
            sent.input = pick(rng, 1_000);
            sent.output = pick(rng, 500);
            attributes.push(attribute("gen_ai.usage.input_tokens", int(sent.input)));
            attributes.push(attribute("gen_ai.usage.output_tokens", int(sent.output)));
            attributes.push(attribute(
                "gen_ai.response.model",
                text(format!("sim-model-{}", pick(rng, 2))),
            ));
            if coin(rng) {
                attributes.push(attribute("gen_ai.operation.name", text("chat".to_owned())));
                attributes.push(attribute(
                    "crucible.completion_start_time",
                    int(start_nanos + 1_000 * pick(rng, 900)),
                ));
            }
            if coin(rng) {
                // A provided cost in whole micro-dollars, as a decimal string of USD.
                let micro_usd = pick(rng, 5_000);
                sent.cost_nusd = micro_usd as i64 * 1_000;
                attributes.push(attribute(
                    "crucible.cost.total",
                    text(format!("0.{micro_usd:06}")),
                ));
            }
        }
        sent.error = pick(rng, 5) == 0;
        let info = SpanInfo {
            trace,
            span,
            root,
            sent,
        };
        let span = Span {
            trace_id: trace.to_vec(),
            span_id: span.to_vec(),
            parent_span_id: if root { Vec::new() } else { vec![7; 8] },
            name,
            kind: 1,
            start_time_unix_nano: start_nanos,
            end_time_unix_nano: start_nanos + 1_500_000 * (1 + ordinal),
            attributes,
            status: sent.error.then(|| Status {
                message: String::new(),
                code: 2,
            }),
            ..Span::default()
        };
        state.spans.push(SentSpan {
            project,
            info,
            wire: span.clone(),
        });
        if state.spans.len() > 16 {
            state.spans.remove(0);
        }
        spans.push(info);
        wire.push(span);
    }
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
    };
    let mut body = if json {
        serde_json::to_vec(&otlp).ok()?
    } else {
        otlp.encode_to_vec()
    };
    if bad {
        let replacement = if json { BAD_JSON } else { BAD_PROTOBUF };
        let mut at = 0;
        while at + MARKER.len() <= body.len() {
            if &body[at..at + MARKER.len()] == MARKER {
                body[at..at + MARKER.len()].copy_from_slice(replacement);
                at += MARKER.len();
            } else {
                at += 1;
            }
        }
    }
    Some(Request {
        project,
        spans: Arc::new(spans),
        body: Arc::new(body),
        json,
        acked: false,
        partial_repeat,
    })
}

/// Builds one fact write: a few facts about recent traces, from small key domains so that versions collide
/// on a key and retractions meet the values they retract.
fn generate_facts(rng: &dyn Rng, state: &ClientState) -> Vec<NewFact> {
    let count = 1 + pick(rng, 5);
    (0..count)
        .map(|_| {
            let trace = match state.traces.len() as u64 {
                0 => [0x5a; 16],
                n => state.traces[pick(rng, n) as usize],
            };
            let on_span = pick(rng, 4) == 0;
            let in_session = !on_span && pick(rng, 5) == 0;
            // Mostly this month; sometimes a month earlier, so a second partition.
            let days_back = if pick(rng, 6) == 0 { 40 } else { 0 };
            let target_time = START_BASE_MICROS + pick(rng, 3 * 86_400_000_000) as i64
                - days_back * 86_400_000_000;
            let (key, body): (String, FactBody) = match pick(rng, 4) {
                0 => (
                    format!("tag-{}", pick(rng, 3)),
                    FactBody::Tag(TagBody {
                        op: if coin(rng) { TagOp::Add } else { TagOp::Remove },
                    }),
                ),
                1 => (
                    ["bookmarked", "public"][pick(rng, 2) as usize].to_owned(),
                    FactBody::Flag(FlagBody { value: coin(rng) }),
                ),
                2 => (
                    "quality".to_owned(),
                    FactBody::Score(ScoreBody {
                        data_type: ScoreDataType::Numeric,
                        value_num: pick(rng, 100) as f64 / 100.0,
                        value_str: "".into(),
                        comment: "x".repeat(pick(rng, 40) as usize).into(),
                        source: ScoreSource::Api,
                        metadata: bytes::Bytes::from_static(b"{}"),
                        config_id: None,
                    }),
                ),
                _ => (
                    "output".to_owned(),
                    FactBody::Correction(CorrectionBody {
                        corrected: bytes::Bytes::from(format!("\"fixed-{}\"", pick(rng, 9))),
                        note: "".into(),
                    }),
                ),
            };
            NewFact {
                target_kind: if on_span {
                    FactTargetKind::Span
                } else if in_session {
                    FactTargetKind::Session
                } else {
                    FactTargetKind::Trace
                },
                trace_id: if in_session {
                    TraceId::ZERO
                } else {
                    TraceId::from_bytes(trace)
                },
                span_id: if on_span {
                    SpanId::from_bytes([1 + pick(rng, 2) as u8; 8])
                } else {
                    SpanId::ZERO
                },
                session_id: if in_session {
                    format!("session-{}", pick(rng, 3)).into()
                } else {
                    "".into()
                },
                target_time: Timestamp::from_micros(target_time),
                key: key.into(),
                retracted: pick(rng, 5) == 0,
                body,
            }
        })
        .collect()
}

/// The node's WAL as the recent-span index sees it: the durable LSN, and the re-read of one span by its
/// locator through `Wal::read_at` and stage A, as the binary does it.
#[derive(Debug)]
struct SimWalView {
    wal: Wal,
    ctx: NormCtx<'static>,
}

impl WalView for SimWalView {
    fn durable_lsn(&self) -> Lsn {
        self.wal.durable_lsn()
    }

    fn read_span(&self, project_id: ProjectId, locator: WalLocator) -> SpanRead {
        let (wal, ctx) = (self.wal.clone(), self.ctx);
        Box::pin(async move {
            let failed = |reason: String| WalReadError::Failed(reason);
            let frame = wal
                .read_at(locator.segment, locator.offset)
                .await
                .map_err(|e| failed(e.to_string()))?
                .filter(|frame| frame.record_type() == RecordType::IngestRequest)
                .ok_or(WalReadError::Gone)?;
            let payload = frame.payload().map_err(|e| failed(e.to_string()))?;
            let Ok(Record::IngestRequest(request)) =
                Record::decode(RecordType::IngestRequest, &payload)
            else {
                return Err(WalReadError::Gone);
            };
            let ingest = IngestFrame {
                wal_epoch: wal.epoch(),
                lsn: frame.lsn(),
                received_at: frame.received_at(),
                project_id: request.project_id,
                api_key_id: request.api_key_id,
                compat_dialect: request.compat_dialect,
                sdk_name: request.sdk_name,
                sdk_version: request.sdk_version,
                rejected: request.rejected,
                otlp: request.otlp,
            };
            normalize(&ingest, &ctx)
                .map_err(|e| failed(e.to_string()))?
                .spans
                .into_iter()
                .nth(locator.ordinal as usize)
                .filter(|span| span.core.project_id == project_id)
                .ok_or(WalReadError::Gone)
        })
    }
}

/// The application of one incarnation, as its clients and readers see it.
struct Node {
    router: Router,
    ingest: Ingest,
    query: QueryService,
    index: Arc<RecentIndex>,
    view: Arc<SimWalView>,
}

/// Sends one request body. `true` when the node answered 200 and rejected no span. A refusal for load must be
/// a 429 or a 503 with `Retry-After` (I5); the other refusals are for the body.
async fn post(shared: &Shared, node: &Node, request: &Request) -> bool {
    let content_type = if request.json {
        "application/json"
    } else {
        "application/x-protobuf"
    };
    let built = HttpRequest::post("/v1/traces")
        .header(
            "authorization",
            format!("Bearer {}", secret(request.project)),
        )
        .header("content-type", content_type)
        .header("content-length", request.body.len().to_string())
        .header("user-agent", "crucible-sim/1")
        .body(Body::from(request.body.to_vec()));
    let Ok(built) = built else {
        return false;
    };
    let Ok(response) = node.router.clone().oneshot(built).await;
    let status = response.status();
    if status != StatusCode::OK {
        let load = matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        );
        let body = matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
        );
        let waits = response.headers().contains_key("retry-after");
        let mut ledger = shared.ledger();
        ledger.stats.throttled += u64::from(status == StatusCode::TOO_MANY_REQUESTS);
        let throttle_expected = shared.plan.mask.has(Dim::ProjectRate);
        if !(body || (load && waits))
            || (status == StatusCode::TOO_MANY_REQUESTS && !throttle_expected)
        {
            ledger.violate(Violation::Backpressure(format!(
                "a request was refused with {status}, Retry-After {waits}"
            )));
        }
        return false;
    }
    let Ok(body) = axum::body::to_bytes(response.into_body(), 1 << 20).await else {
        return false;
    };
    if request.json {
        serde_json::from_slice::<serde_json::Value>(&body)
            .is_ok_and(|reply| reply.get("partialSuccess").is_none())
    } else {
        ExportTraceServiceResponse::decode(body.as_ref())
            .is_ok_and(|reply| reply.partial_success.is_none())
    }
}

/// Which markers a read may carry.
#[derive(Clone, Copy, Debug)]
struct Permitted {
    store_unavailable: bool,
    quarantined: bool,
    lagging: bool,
    /// The name of the read in a violation.
    read: &'static str,
}

impl Permitted {
    /// While the workload runs: what the run's configuration can cause.
    fn live(plan: &Plan, read: &'static str) -> Self {
        let has = |dim| plan.mask.has(dim);
        Self {
            store_unavailable: has(Dim::Outages)
                || has(Dim::LongOutages)
                || has(Dim::ChReadErrors)
                || has(Dim::TinyRecent),
            quarantined: has(Dim::ChPoison) || has(Dim::FrameFaults),
            lagging: true,
            read,
        }
    }

    /// At quiescence: the store answers, no gap is open and stage B has caught up.
    const fn quiescent(quarantines: bool, read: &'static str) -> Self {
        Self {
            store_unavailable: false,
            quarantined: quarantines,
            lagging: false,
            read,
        }
    }
}

/// Checks the markers of a response. `true` when the response is whole and strong.
fn check_meta(shared: &Shared, meta: &Meta, permitted: Permitted) -> bool {
    let mut ledger = shared.ledger();
    match meta.partial_reason {
        Some(PartialReason::StoreUnavailable) => {
            ledger.stats.reads_gap += 1;
            if !permitted.store_unavailable {
                ledger.violate(Violation::UnwarrantedMarker("partial: store_unavailable"));
            }
        }
        Some(PartialReason::Quarantined) => {
            ledger.stats.reads_quarantined += 1;
            if !permitted.quarantined {
                ledger.violate(Violation::UnwarrantedMarker("partial: quarantined"));
            }
        }
        Some(PartialReason::Deadline) => {
            ledger.violate(Violation::UnwarrantedMarker("partial: deadline"));
        }
        None => {}
    }
    if meta.consistency == Consistency::Lagging {
        ledger.stats.reads_lagging += 1;
        if !permitted.lagging {
            ledger.violate(Violation::UnwarrantedMarker("consistency: lagging"));
        }
    }
    !meta.partial && meta.consistency == Consistency::Strong
}

/// Handles a read that failed. `true` when a fault of the run explains the failure.
fn check_error(shared: &Shared, error: &QueryServiceError, permitted: Permitted) -> bool {
    let mut ledger = shared.ledger();
    match error {
        QueryServiceError::Store(QueryError::Unavailable(_)) => {
            ledger.stats.reads_unavailable += 1;
            if !permitted.store_unavailable {
                ledger.violate(Violation::UnwarrantedMarker("telemetry_store_unavailable"));
            }
            true
        }
        other => {
            ledger.violate(Violation::ReadFailed(format!(
                "{}: {other}",
                permitted.read
            )));
            false
        }
    }
}

/// I3: reads every trace of an acknowledged request by id and checks that its spans are there, or that the
/// response says why they may not be.
async fn read_back(shared: &Shared, node: &Node, index: usize, permitted: Permitted) {
    let request = shared.ledger().requests[index].clone();
    let Some(ctx) = shared.context(request.project) else {
        return;
    };
    let traces: BTreeSet<[u8; 16]> = request.spans.iter().map(|span| span.trace).collect();
    for trace in traces {
        let result = node
            .query
            .trace_by_id(&ctx, TraceId::from_bytes(trace))
            .await;
        shared.ledger().stats.trace_reads += 1;
        match result {
            Ok(detail) => {
                let returned: BTreeSet<[u8; 8]> = detail
                    .spans
                    .iter()
                    .map(|span| span.summary.span_id.to_bytes())
                    .collect();
                let whole = check_meta(shared, &detail.meta, permitted);
                let located = {
                    let mut ledger = shared.ledger();
                    let foreign = ledger
                        .traces
                        .get(&(request.project, trace))
                        .is_none_or(|sent| !returned.is_subset(sent));
                    if foreign {
                        ledger.violate(Violation::ForeignData("trace"));
                    }
                    let missing = request
                        .spans
                        .iter()
                        .any(|span| span.trace == trace && !returned.contains(&span.span));
                    if missing && whole {
                        ledger.violate(Violation::SilentMiss {
                            read: permitted.read,
                            request: index,
                        });
                    }
                    ledger.stats.live_reads += u64::from(permitted.lagging);
                    if !missing && whole {
                        ledger.stats.reads_whole += 1;
                        ledger.stats.live_whole += u64::from(permitted.lagging);
                    }
                    let mut located = None;
                    for span in &detail.spans {
                        match span.source {
                            SpanSource::Recent(locator) => {
                                ledger.stats.spans_from_index += 1;
                                located.get_or_insert((span.summary.span_id, locator));
                            }
                            SpanSource::Store(_) => ledger.stats.spans_from_store += 1,
                        }
                    }
                    located
                };
                // Contract 8.6: the locator of an index entry leads back to its span. A frame that is gone or
                // does not read is the store's to serve; a frame that reads must hold the span there.
                if let Some((span_id, locator)) = located
                    && let Ok(record) = node
                        .view
                        .read_span(project_id(request.project), locator)
                        .await
                {
                    let mut ledger = shared.ledger();
                    ledger.stats.locator_reads += 1;
                    if record.core.span_id != span_id {
                        ledger.violate(Violation::Locator { request: index });
                    }
                }
                shared.trace.event(
                    "read.trace",
                    &[
                        index as u64,
                        returned.len() as u64,
                        u64::from(detail.meta.partial),
                        u64::from(detail.meta.consistency == Consistency::Lagging),
                    ],
                );
            }
            Err(QueryServiceError::NotFound) => {
                shared.ledger().violate(Violation::SilentMiss {
                    read: "not_found",
                    request: index,
                });
                shared.trace.event("read.trace.not_found", &[index as u64]);
            }
            Err(error) => {
                let explained = check_error(shared, &error, permitted);
                shared
                    .trace
                    .event("read.trace.error", &[index as u64, u64::from(explained)]);
            }
        }
    }
}

/// Pages through the trace list of one project. Every listed trace must be the project's (I6). With `whole`,
/// the list is read to its end and must hold every acknowledged trace that has a root span (I3).
async fn read_list(
    shared: &Shared,
    node: &Node,
    project: usize,
    whole: bool,
    permitted: Permitted,
) {
    let Some(ctx) = shared.context(project) else {
        return;
    };
    let window = TimeWindow::new(
        Timestamp::from_micros(START_BASE_MICROS),
        Timestamp::from_micros(SIM_EPOCH_MICROS),
    );
    let Ok(window) = window else {
        return;
    };
    let mut cursor = None;
    let mut listed: BTreeSet<[u8; 16]> = BTreeSet::new();
    let mut complete = true;
    let pages = if whole { 10_000 } else { 3 };
    for _ in 0..pages {
        let query = TraceListQuery {
            window,
            filter: Filter::all(),
            limit: Some(if whole { 200 } else { 10 }),
            cursor: cursor.take(),
        };
        let page = match node.query.trace_list(&ctx, query).await {
            Ok(page) => page,
            Err(error) => {
                check_error(shared, &error, permitted);
                complete = false;
                break;
            }
        };
        complete &= check_meta(shared, &page.meta, permitted);
        let mut ledger = shared.ledger();
        ledger.stats.list_pages += 1;
        for item in &page.data {
            let trace = item.head.trace_id.to_bytes();
            if !ledger.traces.contains_key(&(project, trace)) {
                ledger.violate(Violation::ForeignData("list"));
            }
            listed.insert(trace);
        }
        drop(ledger);
        shared.trace.event(
            "read.list",
            &[
                project as u64,
                page.data.len() as u64,
                u64::from(page.has_more),
            ],
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if !(whole && complete) {
        return;
    }
    let mut ledger = shared.ledger();
    let missing = ledger.acked.iter().copied().find(|&index| {
        let request = &ledger.requests[index];
        request.project == project
            && request
                .spans
                .iter()
                .any(|span| span.root && !listed.contains(&span.trace))
    });
    if let Some(request) = missing {
        ledger.violate(Violation::SilentMiss {
            read: permitted.read,
            request,
        });
    }
}

#[derive(Clone)]
struct ClientCtx {
    shared: Arc<Shared>,
    node: Arc<Node>,
    env: Env,
    rng: Arc<dyn Rng>,
}

/// Sends the request at `index` and, when it is acknowledged, reads it back.
async fn exchange(ctx: &ClientCtx, index: usize, repeat: bool) -> bool {
    let request = {
        let mut ledger = ctx.shared.ledger();
        ledger.sending(index);
        ledger.requests[index].clone()
    };
    let acked = post(&ctx.shared, &ctx.node, &request).await;
    {
        let mut ledger = ctx.shared.ledger();
        if acked {
            ledger.stats.acked += 1;
            ledger.stats.partial_repeats += u64::from(request.partial_repeat);
            ledger.stats.acked_json += u64::from(request.json);
            ledger.stats.acked_repeats += u64::from(repeat);
            if !ledger.requests[index].acked {
                ledger.requests[index].acked = true;
                ledger.acked.push(index);
            }
        } else {
            ledger.stats.refused += 1;
        }
    }
    ctx.shared
        .trace
        .event("reply", &[index as u64, u64::from(acked)]);
    if acked {
        let permitted = Permitted::live(&ctx.shared.plan, "trace");
        read_back(&ctx.shared, &ctx.node, index, permitted).await;
    }
    acked
}

/// Writes facts to a project through the entry point the fact routes will call, and records what the server
/// assigned once the write is acknowledged.
async fn write_facts(ctx: &ClientCtx, facts: Vec<NewFact>) {
    let project = pick(ctx.rng.as_ref(), ctx.shared.plan.projects as u64) as usize;
    let Ok(scope) = crucible_auth::test_support::scope(project_id(project)) else {
        return;
    };
    let author = FactAuthor {
        kind: AuthorKind::User,
        id: Uuid::from_bytes([0x77; 16]),
    };
    let facts = Arc::new(facts);
    let index = {
        let mut ledger = ctx.shared.ledger();
        ledger.fact_writes.push(FactWrite {
            project,
            facts: Arc::clone(&facts),
            accepted: None,
        });
        ledger.fact_writes.len() - 1
    };
    let result = ctx.node.ingest.append_facts(&scope, author, &facts).await;
    let mut ledger = ctx.shared.ledger();
    let acked = result.is_ok();
    match result {
        Ok(accepted) => {
            ledger.stats.fact_writes += 1;
            ledger.fact_writes[index].accepted = Some(accepted);
        }
        Err(error) => {
            ledger.stats.fact_writes_refused += 1;
            use crucible_ingest::FactWriteError as E;
            if !matches!(error, E::Throttled { .. } | E::Unavailable { .. }) {
                ledger.violate(Violation::Backpressure(format!(
                    "a fact write was refused with: {error}"
                )));
            }
        }
    }
    drop(ledger);
    ctx.shared.trace.event(
        "facts",
        &[index as u64, u64::from(acked), facts.len() as u64],
    );
}

/// One ingest client: sends a bounded number of requests, sometimes several at once and sometimes a repeat of
/// an earlier one, and reads each acknowledged request back.
async fn client(ctx: ClientCtx, budget: u64) {
    let plan = &ctx.shared.plan;
    let rng = ctx.rng.as_ref();
    let mut state = ClientState::default();
    let mut sent = 0;
    let mut refused = 0u32;
    while sent < budget {
        ctx.env
            .clock
            .sleep(Duration::from_micros(pick(rng, 3_000)))
            .await;
        if plan.mask.has(Dim::Facts) && pick(rng, 4) == 0 {
            write_facts(&ctx, generate_facts(rng, &state)).await;
        }
        let burst = if plan.wal.bursts { 1 + pick(rng, 4) } else { 1 };
        let mut waiting = Vec::new();
        for _ in 0..burst {
            sent += 1;
            let earlier = ctx.shared.ledger().requests.len() as u64;
            let repeat = plan.mask.has(Dim::ClientRetries) && earlier > 0 && pick(rng, 6) == 0;
            let index = if repeat {
                ctx.shared.ledger().repeats += 1;
                pick(rng, earlier) as usize
            } else {
                let Some(request) = generate(plan, rng, &mut state) else {
                    continue;
                };
                let mut ledger = ctx.shared.ledger();
                // A span sent again makes the derived tables depend on what each restart remembered.
                ledger.repeats += u64::from(request.partial_repeat);
                ledger.requests.push(request);
                ledger.requests.len() - 1
            };
            waiting.push(exchange(&ctx, index, repeat));
        }
        let acked = futures::future::join_all(waiting).await;
        if acked.contains(&true) {
            refused = 0;
        } else {
            // The node answered 429 or 503 with Retry-After, or refused the body.
            ctx.env
                .clock
                .sleep(Duration::from_millis(5 << refused.min(7)))
                .await;
            refused += 1;
        }
    }
}

/// One reader: trace-by-id of acknowledged requests of any incarnation, and trace lists.
async fn reader(ctx: ClientCtx) {
    let rng = ctx.rng.as_ref();
    loop {
        ctx.env
            .clock
            .sleep(Duration::from_micros(500 + pick(rng, 40_000)))
            .await;
        if coin(rng) {
            let acked = ctx.shared.ledger().acked.clone();
            if acked.is_empty() {
                continue;
            }
            let index = acked[pick(rng, acked.len() as u64) as usize];
            let permitted = Permitted::live(&ctx.shared.plan, "trace");
            read_back(&ctx.shared, &ctx.node, index, permitted).await;
        } else {
            let project = pick(rng, ctx.shared.plan.projects as u64) as usize;
            let permitted = Permitted::live(&ctx.shared.plan, "list");
            read_list(&ctx.shared, &ctx.node, project, false, permitted).await;
        }
    }
}

/// A running shipper: where its task leaves its result, and its counters.
struct Shipping {
    result: Arc<Mutex<Option<Result<(), ShipError>>>>,
    stats: Arc<ShipperStats>,
    barriers: Arc<SimBarriers>,
}

/// The span keys that the `IngestRequest` frames above `watermark` hold.
fn keys_above(
    log: &BTreeMap<u64, WalFrame>,
    watermark: u64,
) -> BTreeSet<([u8; 16], [u8; 16], [u8; 8])> {
    let mut keys = BTreeSet::new();
    for frame in log.values() {
        if frame.lsn().get() <= watermark || frame.record_type() != RecordType::IngestRequest {
            continue;
        }
        let Ok(payload) = frame.payload() else {
            continue;
        };
        let Ok(Record::IngestRequest(request)) =
            Record::decode(RecordType::IngestRequest, &payload)
        else {
            continue;
        };
        let Ok(scanned) = crucible_otlp::TraceRequest::scan(request.otlp, None) else {
            continue;
        };
        for span in scanned.spans() {
            keys.insert((
                request.project_id.to_bytes(),
                span.trace_id().to_bytes(),
                span.span_id().to_bytes(),
            ));
        }
    }
    keys
}

/// The ids of the facts that the `FactBatch` frames above `watermark` hold.
fn facts_above(log: &BTreeMap<u64, WalFrame>, watermark: u64) -> BTreeSet<[u8; 16]> {
    let mut ids = BTreeSet::new();
    for frame in log.values() {
        if frame.lsn().get() <= watermark || frame.record_type() != RecordType::FactBatch {
            continue;
        }
        let Ok(payload) = frame.payload() else {
            continue;
        };
        if let Ok(Record::FactBatch(batch)) = Record::decode(RecordType::FactBatch, &payload) {
            ids.extend(batch.facts.iter().map(|fact| fact.fact_id.to_bytes()));
        }
    }
    ids
}

/// A frame that was quarantined as a frame: its LSN and the payload of its copy.
struct FrameCopy {
    manifest: FrameManifest,
    payload: Vec<u8>,
}

/// The frame quarantines on `disk` whose manifest is whole, and the directories whose copy does not hold the
/// frame its manifest names (contract 5.6).
fn frame_copies(disk: &SimDisk) -> (Vec<FrameCopy>, Vec<String>) {
    let mut copies = Vec::new();
    let mut broken = Vec::new();
    for path in disk.files(Root::Quarantine) {
        let Some(dir) = path.as_str().strip_suffix(&format!("/{MANIFEST_FILE}")) else {
            continue;
        };
        let text = disk.read_file(&path).unwrap_or_default();
        let Some(manifest) = FrameManifest::parse(&String::from_utf8_lossy(&text)) else {
            continue;
        };
        let frames = DiskPath::new(Root::Quarantine, &format!("{dir}/{FRAMES_FILE}"))
            .ok()
            .and_then(|p| disk.read_file(&p))
            .unwrap_or_default();
        let lsn = manifest.lsn.get();
        let payload = frames
            .get(..manifest.bytes as usize)
            .and_then(|bytes| copied_payloads(bytes, lsn, lsn))
            .and_then(|mut payloads| payloads.pop());
        match payload {
            Some(payload) => copies.push(FrameCopy { manifest, payload }),
            None => broken.push(dir.to_owned()),
        }
    }
    (copies, broken)
}

fn holds(payload: &[u8], id: &[u8]) -> bool {
    payload.windows(id.len()).any(|w| w == id)
}

struct Run<'a> {
    sim: &'a Sim,
    disk: SimDisk,
    model: ChModel,
    shared: Arc<Shared>,
    keys: Arc<StaticKeys>,
    rng: Arc<dyn Rng>,
    boots: u64,
    stopped: bool,
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
        self.shared.ledger().violate(violation);
    }

    fn failed(&self) -> bool {
        !self.shared.ledger().violations.is_empty()
    }

    /// Recovers the WAL on a new host through crashes and faults, compares the result with the uninterrupted
    /// reference recovery (I12) and checks the store and the log against the ledger (I1, I2). `None` when a
    /// violation ended the run.
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
            stats.recovery_crashes += counts.crashes;
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
        let watermark = report.watermark_lsn().get();
        self.event("recovery.done", &[report.last_lsn.get(), watermark]);
        match read_log(&wal, report.first_retained_lsn()).await {
            Ok(log) => {
                if LogState::new(&report, &log) != reference {
                    self.violate(Violation::RecoveryDiverged);
                }
                self.check_store(watermark, &log, false);
            }
            Err(e) => self.violate(Violation::LogUnreadable(e)),
        }
        self.disk.set_faults(workload_faults);
        if self.failed() {
            drop(wal);
            host.stop().await;
            return None;
        }
        Some((host, wal, report))
    }

    /// Checks the store against the ledger. Every accepted span of every acknowledged request must be in the
    /// store, in a durable frame above `watermark` or in a quarantine (I1); no row may be there twice (I2).
    /// With `quiescent`, the derived tables are compared too.
    fn check_store(&self, watermark: u64, log: &BTreeMap<u64, WalFrame>, quiescent: bool) {
        let model = &self.model;
        for what in model.violations() {
            self.violate(Violation::Store(what));
        }
        if let Some((first, second)) = model.overlapping_ranges() {
            self.violate(Violation::IdentityOverlap { first, second });
        }
        let (directories, quarantined, faults) = read_quarantines(&self.disk, model, watermark);
        let (frame_copies, broken_copies) = frame_copies(&self.disk);
        let in_log = keys_above(log, watermark);
        let facts_in_log = facts_above(log, watermark);
        let fact_rows = system_oracle::fact_rows(model);
        let mut ledger = self.shared.ledger();
        ledger.stats.quarantines = directories;
        ledger.stats.frame_quarantines = frame_copies.len() as u64;
        // The directories of pairs: a pair that is quarantined in one table and landed in another makes the
        // tables differ until it is requeued.
        let pair_directories = directories
            .saturating_sub(frame_copies.len() as u64)
            .saturating_sub(broken_copies.len() as u64);
        let quarantined_frames: BTreeSet<u64> = frame_copies
            .iter()
            .map(|copy| copy.manifest.lsn.get())
            .collect();
        let mut found: Vec<Violation> = faults
            .into_iter()
            .map(|fault| match fault {
                QuarantineFault::InDoubt { table, batch_seq } => {
                    Violation::QuarantinedInDoubt { table, batch_seq }
                }
                QuarantineFault::Copy(dir) => Violation::QuarantineCopy(dir),
            })
            .chain(broken_copies.into_iter().map(Violation::QuarantineCopy))
            .collect();
        let project_of =
            |id: [u8; 16]| (0..self.plan().projects).find(|&p| project_id(p).to_bytes() == id);
        let mut core_rows = 0u64;
        let mut core_roots: BTreeSet<(u64, [u8; 8])> = BTreeSet::new();
        let mut core_root_keys: BTreeSet<SpanKey> = BTreeSet::new();
        for table in [TableId::SpanCore, TableId::SpanPayload] {
            let name = table.name();
            let mut by_frame: BTreeSet<(u64, [u8; 8])> = BTreeSet::new();
            let mut by_key: BTreeMap<SpanKey, u32> = BTreeMap::new();
            for part in model.parts(table) {
                let block = &part.block;
                for row in 0..block.rows {
                    let cells = (|| {
                        let lsn = u64_at(block, "lsn", row)?;
                        let span: [u8; 8] = bytes_at(block, "span_id", row)?.try_into().ok()?;
                        let trace: [u8; 16] = bytes_at(block, "trace_id", row)?.try_into().ok()?;
                        let project = project_of(uuid_at(block, "project_id", row)?)?;
                        Some((lsn, (project, trace, span)))
                    })();
                    let Some((lsn, key)) = cells else {
                        found.push(Violation::UnknownRow { table: name });
                        continue;
                    };
                    let sent = ledger.keys.get(&key).copied().unwrap_or(0);
                    if sent == 0 {
                        found.push(Violation::UnknownRow { table: name });
                    }
                    if !by_frame.insert((lsn, key.2)) {
                        found.push(Violation::DuplicateRow { table: name, lsn });
                    }
                    if quarantined_frames.contains(&lsn) {
                        found.push(Violation::Store(format!(
                            "frame {lsn} is quarantined as a frame and has rows in {name}"
                        )));
                    }
                    // I7 at the core tier: tokens, cost and the error status are what the span reported.
                    let reported = ledger.sent.get(&key);
                    if table == TableId::SpanCore
                        && reported
                            .is_some_and(|sent| !system_oracle::span_matches(block, row, sent))
                    {
                        found.push(Violation::Aggregate(format!(
                            "the span_core row of a span of frame {lsn} differs from what the span reported: \
                             {reported:?}"
                        )));
                    }
                    let rows = by_key.entry(key).or_default();
                    *rows += 1;
                    if *rows > sent && sent > 0 {
                        found.push(Violation::TooManyRows { table: name });
                    }
                    if table == TableId::SpanCore {
                        core_rows += 1;
                        if u8_at(block, "is_root", row) == Some(1) {
                            core_roots.insert((lsn, key.2));
                            core_root_keys.insert(key);
                        }
                    }
                }
            }
            for &index in &ledger.acked {
                let request = &ledger.requests[index];
                for span in request.spans.iter() {
                    let key = (request.project, span.trace, span.span);
                    let durable = in_log.contains(&(
                        project_id(request.project).to_bytes(),
                        span.trace,
                        span.span,
                    ));
                    let parked = || {
                        quarantined
                            .iter()
                            .any(|q| q.table == table && q.holds(&span.trace, &span.span))
                            || frame_copies.iter().any(|copy| {
                                !copy.manifest.facts
                                    && holds(&copy.payload, &span.trace)
                                    && holds(&copy.payload, &span.span)
                            })
                    };
                    if !by_key.contains_key(&key) && !durable && !parked() {
                        found.push(Violation::AckedSpanLost {
                            table: name,
                            request: index,
                        });
                    }
                }
            }
        }

        // Facts. I2: no fact row twice, and every row is a fact that was written. I1: every acknowledged fact
        // is in its table, in a durable frame above the watermark, or in a quarantine. I4: the rows resolve to
        // what the reference fold gives for the facts as they were written, whatever the order of the rows.
        match fact_rows {
            None => found.push(Violation::FactRow("a fact row does not read".to_owned())),
            Some(rows) => {
                // Every fact the ledger knows by id, as the row it must be.
                let mut written: BTreeMap<[u8; 16], (usize, FactRow)> = BTreeMap::new();
                for (write, entry) in ledger.fact_writes.iter().enumerate() {
                    let Some(accepted) = &entry.accepted else {
                        continue;
                    };
                    for (fact, assigned) in entry.facts.iter().zip(accepted) {
                        let row = FactRow {
                            table: fact.body.table(),
                            project: project_id(entry.project).to_bytes(),
                            fact_id: assigned.fact_id.to_bytes(),
                            version: assigned.version,
                            retracted: fact.retracted,
                            key: fact.key.as_bytes().to_vec(),
                            trace: fact.trace_id.to_bytes(),
                            span: fact.span_id.to_bytes(),
                            session: fact.session_id.as_bytes().to_vec(),
                        };
                        written.insert(row.fact_id, (write, row));
                    }
                }
                // A write that was not acknowledged may be durable all the same: its rows carry ids the
                // ledger never learned.
                let unacked = |row: &FactRow| {
                    ledger.fact_writes.iter().any(|entry| {
                        entry.accepted.is_none()
                            && project_id(entry.project).to_bytes() == row.project
                            && entry.facts.iter().any(|fact| {
                                fact.body.table() == row.table && fact.key.as_bytes() == row.key
                            })
                    })
                };
                let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
                let mut expected: Vec<FactRow> = Vec::with_capacity(rows.len());
                for row in &rows {
                    if !seen.insert(row.fact_id) {
                        found.push(Violation::FactRow(format!(
                            "a fact landed twice in {}",
                            row.table.name()
                        )));
                    }
                    match written.get(&row.fact_id) {
                        Some((_, sent)) => expected.push(sent.clone()),
                        None if unacked(row) => expected.push(row.clone()),
                        None => found.push(Violation::FactRow(format!(
                            "a row of {} that no fact write explains",
                            row.table.name()
                        ))),
                    }
                }
                for (id, (write, row)) in &written {
                    let parked = || {
                        quarantined
                            .iter()
                            .any(|q| q.table == row.table && q.holds_id(id))
                            || frame_copies
                                .iter()
                                .any(|copy| copy.manifest.facts && holds(&copy.payload, id))
                    };
                    if !seen.contains(id) && !facts_in_log.contains(id) && !parked() {
                        found.push(Violation::AckedFactLost {
                            table: row.table.name(),
                            write: *write,
                        });
                    }
                }
                let resolved = system_oracle::resolve(&rows);
                if resolved != system_oracle::resolve(&expected) {
                    found.push(Violation::FactResolution(
                        "the fact rows do not resolve to what the facts written resolve to"
                            .to_owned(),
                    ));
                }
                if resolved != system_oracle::resolve(rows.iter().rev()) {
                    found.push(Violation::FactResolution(
                        "the resolution depends on the order of the rows".to_owned(),
                    ));
                }
                ledger.stats.fact_rows = rows.len() as u64;
                ledger.stats.fact_keys = resolved.len() as u64;
            }
        }

        // I7: once everything has landed, every rollup equals the reference aggregation over `span_core`.
        if quiescent && pair_directories == 0 {
            let (keys, wrong) = system_oracle::check_aggregates(model);
            ledger.stats.rollup_keys = keys;
            found.extend(wrong.into_iter().map(Violation::Aggregate));
        }

        if quiescent {
            let mut heads: BTreeSet<(u64, [u8; 8], i8)> = BTreeSet::new();
            for part in model.parts(TableId::TraceHeads) {
                let block = &part.block;
                for row in 0..block.rows {
                    let lsn = u64_at(block, "lsn", row).unwrap_or(0);
                    let span: [u8; 8] = bytes_at(block, "span_id", row)
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or_default();
                    let kind = i8_at(block, "head_kind", row).unwrap_or(0);
                    if !heads.insert((lsn, span, kind)) {
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
            let exact = ledger.repeats == 0 && directories == 0;
            if exact {
                for &(lsn, span) in &core_roots {
                    if !heads.contains(&(lsn, span, 1)) {
                        found.push(Violation::DerivedMismatch(format!(
                            "the root span of frame {lsn} has no head row"
                        )));
                    }
                }
                for &index in &ledger.acked {
                    let request = &ledger.requests[index];
                    for span in request.spans.iter().filter(|span| span.root) {
                        if !core_root_keys.contains(&(request.project, span.trace, span.span)) {
                            found.push(Violation::DerivedMismatch(format!(
                                "a root span of request {index} is not a root in span_core"
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

    /// Starts the application of this incarnation: the recent-span index rebuilt at the recovered watermark,
    /// the shipper with the index behind its hook, the ingest router and the query service. `None` when the
    /// shipper refuses to start, which is a violation.
    fn start(
        &self,
        host: &Host,
        wal: &Wal,
        report: &RecoveryReport,
    ) -> Option<(Arc<Node>, Shipping)> {
        let plan = self.plan();
        let watermark = report.watermark_lsn();
        let prices = match PriceTable::bundled() {
            Ok(prices) => prices,
            Err(e) => {
                self.violate(Violation::ShipperFailed(e.to_string()));
                return None;
            }
        };
        let view = Arc::new(SimWalView {
            wal: wal.clone(),
            ctx: NormCtx::new(prices, PayloadCaps::DEFAULT),
        });
        let (index, mut writer) = RecentIndex::rebuild(
            plan.recent,
            host.env.clock.clone(),
            view.clone(),
            watermark,
            std::iter::empty(),
        );
        if plan.mutant == Some(Mutant::SilentEviction) {
            writer.drop_without_gap();
        }
        let hook = Arc::new(RecentHook::new(writer, host.env.clock.clone(), watermark));
        // The `Quarantine` frames at or below the watermark may be gone with their segments; the manifests
        // under the quarantine root still name the pairs.
        hook.restore_quarantined(
            manifests(&self.disk)
                .into_iter()
                .filter_map(|(_, parsed)| parsed)
                .filter(|(q, _)| q.last <= watermark.get())
                .map(|(q, _)| (Lsn::new(q.first), q.table, q.partition)),
        );
        // The same for the frames that were quarantined as frames; the ones the log still holds are marked
        // again when stage B replays them.
        hook.restore_unshippable(
            frame_copies(&self.disk)
                .0
                .into_iter()
                .map(|copy| copy.manifest)
                .filter(|manifest| manifest.lsn <= watermark)
                .map(|manifest| (manifest.lsn, manifest.project_id, manifest.facts)),
        );
        let barriers = Arc::new(SimBarriers::new(&self.disk, self.sim.trace()));
        let hooks = Hooks {
            barriers: barriers.clone(),
            recent: hook,
        };
        let stores = Stores::new(Arc::new(self.model.clone()));
        let shipper = match Shipper::new(
            &host.env,
            &stores,
            wal.clone(),
            report,
            plan.ship.clone(),
            hooks,
        ) {
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
        let ingest = Ingest::new(
            &host.env,
            wal.clone(),
            &plan.wal.config,
            self.keys.clone(),
            plan.ingest.clone(),
        );
        let node = Arc::new(Node {
            router: ingest.router(),
            ingest,
            query: QueryService::new(
                stores,
                host.env.clock.clone(),
                Arc::clone(&index),
                wal.epoch(),
            ),
            index,
            view,
        });
        Some((
            node,
            Shipping {
                result,
                stats,
                barriers,
            },
        ))
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
        let snapshot = shipping.stats.snapshot();
        add(&mut ledger.stats.shipper, &snapshot);
        // I5: the rows the shipper holds stay within `shipper.encoded_bytes`, give or take the one frame
        // that is never split.
        let cap = self.plan().ship.encoded_bytes;
        if snapshot.encoded_bytes_peak > cap.saturating_add(snapshot.largest_frame_bytes) {
            ledger.violate(Violation::Backpressure(format!(
                "the shipper held {} bytes of rows under a budget of {cap} with frames of at most {}",
                snapshot.encoded_bytes_peak, snapshot.largest_frame_bytes
            )));
        }
        match ended {
            None | Some(Ok(())) => {}
            Some(Err(ShipError::WalFailed)) if !healed => {}
            Some(Err(e)) => ledger.violate(Violation::ShipperFailed(e.to_string())),
        }
    }

    async fn crash(&self, host: Host, wal: Wal) {
        drop(wal);
        self.event("crash", &[self.disk.ops()]);
        self.shared.ledger().stats.crashes += 1;
        host.crash().await;
    }

    /// One incarnation: recover, serve clients and readers and ship until a seeded crash point, crash.
    async fn incarnation(&mut self) -> Option<()> {
        let (host, wal, report) = self.boot().await?;
        self.shared.ledger().stats.incarnations += 1;
        let rng = Arc::clone(&self.rng);
        let rng = rng.as_ref();
        let has = |dim| self.plan().mask.has(dim);
        if self.plan().wal.wall_steps {
            host.clock.step_wall(pick(rng, 400_000) as i64 - 200_000);
        }
        let (node, shipping) = self.start(&host, &wal, &report)?;
        let task = |label: String| ClientCtx {
            shared: Arc::clone(&self.shared),
            node: Arc::clone(&node),
            env: host.env.clone(),
            rng: self.sim.rng(&label),
        };
        for index in 0..self.plan().wal.clients {
            let ctx = task(format!("system-sim/client-{}-{index}", self.boots));
            let budget = 3 + pick(rng, 22);
            drop(host.env.spawn("client", client(ctx, budget)));
        }
        if has(Dim::ConcurrentReads) {
            for index in 0..1 + pick(rng, 2) {
                let ctx = task(format!("system-sim/reader-{}-{index}", self.boots));
                drop(host.env.spawn("reader", reader(ctx)));
            }
        }
        drop(node);

        // The crash point: a seeded number of disk operations or of crash barriers from now, or a seeded
        // virtual time, whichever comes first. A crash by time aborts every task at its await point.
        let short = has(Dim::CrashLoops) && pick(rng, 4) > 0;
        let (ops, barriers, micros) = if short {
            (12, 4, 4_000)
        } else {
            match pick(rng, 5) {
                0 | 1 => (400, 12, 250_000),
                2 | 3 => (4_000, 60, 5_000_000),
                _ => (20_000, 200, 60_000_000),
            }
        };
        if has(Dim::DiskCrashPoints) && coin(rng) {
            self.disk.arm_crash_after_ops(1 + pick(rng, ops));
        }
        if has(Dim::BarrierCrashes) && coin(rng) {
            if coin(rng) {
                shipping.barriers.arm(None, 1 + pick(rng, barriers));
            } else if has(Dim::ChPoison) && coin(rng) {
                // The rarest barrier: it needs a pair that is being quarantined.
                shipping.barriers.arm(Some(QUARANTINE_COPIED), 1);
            } else {
                let only = pick(rng, BARRIERS.len() as u64);
                shipping.barriers.arm(Some(only), 1 + pick(rng, 3));
            }
        }
        let run_for = 500 + pick(rng, micros);
        let long = has(Dim::LongOutages) && coin(rng);
        let outage = (long || (has(Dim::Outages) && coin(rng))).then(|| {
            let length = if long {
                30_000_000
                    + pick(
                        rng,
                        if self.plan().stress {
                            570_000_000
                        } else {
                            270_000_000
                        },
                    )
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
                        self.shared.ledger().stats.crashes_at[at] += 1;
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

    /// Quiescence: heal every fault, drain the log, check the store, the WAL, the index and every read, restart
    /// once more and compare.
    async fn quiesce(&mut self) -> Option<()> {
        self.disk.set_faults(DiskFaults::default());
        self.model.set_faults(ChFaults::default());
        self.model.end_outage();
        let (host, wal, report) = self.boot().await?;
        let first_retained = report.first_retained_lsn();
        // The last frame that warrants a seal: the shipper's own control frames after it stay unsealed.
        let target = match read_log(&wal, first_retained).await {
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
        let (node, shipping) = self.start(&host, &wal, &report)?;
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
        // Past replay retention, however the wall clock was stepped, past the visibility grace and the last
        // garbage collection, and past the last insert in flight.
        let retention = self.plan().wal.config.replay_retention;
        let grace = self.plan().recent.visibility_grace;
        clock
            .sleep(retention + grace + Duration::from_secs(10))
            .await;
        let in_flight = self.model.in_flight_until();
        let elapsed = self.sim.elapsed().as_micros() as u64;
        clock
            .sleep(Duration::from_micros(in_flight.saturating_sub(elapsed) + 1))
            .await;
        clock.sleep(grace + Duration::from_secs(2)).await;
        let segments = wal.segments();
        if let [oldest, next, ..] = segments.as_slice()
            && next.first_lsn.get().saturating_sub(1) <= watermark(&wal)
        {
            self.violate(Violation::SpaceNotReclaimed {
                segment_no: oldest.segment_no,
            });
        }
        let entries = node.index.stats().entries;
        if entries > 0 && watermark(&wal) >= target {
            self.violate(Violation::IndexNotDrained { entries });
        }
        self.account(&shipping, true);
        match read_log(
            &wal,
            wal.segments()
                .first()
                .map_or(first_retained, |s| s.first_lsn),
        )
        .await
        {
            Ok(log) => self.check_store(watermark(&wal), &log, true),
            Err(e) => self.violate(Violation::LogUnreadable(e)),
        }
        self.read_everything(&node, "quiescent").await;
        drop(node);

        // I12: a restart of a drained node ships nothing and changes nothing. I3 holds after it too.
        let before = (
            self.model.digest(),
            self.model.stats().inserts,
            watermark(&wal),
        );
        self.crash(host, wal).await;
        let (host, wal, report) = self.boot().await?;
        let (node, shipping) = self.start(&host, &wal, &report)?;
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
        self.read_everything(&node, "after the last restart").await;
        lists::check(&self.shared, &node, &self.model).await;
        drop(node);
        drop(wal);
        host.stop().await;
        Some(())
    }

    /// Reads every acknowledged request back by trace id, and the whole trace list of every project, with no
    /// marker accepted except `quarantined` when a quarantine exists.
    async fn read_everything(&self, node: &Node, read: &'static str) {
        let (acked, quarantines) = {
            let ledger = self.shared.ledger();
            (ledger.acked.clone(), ledger.stats.quarantines > 0)
        };
        let permitted = Permitted::quiescent(quarantines, read);
        for index in acked {
            read_back(&self.shared, node, index, permitted).await;
        }
        for project in 0..self.plan().projects {
            read_list(&self.shared, node, project, true, permitted).await;
        }
    }
}

/// The virtual time one run may take.
const DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let plan_rng = sim.rng("system-sim/plan");
    let plan = Plan::draw(plan_rng.as_ref(), &options);
    let fsync_lie = plan.fsync_lie;
    let disk = sim.disk("system-sim/disk");
    disk.set_faults(plan.wal.faults.clone());
    let model = ChModel::new(
        sim.rng("system-sim/store").as_ref(),
        plan.ship.flavour,
        plan.ship.granularity,
        plan.ship.probe_valid_for,
    );
    model.set_faults(plan.faults.clone());
    let keys = (0..plan.projects)
        .filter_map(|project| {
            Some(StaticKey {
                id: ApiKeyId::from_bytes([project as u8 + 0x40; 16]),
                project_id: project_id(project),
                public_key: PublicKey::parse(&secret(project).replace("csk_", "cpk_")).ok()?,
                secret: SecretKey::parse(&secret(project)).ok()?,
            })
        })
        .collect();
    let incarnations = plan.wal.incarnations;
    let shared = Arc::new(Shared {
        ledger: Mutex::default(),
        plan,
        trace: Arc::clone(sim.trace()),
    });
    let finished = match StaticKeys::new(sim.rng("system-sim/keys").as_ref(), keys) {
        Ok(keys) => {
            let mut run = Run {
                sim: &sim,
                disk,
                model: model.clone(),
                shared: Arc::clone(&shared),
                keys: Arc::new(keys),
                rng: sim.rng("system-sim/driver"),
                boots: 0,
                stopped: false,
            };
            let body = async {
                for _ in 0..incarnations {
                    run.incarnation().await?;
                }
                run.quiesce().await
            };
            tokio::time::timeout(DEADLINE, Box::pin(body)).await.is_ok()
        }
        Err(_) => false,
    };
    let mut ledger = shared.ledger();
    if !finished {
        ledger.violate(Violation::Deadline);
    }
    ledger.stats.store = model.stats();
    ledger.stats.virtual_micros = sim.elapsed().as_micros() as u64;
    Outcome {
        seed: sim.seed(),
        options,
        trace_hash: sim.trace().hash(),
        violations: ledger.violations.clone(),
        fsync_lie,
        stats: ledger.stats,
    }
}

/// Runs the system simulation for one seed. The whole run, its faults and its crash points are a function of
/// `seed` and `options`.
pub fn run_system_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
