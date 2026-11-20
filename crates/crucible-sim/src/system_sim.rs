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
//!   and the duration histograms. No row of `span_core`, `span_payload`, `mcp_ops` or `mcp_content` holds a
//!   value that a span carried in an event under a key that must never be stored.
//! - **I8** every `mcp_ops` row carries the status, the error class, the subclass, the JSON-RPC code and the
//!   caller-fault flag that the shared classifier gives for the exchange the client simulated, and the other
//!   fields of the wire contract as the exchange had them. At quiescence `mcp_tool_rollup`, `mcp_deploys` and
//!   `mcp_tool_inventory` equal the reference over `mcp_ops` per capture source (I7), and `mcp_ops` and
//!   `mcp_content` hold one row per operation (I2).
//! - **I9** with `deletions`, one more project gets traffic and deletion requests: by trace, by user (with and
//!   without a token issuer) and by session, their tombstones appended to the WAL as the node timer appends them,
//!   and requests that complete while the node is down, which leave no tombstone. No row that a completed
//!   request selects lands after the request completed, and every span, MCP operation and fact of that project
//!   that no request selects is still there (I1 for that project).
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
use crucible_normalize::{
    Admission, IngestFrame, NormCtx, NormalizedFrame, PriceTable, ProjectSalt, Tombstone,
    TombstoneSelector, normalize,
};
use crucible_plan::{Caller, Filter};
use crucible_query::{
    Consistency, Meta, PartialReason, QueryService, QueryServiceError, ReadContext, SpanSource,
    TraceListQuery,
};
use crucible_recent::{RecentConfig, RecentIndex, SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{
    ApiKeyId, AuthorKind, CancelSource, CaptureSource, CorrectionBody, DeletionRequestId,
    DeletionState, Era, ErrorClass, ErrorSubclass, FactBody, FactTargetKind, FlagBody, Lsn,
    McpErrorAnnotations, McpErrorInput, McpSide, McpStatus, PayloadCaps, ProjectId, ScoreBody,
    ScoreDataType, ScoreSource, SpanId, TableId, TagBody, TagOp, TimeWindow, Timestamp, TraceId,
    Transport, Uuid, classify_mcp_error,
};
use crucible_schema::{Granularity, RetentionSettings};
use crucible_shipper::{
    DeletionRequests, DeletionsUnavailable, FRAMES_FILE, FrameFault, FrameManifest, Hooks,
    KnownDeletion, MANIFEST_FILE, NoDeletions, NoSalts, ProjectSalts, RecentHook,
    RecentIndex as StageBHook, SaltUnavailable, ShipError, Shipper, ShipperConfig, ShipperStats,
    StatsSnapshot,
};
use crucible_wal::{RecoveryReport, Wal, WalConfig, WalFrame};
use crucible_wal_codec::{Record, RecordType};
use futures::FutureExt;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, KeyValueList, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::Event;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::boot::{BootCounts, Booted, Recovery, recover};
use crate::ch_model::{
    ChFaults, ChModel, ChStats, bytes_at, i8_at, i64_at, u8_at, u64_at, uuid_at,
};
use crate::disk::{DiskFaults, SimDisk};
use crate::runtime::{Host, SIM_EPOCH_MICROS, Sim, run};
use crate::ship_sim::{
    BARRIERS, QUARANTINE_COPIED, QuarantineFault, SimBarriers, add, copied_payloads, manifests,
    random, read_quarantines,
};
use crate::swarm::{ConfigMask, Dim, RunRecord};
use crate::system_oracle::{self, Admitted, FactRow, FrameSpan, McpKey, McpSent};
use crate::trace::Trace;
use crate::wal_sim::{self, LogState, coin, pick, read_log};

mod lists;
mod reads;

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
    /// `crucible-shipper` counts the tokens and cost of a usage aggregate, a span with a descendant that carries
    /// usage or cost.
    CountAggregateUsage,
    /// `crucible-shipper` collects the segments that the admission of an unchecked frame relied on.
    CollectWhatFramesRelyOn,
    /// `crucible-normalize` reports an invalid-params error on a server-side MCP row as the server's fault.
    MisclassifyCallerFault,
    /// `crucible-shipper` gives every `mcp_tool_rollup` partial one capture source, so that rows of different
    /// capture sources are summed.
    SumCaptureSources,
    /// `crucible-shipper` sends the blocks of `mcp_tool_rollup` without their marker row.
    DropRollupMarker,
    /// `crucible-normalize` flags the usage aggregates of a frame after its exact repeats were dropped.
    MarkAfterDuplicateDrop,
    /// `crucible-shipper` applies no deletion: neither a tombstone of the log nor a request read before
    /// shipping drops anything.
    SkipTombstoneFilter,
    /// `crucible-normalize` matches the subject hash of an MCP operation under its own issuer for a deletion by
    /// user that names another issuer.
    IgnoreTombstoneIssuer,
    /// `crucible-shipper` applies, before shipping, only the deletion requests that are still open.
    SeedOnlyOpenRequests,
    /// `crucible-normalize` lets a kept span flagged a usage aggregate on an earlier frame's support rely on
    /// nothing for its flag.
    SkipFlagReliance,
}

impl Mutant {
    /// Every mutant.
    pub const ALL: [Self; 19] = [
        Self::SkipWalSync,
        Self::DropToken,
        Self::GcAboveCheckpoint,
        Self::SilentEviction,
        Self::SkipProbeAfterUnknown,
        Self::AckBeforeAppend,
        Self::DoubleCountRollup,
        Self::DropFactOnRetry,
        Self::FrameQuarantineWithoutCopy,
        Self::CountAggregateUsage,
        Self::CollectWhatFramesRelyOn,
        Self::MisclassifyCallerFault,
        Self::SumCaptureSources,
        Self::DropRollupMarker,
        Self::MarkAfterDuplicateDrop,
        Self::SkipTombstoneFilter,
        Self::IgnoreTombstoneIssuer,
        Self::SeedOnlyOpenRequests,
        Self::SkipFlagReliance,
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
            Self::CountAggregateUsage => "count-aggregate-usage",
            Self::CollectWhatFramesRelyOn => "collect-what-frames-rely-on",
            Self::MisclassifyCallerFault => "misclassify-caller-fault",
            Self::SumCaptureSources => "sum-capture-sources",
            Self::DropRollupMarker => "drop-rollup-marker",
            Self::MarkAfterDuplicateDrop => "mark-after-duplicate-drop",
            Self::SkipTombstoneFilter => "skip-tombstone-filter",
            Self::IgnoreTombstoneIssuer => "ignore-tombstone-issuer",
            Self::SeedOnlyOpenRequests => "seed-only-open-requests",
            Self::SkipFlagReliance => "skip-flag-reliance",
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
    /// I8: an `mcp_ops` row is not classified as the shared classifier classifies its exchange.
    Classification(String),
    /// A stored row holds a value that the generator planted under a key that must never be stored (contracts
    /// 3.5, 9.3 and 9.4). The catalogue has no invariant of its own for it; it is reported under I7, beside the
    /// other checks that a row carries what the wire contract says of its span.
    NeverStored(String),
    /// I9: a row that a completed deletion request selects landed after the request completed, or a span, an
    /// MCP operation or a fact of the deleted project that no request selects is in no table, frame or
    /// quarantine.
    Deletion(String),
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
            Self::Aggregate(_) | Self::NeverStored(_) => "I7",
            Self::Classification(_) => "I8",
            Self::Deletion(_) => "I9",
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
    /// `mcp_ops` rows compared with the exchange their operation simulated.
    pub mcp_ops: u64,
    /// Of those, rows whose exchange failed in some error class.
    pub mcp_errors: u64,
    /// Of those, rows that are the caller's fault.
    pub mcp_caller_faults: u64,
    /// Of those, rows of an operation that another capture source reported too.
    pub mcp_proxy_rows: u64,
    /// Of those, rows with a hashed user subject.
    pub mcp_subjects: u64,
    /// Keys of the MCP aggregating tables compared with the reference at the end.
    pub mcp_keys: u64,
    /// Reads of a project salt that the simulated control store did not answer.
    pub salt_waits: u64,
    /// Windows in which directory syncs failed.
    pub dir_sync_windows: u64,
    /// Deletion requests made, those that completed while the node was down included.
    pub deletions: u64,
    /// Of those, requests that completed while the node was down, which left no tombstone in its log.
    pub deletions_while_down: u64,
    /// Tombstones appended to the WAL.
    pub tombstones: u64,
    /// Deletion requests that completed once the checkpoint passed their tombstone.
    pub deletions_completed: u64,
    /// Spans that stage B dropped because a deletion selects them, over every admission.
    pub spans_deleted: u64,
    /// MCP operations that stage B dropped because a deletion by user names them by their own identity.
    pub ops_deleted: u64,
    /// Reads of the deletion requests that the simulated control store did not answer.
    pub deletion_waits: u64,
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
            (self.mcp_ops, "mcp operation compared with its exchange"),
            (self.mcp_errors, "mcp operation classified as an error"),
            (self.mcp_caller_faults, "mcp caller fault"),
            (self.mcp_proxy_rows, "mcp operation captured by a proxy"),
            (self.mcp_subjects, "mcp user subject hashed"),
            (self.mcp_keys, "mcp aggregates compared with the reference"),
            (self.salt_waits, "frame waited for its project salt"),
            (self.dir_sync_windows, "directory syncs failed"),
            (self.tombstones, "tombstone appended"),
            (
                self.deletions_while_down,
                "deletion completed while the node was down",
            ),
            (self.deletions_completed, "deletion completed"),
            (self.spans_deleted, "span dropped by a deletion"),
            (
                self.ops_deleted,
                "mcp operation dropped by its user identity",
            ),
            (self.deletion_waits, "read of the deletion requests failed"),
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
    /// The share of project salt reads that fail; `None` for a node without a control store.
    salt_fail_ppm: Option<u32>,
    /// With `deletions`, the share of reads of the deletion requests that fail.
    deletion_fail_ppm: Option<u32>,
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
                Some(Mutant::CountAggregateUsage) => {
                    Some(crucible_shipper::Mutant::CountAggregateUsage)
                }
                Some(Mutant::CollectWhatFramesRelyOn) => {
                    Some(crucible_shipper::Mutant::CollectWhatFramesRelyOn)
                }
                Some(Mutant::MisclassifyCallerFault) => {
                    Some(crucible_shipper::Mutant::MisclassifyCallerFault)
                }
                Some(Mutant::SumCaptureSources) => {
                    Some(crucible_shipper::Mutant::SumCaptureSources)
                }
                Some(Mutant::DropRollupMarker) => Some(crucible_shipper::Mutant::DropRollupMarker),
                Some(Mutant::MarkAfterDuplicateDrop) => {
                    Some(crucible_shipper::Mutant::MarkAfterDuplicateDrop)
                }
                Some(Mutant::SkipTombstoneFilter) => {
                    Some(crucible_shipper::Mutant::SkipTombstoneFilter)
                }
                Some(Mutant::IgnoreTombstoneIssuer) => {
                    Some(crucible_shipper::Mutant::IgnoreTombstoneIssuer)
                }
                Some(Mutant::SeedOnlyOpenRequests) => {
                    Some(crucible_shipper::Mutant::SeedOnlyOpenRequests)
                }
                Some(Mutant::SkipFlagReliance) => Some(crucible_shipper::Mutant::SkipFlagReliance),
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
        let mut plan = Self {
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
            // With an identity the node has a control store, whose reads sometimes fail; without one it runs
            // in ingest-only mode and drops the identity keys.
            salt_fail_ppm: has(Dim::McpIdentity).then(|| choose(&[0, 150_000]) as u32),
            deletion_fail_ppm: None,
        };
        // Drawn last, so that a mask without the dimension draws what it drew before. Deletion requests live in
        // a control store, which also holds the project salts.
        if has(Dim::Deletions) {
            plan.deletion_fail_ppm = Some(choose(&[0, 150_000]) as u32);
            plan.salt_fail_ppm = plan.salt_fail_ppm.or(Some(0));
        }
        plan
    }

    /// The project whose data the run deletes: with `deletions`, the one after the workload's projects.
    const fn deleted(&self) -> Option<usize> {
        match self.deletion_fail_ppm {
            Some(_) => Some(self.projects),
            None => None,
        }
    }

    /// Every project that gets traffic, the deleted one included.
    const fn all_projects(&self) -> usize {
        self.projects + self.deletion_fail_ppm.is_some() as usize
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

/// The MCP operations of a request: (trace, span) of each and what the oracle expects of it.
type SentOps = Vec<([u8; 16], [u8; 8], McpSent)>;

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
    /// Whether the request that carried it was acknowledged, so that stage B has seen it.
    acked: bool,
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
    /// The MCP operations of the request: (trace, span) and what the oracle expects of each.
    mcp: Arc<SentOps>,
}

/// A deletion request of the simulated control store.
#[derive(Clone, Debug)]
struct SimDeletion {
    /// What it deletes and where it stands, as the shipper reads it.
    known: KnownDeletion,
    /// The LSN of its durable `Tombstone` frame in the node's log; `None` while it has none.
    tombstone_lsn: Option<u64>,
    /// The virtual microsecond from which the deletion executor has deleted what the request selects: once the
    /// node's checkpoint passed the tombstone, or at once while the node is down (after the inserts it left in
    /// flight). A row the request selects that lands later stays, which I9 forbids.
    completed_at: Option<u64>,
}

/// A span as I9's model reads it: its trace, span id, `user_id` and `session_id` as stage A made them.
type DeletableSpan = ([u8; 16], [u8; 8], Box<str>, Box<str>);

/// One admitted frame of the deleted project, as I9's model reads it: when it arrived, and each span stage A made
/// of it.
#[derive(Clone, Debug)]
struct FrameSpans {
    received_at: i64,
    spans: Vec<DeletableSpan>,
}

/// The token issuers of the simulated MCP identities.
const ISSUERS: [&str; 2] = ["https://issuer-0.example", "https://issuer-1.example"];

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
    /// Every MCP operation sent, as the exchange it simulated.
    mcp: BTreeMap<McpKey, McpSent>,
    /// What stage B was given: every span of every frame it admitted, and where the WAL started then.
    admitted: Admitted,
    /// The deletion requests of the simulated control store, in the order they were made.
    deletions: Vec<SimDeletion>,
    /// Each admitted `IngestRequest` frame of the deleted project, by LSN.
    deletable: BTreeMap<u64, FrameSpans>,
    /// The frames of `deletable` that hold each (trace, span id).
    copies: BTreeMap<([u8; 16], [u8; 8]), BTreeSet<u64>>,
    /// When each fact of the deleted project arrived, as stage B passed its frame.
    fact_arrivals: BTreeMap<[u8; 16], i64>,
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
        let project = project_id(request.project).to_bytes();
        for (trace, span, sent) in request.mcp.iter() {
            self.mcp.insert((project, *trace, *span), sent.clone());
        }
        self.stats.sent += 1;
    }

    /// Whether a deletion selects a copy of the span `key`: a frame that holds it, arrived before the deletion
    /// and is of the deleted project, the only one that has deletions.
    fn deleted_span(&self, key: &SpanKey) -> bool {
        let project = project_id(key.0).to_bytes();
        self.copies.get(&(key.1, key.2)).is_some_and(|lsns| {
            lsns.iter().any(|lsn| {
                let Some(frame) = self.deletable.get(lsn) else {
                    return false;
                };
                frame
                    .spans
                    .iter()
                    .filter(|(trace, span, ..)| *trace == key.1 && *span == key.2)
                    .any(|(trace, _, user, session)| {
                        self.deletions.iter().any(|d| {
                            system_oracle::deletion_selects_span(
                                &d.known.tombstone,
                                &project,
                                frame.received_at,
                                trace,
                                user,
                                session,
                            )
                        })
                    })
            })
        })
    }

    /// Whether a deletion selects a copy of the MCP operation `key`: with its span, or by its own identity.
    fn deleted_op(&self, key: &SpanKey) -> bool {
        let project = project_id(key.0).to_bytes();
        let Some(op) = self.mcp.get(&(project, key.1, key.2)) else {
            return false;
        };
        self.deleted_span(key)
            || self.copies.get(&(key.1, key.2)).is_some_and(|lsns| {
                lsns.iter()
                    .filter_map(|lsn| self.deletable.get(lsn))
                    .any(|frame| {
                        self.deletions.iter().any(|d| {
                            system_oracle::deletion_names_op(
                                &d.known.tombstone,
                                &project,
                                frame.received_at,
                                op,
                            )
                        })
                    })
            })
    }

    /// Whether a deletion selects the fact `id` of `project`, whose target is `trace` and `session`.
    fn deleted_fact(
        &self,
        project: usize,
        id: &[u8; 16],
        trace: &[u8; 16],
        session: &[u8],
    ) -> bool {
        let Some(&received_at) = self.fact_arrivals.get(id) else {
            return false;
        };
        let project = project_id(project).to_bytes();
        self.deletions.iter().any(|d| {
            system_oracle::deletion_selects_fact(
                &d.known.tombstone,
                &project,
                received_at,
                trace,
                session,
            )
        })
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
    /// Parent ids that sent spans name and no request has sent yet, as (project, trace, span id).
    awaited: Vec<(usize, [u8; 16], [u8; 8])>,
    /// The users that deletions of the deleted project name, with the issuer each names.
    named: Vec<(String, Option<String>)>,
}

/// Builds one request: a few spans, mostly of one new or recent trace. Spans report a session, a user, an
/// environment, a release, a model, usage, a provided cost and an error status from small seeded domains, so
/// that the rollups have keys that collide and measures to sum.
fn generate(plan: &Plan, rng: &dyn Rng, state: &mut ClientState) -> Option<Request> {
    let traces = &mut state.traces;
    let has = |dim| plan.mask.has(dim);
    let project = pick(rng, plan.all_projects() as u64) as usize;
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
    // With nested usage, half of the time it is a span whose parent no request has sent yet, so that the parent
    // arrives after a frame that held the repeat. That repeat is weighted toward the frame that shows a mark
    // taken after the duplicate drop (contract 5.3), which needs a span that stage B has seen (its request was
    // acknowledged) and that carries no usage (so its parent is not marked already). When such a span waits,
    // the repeat is the latest of them (the one the duplicate window most likely still holds) and is always
    // sent; otherwise a coin decides, and a waiting repeat is sent one time in two rather than four.
    let nth = pick(rng, 4) as usize;
    let awaited = &state.awaited;
    let waits = |sent: &SentSpan| {
        sent.project == project
            && awaited.iter().any(|(p, trace, parent)| {
                *p == project
                    && *trace == sent.info.trace
                    && parent[..] == sent.wire.parent_span_id[..]
            })
    };
    let exposes = |sent: &SentSpan| {
        sent.acked
            && sent.info.sent.input == 0
            && sent.info.sent.output == 0
            && sent.info.sent.cost_nusd == 0
    };
    let waiting = has(Dim::NestedUsage)
        && (state.spans.iter().any(|sent| waits(sent) && exposes(sent)) || coin(rng));
    let mut candidates = state
        .spans
        .iter()
        .filter(|sent| sent.project == project && (!waiting || waits(sent)));
    let again = if waiting {
        candidates.max_by_key(|sent| exposes(sent))
    } else {
        candidates.nth(nth)
    }
    .filter(|sent| {
        let odds = match (waiting, exposes(sent)) {
            (true, true) => 4,
            (true, false) => 2,
            (false, _) => 1,
        };
        has(Dim::ClientRetries) && pick(rng, 4) < odds
    })
    .cloned();
    let partial_repeat = again.is_some();
    // The parent that the repeat waits for: this request may send it beside the repeat.
    let repeat_parent: Option<([u8; 16], Vec<u8>)> = again
        .as_ref()
        .filter(|_| waiting)
        .map(|sent| (sent.info.trace, sent.wire.parent_span_id.clone()));
    if let Some(sent) = again {
        spans.push(sent.info);
        wire.push(sent.wire);
    }
    let session = (pick(rng, 3) > 0).then(|| format!("session-{}", pick(rng, 3)));
    // Beside a repeat whose parent is still to come there is room for its child and its parent.
    let count = count + u64::from(repeat_parent.is_some() && count == 1);
    // The position in `spans` of the first span this request adds.
    let first_new = spans.len();
    let mut sent_now: Vec<SentSpan> = Vec::new();
    // Whether a new span hangs under the repeat whose parent is still to come.
    let mut child_placed = false;
    for ordinal in 0..count {
        let other = traces.len() > 1 && pick(rng, 5) == 0;
        let trace = if other {
            traces[pick(rng, traces.len() as u64) as usize]
        } else {
            main
        };
        // Beside a repeat whose parent is still to come, the first new span is its child and carries usage, and
        // the next one is the repeat's parent: the frame of the weighting above.
        let child_of_repeat = repeat_parent.is_some() && !child_placed;
        // With nested usage, a span may be the parent that an earlier request of this client named for one of its
        // spans: the child was exported first, as a child that ends before its parent is.
        let parent_odds = if repeat_parent.is_some() { 3 } else { 1 };
        let awaited = (has(Dim::NestedUsage) && !child_of_repeat && pick(rng, 3) < parent_odds)
            .then(|| {
                let at = state.awaited.iter().position(|(p, trace, id)| {
                    *p == project
                        && repeat_parent
                            .as_ref()
                            .is_none_or(|(t, parent)| trace == t && id[..] == parent[..])
                })?;
                Some(state.awaited.remove(at))
            })
            .flatten();
        // Or it hangs under the span of an earlier request that this request sends again: stage B drops the
        // repeat, which still passes the mark of its new child on to its own parent.
        let under_repeat = spans
            .first()
            .filter(|_| {
                has(Dim::NestedUsage)
                    && partial_repeat
                    && awaited.is_none()
                    && (child_of_repeat || coin(rng))
            })
            .map(|repeat| repeat.trace);
        child_placed |= under_repeat.is_some();
        let (trace, span, root) = match (awaited, under_repeat) {
            (Some((_, trace, span)), _) => (trace, span, false),
            (None, Some(trace)) => (trace, random::<8>(rng), false),
            (None, None) => {
                let root = !other && !rootless;
                // One root per trace.
                rootless = true;
                (trace, random::<8>(rng), root)
            }
        };
        // A span of the repeat's trace reports no trace-level attribute: the forward fill would hand it to the
        // repeat, which would then be a changed row and no exact repeat.
        let bare = under_repeat.is_some()
            || repeat_parent
                .as_ref()
                .is_some_and(|(repeated, _)| *repeated == trace);
        // With nested usage, half of the spans below the root hang under the latest span of their trace in this
        // request, so chains form.
        let parent = if under_repeat.is_some() {
            Some(0)
        } else {
            (has(Dim::NestedUsage) && !root && coin(rng))
                .then(|| {
                    spans
                        .iter()
                        .enumerate()
                        .skip(first_new)
                        .rev()
                        .find(|(_, earlier)| earlier.trace == trace)
                        .map(|(at, _)| at)
                })
                .flatten()
        };
        // Or, one time in two, it names a parent that a later request will send.
        let later_parent =
            (has(Dim::NestedUsage) && !root && parent.is_none() && pick(rng, 2) == 0)
                .then(|| random::<8>(rng));
        if let Some(id) = later_parent {
            state.awaited.push((project, trace, id));
            if state.awaited.len() > 8 {
                state.awaited.remove(0);
            }
        }
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
        if let Some(session) = session.as_ref().filter(|_| !bare) {
            attributes.push(attribute("crucible.session.id", text(session.clone())));
        }
        // A span that names a parent a later request will send carries every trace-level attribute of its own,
        // so the forward fill gives it nothing and a request that sends it again sends an exact repeat.
        let own = later_parent.is_some();
        if (own || coin(rng)) && !bare {
            attributes.push(attribute("user.id", text(format!("user-{}", pick(rng, 3)))));
        }
        if (own || pick(rng, 3) == 0) && !bare {
            let environment = ["prod", "staging"][pick(rng, 2) as usize];
            attributes.push(attribute(
                "crucible.environment",
                text(environment.to_owned()),
            ));
        }
        if (own || pick(rng, 4) == 0) && !bare {
            attributes.push(attribute("crucible.release", text("r1".to_owned())));
        }
        let mut sent = system_oracle::Sent::default();
        // A span that names a later parent carries usage one time in four, so that most of them can be the
        // repeat of the weighting above.
        let usage = if own { pick(rng, 4) == 0 } else { coin(rng) };
        if child_of_repeat || usage {
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
            parent_span_id: match parent.and_then(|at| spans.get(at)) {
                Some(parent) => parent.span.to_vec(),
                None if root => Vec::new(),
                None => later_parent.map_or_else(|| vec![7; 8], |id| id.to_vec()),
            },
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
        sent_now.push(SentSpan {
            project,
            info,
            wire: span.clone(),
            acked: false,
        });
        spans.push(info);
        wire.push(span);
    }
    for sent in sent_now {
        state.spans.push(sent);
        if state.spans.len() > 16 {
            state.spans.remove(0);
        }
    }
    // The MCP operations come last and in their own traces, each call under the resource of its server. A mask
    // without the dimension draws nothing here.
    let mut mcp = McpTraffic::default();
    if has(Dim::Mcp) && pick(rng, 3) > 0 {
        for _ in 0..1 + pick(rng, 2) {
            generate_mcp(plan, rng, project, &state.named, &mut mcp);
        }
    }
    spans.append(&mut mcp.spans);
    let mut resource_spans = vec![ResourceSpans {
        resource: None,
        scope_spans: vec![ScopeSpans {
            scope: None,
            spans: wire,
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    }];
    resource_spans.append(&mut mcp.resources);
    let otlp = ExportTraceServiceRequest { resource_spans };
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
        mcp: Arc::new(mcp.sent),
    })
}

/// The first protocol version of each era.
const LEGACY_VERSION: &str = "2025-11-25";
const MODERN_VERSION: &str = "2026-07-28";

/// Operation durations in microseconds, one in each of several buckets of the MCP duration metric.
const MCP_DURATIONS: [u64; 8] = [
    400,
    9_000,
    15_000,
    80_000,
    700_000,
    3_000_000,
    45_000_000,
    400_000_000,
];

/// Tool schemas as a `tools/list` result sends them, with the canonical JSON of the input and the output
/// schema that the inventory's hash is taken of (contract 3.6). `N` stands for the tool's name. The wire forms
/// order their members otherwise than the canonical form does.
const TOOL_SCHEMAS: [(&str, &str, &str); 3] = [
    (
        r#"{"name":"N","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}"#,
        r#"{"properties":{"q":{"type":"string"}},"type":"object"}"#,
        "null",
    ),
    (
        r#"{"outputSchema":{"type":"object"},"inputSchema":{"required":["q"],"type":"object","properties":{"q":{"type":"string"},"n":{"type":"integer"}}},"name":"N"}"#,
        r#"{"properties":{"n":{"type":"integer"},"q":{"type":"string"}},"required":["q"],"type":"object"}"#,
        r#"{"type":"object"}"#,
    ),
    (r#"{"name":"N"}"#, "null", "null"),
];

/// What happened in one simulated MCP exchange, before any instrumentation saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Exchange {
    Ok,
    /// A result with `isError: true`, and the SDK annotation of the handler, if any.
    ToolError(Option<ErrorClass>, ErrorSubclass),
    /// A JSON-RPC error with this code; with the not-found annotation when set.
    Rpc(i32, bool),
    /// An HTTP status without a JSON-RPC body.
    Http(u16),
    Cancelled(CancelSource),
    TimedOut,
    /// The stream broke with the operation pending.
    Transport,
    /// The handler raised an exception that nothing reported.
    Exception,
}

/// The salt of a simulated project: fixed for the life of the project, as a salt is (contract 9.3).
fn project_salt(project: ProjectId) -> ProjectSalt {
    let mut bytes = [0xa5; 32];
    bytes[..16].copy_from_slice(&project.to_bytes());
    ProjectSalt::new(bytes)
}

fn flag(value: bool) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::BoolValue(value)),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What one MCP call shares between its spans: who serves it and how the client reached it.
struct McpCall {
    project: usize,
    trace: [u8; 16],
    server: &'static str,
    version: &'static str,
    deploy: &'static str,
    instance: String,
    era: Era,
    transport: Transport,
    /// The session id of a legacy HTTP session and whether the sender hashes it.
    session: Option<(String, bool)>,
    client: &'static str,
    start_micros: i64,
}

/// One MCP operation as one instrumentation reports it.
struct McpOpSpec {
    side: McpSide,
    capture: CaptureSource,
    span: [u8; 8],
    parent: Option<[u8; 8]>,
    method: &'static str,
    target: String,
    exchange: Exchange,
    /// An explicit class that the sender wrote although the exchange does not warrant it.
    stray_class: Option<ErrorClass>,
    duration_us: u64,
    round: u16,
    state_in: Option<[u8; 16]>,
    state_out: Option<[u8; 16]>,
    handle: String,
    subject: Option<(String, String)>,
    user_id: String,
    content: Option<(String, String)>,
    /// The tools a `tools/list` result names: each name and its schema variant.
    tools: Vec<(String, usize)>,
    bytes: (u64, u64),
}

/// What every value starts with that the generator plants under a key that must never be stored. The oracle
/// looks for it in every text cell of the span tables and the plain MCP tables.
const PLANTED: &str = "sim-planted:";

/// The keys that must never be stored, as the contract names them: Authorization values, cookies,
/// `Mcp-Param-*` values, baggage and raw IP addresses (3.5), the raw subject and issuer (9.3) and the raw session
/// id (9.4), with a header name in another case. The list follows the contract and not the normalizer, so that
/// a key the normalizer forgets is planted all the same.
const NEVER_STORED: [&str; 11] = [
    "crucible.mcp.user.subject",
    "crucible.mcp.user.issuer",
    "mcp.session.id",
    "client.address",
    "network.peer.address",
    "http.request.header.authorization",
    "http.request.header.cookie",
    "HTTP.Request.Header.Cookie",
    "http.response.header.set-cookie",
    "http.request.header.mcp-param-region",
    "baggage.tenant",
];

/// An event of an MCP operation's span with every key of [`NEVER_STORED`], each with a value the span never
/// sends otherwise. It takes no draw: the values follow from the span id.
fn planted_event(span: [u8; 8], time_unix_nano: u64) -> Event {
    Event {
        time_unix_nano,
        name: "sim.planted".to_owned(),
        attributes: NEVER_STORED
            .iter()
            .enumerate()
            .map(|(i, key)| attribute(key, text(format!("{PLANTED}{i}:{}", hex(&span)))))
            .collect(),
        dropped_attributes_count: 0,
    }
}

/// The wire span of one MCP operation and what the oracle expects of its rows. The keys follow what each
/// capture source sends: an SDK wrapper writes the `crucible.mcp.*` keys, the proxy the keys it can observe on
/// the wire, and a server without either only the semantic-convention keys. Every span also carries an event
/// with the keys that must never be stored ([`planted_event`]).
fn mcp_span(call: &McpCall, op: &McpOpSpec) -> (Span, SpanInfo, McpSent) {
    use Exchange as X;
    let sdk = op.capture == CaptureSource::Sdk;
    let proxy = matches!(
        op.capture,
        CaptureSource::ProxyStdio | CaptureSource::ProxyHttp
    );
    let http = call.transport == Transport::StreamableHttp;
    let notification = op.method.starts_with("notifications/");
    let mut attributes = vec![attribute("mcp.method.name", text(op.method.to_owned()))];
    let mut put = |key: &str, value: &str| {
        if !value.is_empty() {
            attributes.push(attribute(key, text(value.to_owned())));
        }
    };
    let version = match call.era {
        Era::Legacy => LEGACY_VERSION,
        Era::Modern => MODERN_VERSION,
    };
    put("mcp.protocol.version", version);
    put("network.transport", if http { "tcp" } else { "pipe" });
    match op.method {
        "tools/call" => put("gen_ai.tool.name", &op.target),
        "prompts/get" => put("gen_ai.prompt.name", &op.target),
        "resources/read" => put("crucible.mcp.resource.template", &op.target),
        _ => {}
    }
    if sdk || proxy {
        put("crucible.mcp.capture_source", op.capture.as_str());
        put("crucible.mcp.era", call.era.as_str());
        put("crucible.mcp.transport", call.transport.as_str());
        put("crucible.mcp.server.name", call.server);
        put("crucible.mcp.server.version", call.version);
        put("crucible.mcp.client.name", call.client);
    }
    if let Some((id, hashed)) = &call.session {
        let sent = if *hashed {
            hex(&Sha256::digest(id.as_bytes())[..16])
        } else {
            id.clone()
        };
        put("mcp.session.id", &sent);
    }

    // The outcome keys, and what the shared classifier is given for the exchange.
    let class =
        |class: ErrorClass| attribute("crucible.mcp.error.class", text(class.as_str().to_owned()));
    let mut outcome = Vec::new();
    let mut input = McpErrorInput {
        era: call.era,
        side: op.side,
        jsonrpc_code: None,
        is_error: false,
        http_status: (http && sdk).then_some(if notification { 202 } else { 200 }),
        cancel_source: CancelSource::None,
        timed_out: false,
        transport_failure: false,
        unhandled_exception: false,
        annotations: McpErrorAnnotations::NONE,
    };
    let mut explicit = op.stray_class;
    match op.exchange {
        X::Ok => {}
        X::ToolError(mark, subclass) => {
            input.is_error = true;
            outcome.push(attribute("error.type", text("tool_error".to_owned())));
            if sdk || proxy {
                explicit = explicit.or(Some(mark.unwrap_or(ErrorClass::ToolExecution)));
            }
            if subclass != ErrorSubclass::None {
                input.annotations.subclass = subclass;
                outcome.push(attribute(
                    "crucible.mcp.error.subclass",
                    text(subclass.as_str().to_owned()),
                ));
            }
        }
        X::Rpc(code, not_found) => {
            input.jsonrpc_code = Some(code);
            outcome.push(attribute("error.type", text(code.to_string())));
            outcome.push(attribute(
                "rpc.response.status_code",
                text(code.to_string()),
            ));
            if not_found {
                explicit = Some(ErrorClass::NotFound);
            }
        }
        X::Http(status) => {
            input.http_status = Some(status);
            // The status as the error type, as the proxy and the HTTP conventions report it.
            outcome.push(attribute("error.type", text(status.to_string())));
        }
        X::Cancelled(source) => {
            input.cancel_source = source;
            outcome.push(attribute(
                "crucible.mcp.cancel.source",
                text(source.as_str().to_owned()),
            ));
            explicit = explicit.or(Some(ErrorClass::Cancelled));
        }
        X::TimedOut => {
            input.timed_out = true;
            outcome.push(attribute("crucible.mcp.timed_out", flag(true)));
        }
        X::Transport => {
            input.transport_failure = true;
            outcome.push(attribute("error.type", text("transport".to_owned())));
            explicit = Some(ErrorClass::Transport);
        }
        X::Exception => {
            input.unhandled_exception = true;
            outcome.push(attribute("error.type", text("RuntimeError".to_owned())));
        }
    }
    if let Some(explicit) = explicit {
        input.annotations.explicit_class = Some(explicit);
        outcome.push(class(explicit));
    }
    if let Some(status) = input.http_status {
        outcome.push(attribute(
            "http.response.status_code",
            int(u64::from(status)),
        ));
    }
    let expected = classify_mcp_error(&input);
    attributes.extend(outcome);

    if !notification {
        attributes.push(attribute(
            "jsonrpc.request.id",
            text(format!("{}", op.span[0])),
        ));
    } else if sdk || proxy {
        attributes.push(attribute("crucible.mcp.is_notification", flag(true)));
    }
    if op.round > 0 {
        attributes.push(attribute(
            "crucible.mcp.mrtr.round",
            int(u64::from(op.round)),
        ));
    }
    if let Some(state) = op.state_in {
        attributes.push(attribute("crucible.mcp.mrtr.state_in", text(hex(&state))));
    }
    if let Some(state) = op.state_out {
        attributes.push(attribute("crucible.mcp.mrtr.state_out", text(hex(&state))));
        attributes.push(attribute(
            "crucible.mcp.result_type",
            text("input_required".to_owned()),
        ));
    }
    let mut put = |key: &str, value: &str| {
        if !value.is_empty() {
            attributes.push(attribute(key, text(value.to_owned())));
        }
    };
    put("crucible.mcp.handle.id", &op.handle);
    put("user.id", &op.user_id);
    if let Some((issuer, subject)) = &op.subject {
        put("crucible.mcp.user.issuer", issuer);
        put("crucible.mcp.user.subject", subject);
        put("crucible.mcp.identity.source", "token_verified");
    }
    if op.bytes.0 > 0 {
        attributes.push(attribute("crucible.mcp.request.bytes", int(op.bytes.0)));
    }
    if op.bytes.1 > 0 {
        attributes.push(attribute("crucible.mcp.response.bytes", int(op.bytes.1)));
    }
    if let Some((args, result)) = &op.content {
        attributes.push(attribute("gen_ai.tool.call.arguments", text(args.clone())));
        attributes.push(attribute("gen_ai.tool.call.result", text(result.clone())));
        // A server without a wrapper sends no flag; the content it sends counts as captured.
        if sdk {
            attributes.push(attribute("crucible.mcp.content.captured", flag(true)));
        }
    }
    if !op.tools.is_empty() {
        let listed: Vec<String> = op
            .tools
            .iter()
            .map(|(name, variant)| TOOL_SCHEMAS[*variant].0.replace('N', name))
            .collect();
        attributes.push(attribute(
            "crucible.mcp.tools",
            text(format!("[{}]", listed.join(", "))),
        ));
    }

    let failed = expected.status != McpStatus::Ok;
    let start_nanos = (call.start_micros as u64) * 1_000;
    let wire = Span {
        trace_id: call.trace.to_vec(),
        span_id: op.span.to_vec(),
        parent_span_id: op.parent.map(|id| id.to_vec()).unwrap_or_default(),
        name: format!("{} {}", op.method, op.target),
        kind: if op.side == McpSide::Client { 3 } else { 2 },
        start_time_unix_nano: start_nanos,
        end_time_unix_nano: start_nanos + op.duration_us * 1_000,
        attributes,
        events: vec![planted_event(op.span, start_nanos)],
        status: failed.then(|| Status {
            message: String::new(),
            code: 2,
        }),
        ..Span::default()
    };
    let info = SpanInfo {
        trace: call.trace,
        span: op.span,
        root: op.parent.is_none(),
        sent: system_oracle::Sent {
            error: failed,
            ..system_oracle::Sent::default()
        },
    };
    let sent = McpSent {
        side: op.side,
        capture_source: op.capture,
        era: call.era,
        transport: call.transport,
        method: op.method.to_owned(),
        target: op.target.clone(),
        server: call.server.to_owned(),
        version: call.version.to_owned(),
        deploy: call.deploy.to_owned(),
        instance: call.instance.clone(),
        client: if sdk || proxy {
            call.client.to_owned()
        } else {
            String::new()
        },
        notification: notification && (sdk || proxy),
        start_micros: call.start_micros,
        duration_us: op.duration_us,
        request_bytes: op.bytes.0,
        response_bytes: op.bytes.1,
        parent: op.parent.unwrap_or_default(),
        outcome: expected,
        jsonrpc_code: input.jsonrpc_code,
        http_status: input.http_status,
        round: op.round,
        state_in: op.state_in,
        state_out: op.state_out,
        session: call.session.clone(),
        handle: op.handle.clone(),
        subject: op.subject.clone(),
        user_id: op.user_id.clone(),
        content: op
            .content
            .as_ref()
            .map(|(args, result)| (args.len() as u64, result.len() as u64)),
        tools: op
            .tools
            .iter()
            .map(|(name, variant)| {
                let (_, input, output) = TOOL_SCHEMAS[*variant];
                (name.clone(), input.to_owned(), output.to_owned())
            })
            .collect(),
    };
    (wire, info, sent)
}

/// The MCP operations a request carries beside its spans.
#[derive(Default)]
struct McpTraffic {
    /// One resource per call: the server's, with the spans its instrumentation exports.
    resources: Vec<ResourceSpans>,
    spans: Vec<SpanInfo>,
    /// (trace, span) of each operation and what the oracle expects of it.
    sent: SentOps,
}

/// Builds the MCP operations of one call: a server-side operation as an SDK wrapper, a server without a
/// wrapper or the proxy reports it, in either era and over either transport; with the dimensions of the mask,
/// the agent's client-side span above it, the proxy's row of the same call beside the SDK's (double capture),
/// every error class, a multi-round exchange, a tool list, a deploy that changes, captured content and a user
/// identity.
///
/// In the deleted project, an operation that carries a subject carries, three times in four, a user that a
/// deletion names, under the issuer it does not name: that deletion keeps the operation, which a deletion that
/// ignored its issuer would drop.
fn generate_mcp(
    plan: &Plan,
    rng: &dyn Rng,
    project: usize,
    named: &[(String, Option<String>)],
    out: &mut McpTraffic,
) {
    let has = |dim| plan.mask.has(dim);
    let of = |items: &[&'static str]| items[pick(rng, items.len() as u64) as usize];
    let era = if coin(rng) { Era::Legacy } else { Era::Modern };
    let transport = if coin(rng) {
        Transport::Stdio
    } else {
        Transport::StreamableHttp
    };
    let http = transport == Transport::StreamableHttp;
    let day = if has(Dim::MultiDay) { pick(rng, 3) } else { 1 };
    let call = McpCall {
        project,
        trace: random::<16>(rng),
        server: of(&["files", "orders"]),
        version: if has(Dim::McpDeploys) {
            of(&["1.0.0", "1.1.0"])
        } else {
            "1.0.0"
        },
        deploy: if has(Dim::McpDeploys) {
            of(&["", "blue", "green"])
        } else {
            ""
        },
        instance: format!("pod-{}", pick(rng, 2)),
        era,
        transport,
        session: (era == Era::Legacy && http && coin(rng))
            .then(|| (format!("session-{}", pick(rng, 3)), coin(rng))),
        client: of(&["", "inspector", "agent-x"]),
        start_micros: START_BASE_MICROS + (day * 86_400_000_000 + pick(rng, 3_600_000_000)) as i64,
    };
    let method = match pick(rng, 8) {
        0 => "tools/list",
        1 => "prompts/get",
        2 => "resources/read",
        3 => "notifications/initialized",
        _ => "tools/call",
    };
    let notification = method.starts_with("notifications/");
    let capture = match pick(rng, 4) {
        0 => CaptureSource::Otlp,
        1 if http => CaptureSource::ProxyHttp,
        1 => CaptureSource::ProxyStdio,
        _ => CaptureSource::Sdk,
    };
    let sdk = capture == CaptureSource::Sdk;
    let plain = capture == CaptureSource::Otlp;
    let target = match method {
        "tools/call" => format!("tool-{}", pick(rng, 3)),
        "prompts/get" => "greeting".to_owned(),
        // The proxy reports no resource template.
        "resources/read" if sdk => "file:///{path}".to_owned(),
        _ => String::new(),
    };
    // What happened, among what the capture source can observe (contract 9.2).
    let exchange = if !has(Dim::McpErrors) || notification || coin(rng) {
        Exchange::Ok
    } else {
        let codes = [
            -32700, -32600, -32601, -32602, -32603, -32002, -32020, -32000,
        ];
        let code = codes[pick(rng, codes.len() as u64) as usize];
        match pick(rng, 8) {
            0 | 1 => Exchange::Rpc(code, sdk && code == -32602 && coin(rng)),
            2 if sdk => {
                let mark = [
                    None,
                    Some(ErrorClass::Validation),
                    Some(ErrorClass::NotFound),
                ][pick(rng, 3) as usize];
                let subclass = if mark.is_none() && coin(rng) {
                    ErrorSubclass::ALL[pick(rng, ErrorSubclass::ALL.len() as u64) as usize]
                } else {
                    ErrorSubclass::None
                };
                Exchange::ToolError(mark, subclass)
            }
            2 | 3 => Exchange::ToolError(None, ErrorSubclass::None),
            4 if http && !sdk => Exchange::Http([401, 403, 429, 500, 503][pick(rng, 5) as usize]),
            5 if !plain => Exchange::Cancelled(if coin(rng) {
                CancelSource::Notification
            } else {
                CancelSource::StreamClose
            }),
            6 if sdk => Exchange::TimedOut,
            6 if !plain => Exchange::Transport,
            7 if !sdk => Exchange::Exception,
            _ => Exchange::Rpc(code, false),
        }
    };
    // An SDK may annotate a class that the exchange does not warrant; the channel decides then.
    let stray_class = (has(Dim::McpErrors) && sdk && pick(rng, 8) == 0).then(|| {
        [
            ErrorClass::ToolExecution,
            ErrorClass::Validation,
            ErrorClass::Auth,
            ErrorClass::Internal,
        ][pick(rng, 4) as usize]
    });
    let duration_us =
        MCP_DURATIONS[pick(rng, MCP_DURATIONS.len() as u64) as usize] + pick(rng, 300);

    // The agent's side, when it is instrumented: a client span that the server's span hangs under, because the
    // client wrapper sends its trace context in `params._meta`.
    let client = (pick(rng, 3) > 0).then(|| random::<8>(rng));
    let mut specs: Vec<McpOpSpec> = Vec::new();
    if let Some(span) = client {
        specs.push(McpOpSpec {
            side: McpSide::Client,
            capture: CaptureSource::Sdk,
            span,
            parent: None,
            method,
            target: target.clone(),
            exchange: match exchange {
                Exchange::Rpc(code, _) => Exchange::Rpc(code, false),
                _ => Exchange::Ok,
            },
            stray_class: None,
            duration_us: duration_us + 200,
            round: 0,
            state_in: None,
            state_out: None,
            handle: String::new(),
            subject: None,
            user_id: String::new(),
            content: None,
            tools: Vec::new(),
            bytes: (0, 0),
        });
    }
    let parent = client.or_else(|| coin(rng).then_some([9; 8]));
    let identified = has(Dim::McpIdentity) && !plain && pick(rng, 3) > 0;
    let server_op = |span: [u8; 8], capture: CaptureSource, exchange: Exchange| McpOpSpec {
        side: McpSide::Server,
        capture,
        span,
        parent,
        method,
        target: target.clone(),
        exchange,
        stray_class: stray_class.filter(|_| capture == CaptureSource::Sdk),
        duration_us,
        round: 0,
        state_in: None,
        state_out: None,
        handle: String::new(),
        subject: None,
        user_id: String::new(),
        content: None,
        tools: Vec::new(),
        bytes: (0, 0),
    };
    let mut main = server_op(random::<8>(rng), capture, exchange);
    if !plain {
        main.bytes = (1 + pick(rng, 4_000), pick(rng, 20_000));
    }
    if sdk && pick(rng, 4) == 0 {
        main.handle = format!("ord_{}", pick(rng, 3));
    }
    if identified {
        let weighted = (Some(project) == plan.deleted() && !named.is_empty() && pick(rng, 4) > 0)
            .then(|| {
                let (user, issuer) = &named[pick(rng, named.len() as u64) as usize];
                let other = match issuer {
                    Some(issuer) => ISSUERS.iter().find(|i| **i != issuer.as_str()).copied(),
                    None => Some(ISSUERS[pick(rng, 2) as usize]),
                };
                other.map(|other| (other.to_owned(), user.clone()))
            })
            .flatten();
        main.subject = Some(weighted.unwrap_or_else(|| {
            (
                format!("https://issuer-{}.example", pick(rng, 2)),
                format!("subject-{}", pick(rng, 3)),
            )
        }));
    }
    // The identify hook's user id needs no salt; without a control store it is dropped all the same.
    if sdk && pick(rng, 3) == 0 {
        main.user_id = format!("user-{}", pick(rng, 3));
    }
    if has(Dim::McpContent)
        && method == "tools/call"
        && !matches!(
            capture,
            CaptureSource::ProxyStdio | CaptureSource::ProxyHttp
        )
        && coin(rng)
    {
        main.content = Some((
            format!("{{\"q\":\"{}\"}}", "a".repeat(pick(rng, 40) as usize)),
            "{\"ok\":true}".to_owned(),
        ));
    }
    if has(Dim::McpInventory) && method == "tools/list" && exchange == Exchange::Ok {
        // The tools a deploy lists change with the deploy and from one listing to the next.
        main.tools = (0..4)
            .filter(|_| pick(rng, 3) > 0)
            .map(|tool| (format!("tool-{tool}"), pick(rng, 3) as usize))
            .collect();
    }
    // A multi-round exchange: the first call asks for input, the retries carry its state back.
    let rounds = if has(Dim::McpMrtr)
        && era == Era::Modern
        && method == "tools/call"
        && sdk
        && exchange == Exchange::Ok
        && pick(rng, 3) == 0
    {
        1 + pick(rng, 2) as u16
    } else {
        0
    };
    let mut state = None;
    for round in 0..rounds {
        let mut ask = server_op(random::<8>(rng), capture, Exchange::Ok);
        ask.round = round;
        ask.state_in = state;
        state = Some(random::<16>(rng));
        ask.state_out = state;
        specs.push(ask);
    }
    if rounds > 0 {
        main.round = rounds;
        main.state_in = state;
    }
    // The proxy in front of the same server sees the same call (contract 9.5): a second server-side row, with
    // what a proxy can observe.
    let seen_by_proxy = match exchange {
        Exchange::Ok | Exchange::Rpc(..) | Exchange::Cancelled(_) => Some(exchange),
        Exchange::ToolError(..) => Some(Exchange::ToolError(None, ErrorSubclass::None)),
        _ => None,
    };
    if has(Dim::McpDoubleCapture)
        && sdk
        && coin(rng)
        && let Some(exchange) = seen_by_proxy
    {
        let source = if http {
            CaptureSource::ProxyHttp
        } else {
            CaptureSource::ProxyStdio
        };
        let exchange = match exchange {
            Exchange::Rpc(code, _) => Exchange::Rpc(code, false),
            other => other,
        };
        let mut twin = server_op(random::<8>(rng), source, exchange);
        twin.bytes = main.bytes;
        specs.push(twin);
    }
    specs.push(main);

    let mut spans = Vec::new();
    for spec in &specs {
        let (wire, info, sent) = mcp_span(&call, spec);
        spans.push(wire);
        out.spans.push(info);
        out.sent.push((call.trace, spec.span, sent));
    }
    let mut resource = vec![
        attribute("service.name", text(call.server.to_owned())),
        attribute("service.version", text(call.version.to_owned())),
        attribute("service.instance.id", text(call.instance.clone())),
    ];
    if !call.deploy.is_empty() {
        resource.push(attribute(
            "crucible.deploy.id",
            text(call.deploy.to_owned()),
        ));
    }
    let _ = call.project;
    out.resources.push(ResourceSpans {
        resource: Some(Resource {
            attributes: resource,
            ..Resource::default()
        }),
        scope_spans: vec![ScopeSpans {
            scope: None,
            spans,
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    });
}

/// The project salts of the simulated control store: one fixed salt per project, and a share of the reads fail,
/// as a store that does not answer does.
struct SimSalts {
    fail_ppm: u32,
    rng: Arc<dyn Rng>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SimSalts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimSalts")
            .field("fail_ppm", &self.fail_ppm)
            .finish_non_exhaustive()
    }
}

impl ProjectSalts for SimSalts {
    fn configured(&self) -> bool {
        true
    }

    fn salt(
        &self,
        project: ProjectId,
    ) -> futures::future::BoxFuture<'_, Result<Option<ProjectSalt>, SaltUnavailable>> {
        let failed = self.rng.next_u64() % 1_000_000 < u64::from(self.fail_ppm);
        if failed {
            self.shared.ledger().stats.salt_waits += 1;
        }
        Box::pin(futures::future::ready(if failed {
            Err(SaltUnavailable("the store did not answer".to_owned()))
        } else {
            Ok(Some(project_salt(project)))
        }))
    }
}

/// The deletion requests of the simulated control store: what the ledger holds, and a share of the reads fail,
/// as a store that does not answer does.
struct SimDeletions {
    fail_ppm: u32,
    rng: Arc<dyn Rng>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SimDeletions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimDeletions")
            .field("fail_ppm", &self.fail_ppm)
            .finish_non_exhaustive()
    }
}

impl SimDeletions {
    fn fails(&self) -> bool {
        let failed = self.rng.next_u64() % 1_000_000 < u64::from(self.fail_ppm);
        if failed {
            self.shared.ledger().stats.deletion_waits += 1;
        }
        failed
    }
}

impl DeletionRequests for SimDeletions {
    fn configured(&self) -> bool {
        true
    }

    fn all(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        let answer = if self.fails() {
            Err(DeletionsUnavailable("the store did not answer".to_owned()))
        } else {
            let ledger = self.shared.ledger();
            Ok(ledger.deletions.iter().map(|d| d.known.clone()).collect())
        };
        Box::pin(futures::future::ready(answer))
    }

    fn request(
        &self,
        project: ProjectId,
        id: DeletionRequestId,
    ) -> futures::future::BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>> {
        let answer = if self.fails() {
            Err(DeletionsUnavailable("the store did not answer".to_owned()))
        } else {
            let ledger = self.shared.ledger();
            Ok(ledger
                .deletions
                .iter()
                .map(|d| &d.known)
                .find(|known| {
                    known.tombstone.project_id == project && known.tombstone.request_id == id
                })
                .cloned())
        };
        Box::pin(futures::future::ready(answer))
    }
}

/// Appends the `Tombstone` frame of a deletion, as the node timer does through `crucible_lifecycle::WalLog`, and
/// returns its LSN once the frame is durable. The frame carries no issuer.
async fn append_tombstone(wal: &Wal, tombstone: &Tombstone) -> Option<u64> {
    use crucible_wal_codec::TombstoneSelector as Codec;
    let record = Record::Tombstone(crucible_wal_codec::Tombstone {
        deletion_request_id: tombstone.request_id,
        project_id: tombstone.project_id,
        requested_at: tombstone.requested_at,
        deletion_seq: tombstone.deletion_seq,
        selector: match &tombstone.selector {
            TombstoneSelector::Traces(ids) => Codec::Traces(ids.clone()),
            TombstoneSelector::User { user_id, .. } => Codec::User(user_id),
            TombstoneSelector::Session(id) => Codec::Session(id),
            TombstoneSelector::Project => Codec::Project,
        },
    });
    let payload = record.encode().ok()?;
    let frame = crucible_wal_codec::PreparedFrame::new(
        record.record_type(),
        payload,
        crucible_wal_codec::Compression::Lz4,
    )
    .ok()?;
    wal.append(frame).await.ok().map(Lsn::get)
}

/// The node timer's part of a deletion (contract 13.3, steps 2 and 4): an open request without a tombstone in
/// this node's log gets one, and a request whose tombstone the checkpoint has passed is complete.
async fn node_timer(shared: &Shared, wal: &Wal, model: &ChModel, now: u64) {
    let pending: Vec<(usize, Tombstone)> = shared
        .ledger()
        .deletions
        .iter()
        .enumerate()
        .filter(|(_, d)| d.completed_at.is_none() && d.tombstone_lsn.is_none())
        .map(|(at, d)| (at, d.known.tombstone.clone()))
        .collect();
    for (at, tombstone) in pending {
        if let Some(lsn) = append_tombstone(wal, &tombstone).await {
            let mut ledger = shared.ledger();
            ledger.stats.tombstones += 1;
            if let Some(deletion) = ledger.deletions.get_mut(at) {
                deletion.tombstone_lsn = Some(lsn);
                deletion.known.state = DeletionState::Logged;
            }
            drop(ledger);
            shared.trace.event("deletion.tombstone", &[at as u64, lsn]);
        }
    }
    complete_passed(
        shared,
        model,
        wal.watermark().map_or(0, |w| w.last_lsn().get()),
        now,
    );
}

/// Completes every open request whose tombstone is at or below `watermark`. The executor deletes what it
/// selects from `now` on, or from when the last insert in flight lands: an attempt sent before stage B knew the
/// request (a probe read absent while it was still committing, contract 5.5) can land after the delete, and
/// the executor's verification finds and deletes it, which stage B has no part in.
fn complete_passed(shared: &Shared, model: &ChModel, watermark: u64, now: u64) {
    let now = now.max(model.in_flight_until());
    let mut ledger = shared.ledger();
    let mut completed = Vec::new();
    for (at, deletion) in ledger.deletions.iter_mut().enumerate() {
        if deletion.completed_at.is_none()
            && deletion.tombstone_lsn.is_some_and(|lsn| lsn <= watermark)
        {
            deletion.completed_at = Some(now);
            deletion.known.state = DeletionState::Completed;
            completed.push(at as u64);
        }
    }
    ledger.stats.deletions_completed += completed.len() as u64;
    drop(ledger);
    for at in completed {
        shared.trace.event("deletion.completed", &[at, now]);
    }
}

/// Makes a deletion request of the deleted project, about what it has sent lately: one or two of its traces, a
/// user (of its spans or MCP operations, or an MCP subject) with or without an issuer, a session, or now and then
/// the whole project. An MCP subject is one that an operation in `unshipped` carries, when that is given (the
/// operations of the frames above the watermark, which a request read before shipping applies to). `false` when
/// the project has nothing acknowledged yet.
fn make_request(
    shared: &Shared,
    rng: &dyn Rng,
    requested_at: Timestamp,
    completed_at: Option<u64>,
    unshipped: Option<&BTreeSet<McpKey>>,
) -> bool {
    let Some(deleted) = shared.plan.deleted() else {
        return false;
    };
    let mut ledger = shared.ledger();
    let traces: Vec<[u8; 16]> = ledger
        .acked
        .iter()
        .rev()
        .map(|&index| &ledger.requests[index])
        .filter(|request| request.project == deleted)
        .take(8)
        .flat_map(|request| request.spans.iter().map(|span| span.trace))
        .collect();
    if traces.is_empty() {
        return false;
    }
    let trace =
        |rng: &dyn Rng| TraceId::from_bytes(traces[pick(rng, traces.len() as u64) as usize]);
    let project = project_id(deleted).to_bytes();
    let subjects: Vec<(String, String)> = ledger
        .mcp
        .iter()
        .filter(|(key, _)| key.0 == project && unshipped.is_none_or(|keys| keys.contains(*key)))
        .filter_map(|(_, sent)| sent.subject.clone())
        .collect();
    // A request read before shipping applies to every frame above the watermark: three times in four it names a
    // subject that an operation there carries. A deletion of the whole project, which selects everything that
    // arrived before it, is rare.
    let kind = if unshipped.is_some() && !subjects.is_empty() && pick(rng, 4) > 0 {
        12
    } else {
        pick(rng, 34)
    };
    let (kind, selector) = match kind {
        0..=11 => {
            let mut ids = vec![trace(rng)];
            if coin(rng) {
                ids.push(trace(rng));
            }
            (0, TombstoneSelector::Traces(ids))
        }
        12..=23 => {
            // Three times in four an MCP subject that the project's operations carry, under the other issuer
            // (half of the time), its own or any: a deletion that names the other issuer keeps those operations.
            let (user, issuer) = if !subjects.is_empty() && pick(rng, 4) > 0 {
                let (own, subject) = subjects[pick(rng, subjects.len() as u64) as usize].clone();
                let other = ISSUERS.iter().find(|issuer| **issuer != own).copied();
                let issuer = match pick(rng, 4) {
                    0 => None,
                    1 => Some(own),
                    _ => other.map(str::to_owned),
                };
                (subject, issuer)
            } else {
                let user = if shared.plan.mask.has(Dim::McpIdentity) && coin(rng) {
                    format!("subject-{}", pick(rng, 3))
                } else {
                    format!("user-{}", pick(rng, 3))
                };
                (
                    user,
                    coin(rng).then(|| ISSUERS[pick(rng, 2) as usize].to_owned()),
                )
            };
            (
                1,
                TombstoneSelector::User {
                    user_id: user.into(),
                    issuer: issuer.map(Into::into),
                },
            )
        }
        24..=32 => (
            2,
            TombstoneSelector::Session(format!("session-{}", pick(rng, 3)).into()),
        ),
        _ => (3, TombstoneSelector::Project),
    };
    let n = ledger.deletions.len() as u64 + 1;
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&n.to_be_bytes());
    let known = KnownDeletion {
        tombstone: Tombstone {
            request_id: DeletionRequestId::from_bytes(id),
            project_id: project_id(deleted),
            requested_at,
            deletion_seq: n,
            selector,
        },
        state: if completed_at.is_some() {
            DeletionState::Completed
        } else {
            DeletionState::Pending
        },
    };
    ledger.deletions.push(SimDeletion {
        known,
        tombstone_lsn: None,
        completed_at,
    });
    ledger.stats.deletions += 1;
    ledger.stats.deletions_while_down += u64::from(completed_at.is_some());
    drop(ledger);
    shared
        .trace
        .event("deletion.request", &[n, kind, requested_at.micros() as u64]);
    true
}

/// What the deletion workload of one incarnation takes.
struct DeleterCtx {
    shared: Arc<Shared>,
    wal: Wal,
    model: ChModel,
    env: Env,
    rng: Arc<dyn Rng>,
}

/// The deletion workload of one incarnation: a few requests while the clients send (paced as they are, so that
/// traffic follows each), each accepted on a node whose clock is up to 2 s off this one's, and the node timer,
/// which appends their tombstones and completes them.
async fn deleter(ctx: DeleterCtx) {
    let rng = ctx.rng.as_ref();
    let mut quota = 1 + pick(rng, 3);
    loop {
        ctx.env
            .clock
            .sleep(Duration::from_micros(200 + pick(rng, 3_000)))
            .await;
        if quota > 0 && pick(rng, 3) == 0 {
            let skew = pick(rng, 4_000_001) as i64 - 2_000_000;
            let requested_at = ctx.env.clock.wall_now().saturating_add_micros(skew);
            if make_request(&ctx.shared, rng, requested_at, None, None) {
                quota -= 1;
            }
        }
        let now = ctx.env.clock.now().nanos() / 1_000;
        node_timer(&ctx.shared, &ctx.wal, &ctx.model, now).await;
    }
}

/// The recent-span index's hook, with the oracle listening: every frame that stage B admits is recorded with
/// all the spans stage A made of it, the exact repeats that stage B then drops included, and with the first
/// LSN the WAL retains at that moment. The usage-aggregate rule is checked against exactly that.
struct Observed {
    inner: Arc<RecentHook>,
    wal: Wal,
    shared: Arc<Shared>,
    /// The watermark this incarnation recovered: frames at or below it only rebuild stage B.
    watermark: Lsn,
    /// The last LSN of the sealed batches this incarnation recovered: pairs of those batches may have landed
    /// before the restart.
    sealed_through: u64,
    /// Stage A's context, to make the records of a frame of the deleted project again for I9's model.
    ctx: NormCtx<'static>,
    /// The deleted project.
    deleted: Option<usize>,
}

impl std::fmt::Debug for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observed")
            .field("watermark", &self.watermark)
            .finish_non_exhaustive()
    }
}

impl Observed {
    /// The spans of an `IngestRequest` frame that stage A makes records of, in wire order.
    fn spans(&self, source: &WalFrame) -> Option<Vec<FrameSpan>> {
        let payload = source.payload().ok()?;
        let Ok(Record::IngestRequest(request)) =
            Record::decode(RecordType::IngestRequest, &payload)
        else {
            return None;
        };
        let project =
            (0..self.shared.plan.all_projects()).find(|&p| project_id(p) == request.project_id)?;
        let scanned = crucible_otlp::TraceRequest::scan(request.otlp, None).ok()?;
        let ledger = self.shared.ledger();
        let rejected = |ordinal: u32| {
            request
                .rejected
                .get((ordinal / 8) as usize)
                .is_some_and(|byte| byte & (1 << (ordinal % 8)) != 0)
        };
        Some(
            scanned
                .spans()
                .filter(|span| !rejected(span.ordinal()) && span.rejection().is_none())
                .map(|span| {
                    let (trace, id) = (span.trace_id().to_bytes(), span.span_id().to_bytes());
                    let parent = span.parent_span_id().map(|parent| parent.to_bytes());
                    let carries = ledger.sent.get(&(project, trace, id)).is_some_and(|sent| {
                        sent.input > 0 || sent.output > 0 || sent.cost_nusd > 0
                    });
                    FrameSpan::new(
                        &request.project_id.to_bytes(),
                        &trace,
                        &id,
                        parent.as_ref().map(|parent| &parent[..]),
                        carries,
                    )
                })
                .collect(),
        )
    }
}

impl Observed {
    /// The project of an `IngestRequest` or `FactBatch` frame.
    fn project_of(&self, source: &WalFrame) -> Option<usize> {
        let payload = source.payload().ok()?;
        let id = match Record::decode(source.record_type(), &payload).ok()? {
            Record::IngestRequest(request) => request.project_id,
            Record::FactBatch(batch) => batch.project_id,
            _ => return None,
        };
        (0..self.shared.plan.all_projects()).find(|&p| project_id(p) == id)
    }

    /// The spans of a frame as stage A makes them, with what a deletion selects them by.
    fn deletable(&self, source: &WalFrame) -> Option<FrameSpans> {
        let payload = source.payload().ok()?;
        let Ok(Record::IngestRequest(request)) =
            Record::decode(RecordType::IngestRequest, &payload)
        else {
            return None;
        };
        let ingest = IngestFrame {
            wal_epoch: self.wal.epoch(),
            lsn: source.lsn(),
            received_at: source.received_at(),
            project_id: request.project_id,
            api_key_id: request.api_key_id,
            compat_dialect: request.compat_dialect,
            sdk_name: request.sdk_name,
            sdk_version: request.sdk_version,
            rejected: request.rejected,
            otlp: request.otlp,
        };
        let normalized = normalize(&ingest, &self.ctx).ok()?;
        Some(FrameSpans {
            received_at: source.received_at().micros(),
            spans: normalized
                .spans
                .iter()
                .map(|span| {
                    (
                        *span.core.trace_id.as_bytes(),
                        *span.core.span_id.as_bytes(),
                        span.core.user_id.clone(),
                        span.core.session_id.clone(),
                    )
                })
                .collect(),
        })
    }

    /// When the facts of a `FactBatch` frame of the deleted project arrived.
    fn record_facts(&self, source: &WalFrame) {
        let Ok(payload) = source.payload() else {
            return;
        };
        let Ok(Record::FactBatch(batch)) = Record::decode(RecordType::FactBatch, &payload) else {
            return;
        };
        if Some(batch.project_id) != self.deleted.map(project_id) {
            return;
        }
        let mut ledger = self.shared.ledger();
        for fact in &batch.facts {
            ledger
                .fact_arrivals
                .insert(fact.fact_id.to_bytes(), source.received_at().micros());
        }
    }

    /// Contract 5.3 and 4.7: the WAL keeps the frame that an admission relied on until the deciding frame is
    /// checkpointed, so that a restart decides as this process did.
    fn check_retained(&self) {
        let watermark = self.wal.watermark().map_or(0, |w| w.last_lsn().get());
        let Some(floor) = self.wal.segments().first().map(|s| s.first_lsn.get()) else {
            return;
        };
        let mut ledger = self.shared.ledger();
        ledger.admitted.relying = ledger.admitted.relying.split_off(&(watermark + 1));
        let lost = ledger
            .admitted
            .relying
            .iter()
            .find(|(_, relied)| **relied < floor)
            .map(|(lsn, relied)| (*lsn, *relied));
        if let Some((lsn, relied)) = lost {
            ledger.violate(Violation::DerivedMismatch(format!(
                "the WAL starts at frame {floor} and no longer holds frame {relied}, which the admission \
                 of frame {lsn} relied on; frame {lsn} is not checkpointed"
            )));
        }
    }
}

impl StageBHook for Observed {
    fn applied(&self, source: &WalFrame, frame: &NormalizedFrame, admission: &Admission) {
        let lsn = source.lsn().get();
        let of_deleted = self.deleted.is_some() && self.project_of(source) == self.deleted;
        let known = self.shared.ledger().admitted.frames.contains_key(&lsn);
        let spans = if known { None } else { self.spans(source) };
        let deletable = (of_deleted && !self.shared.ledger().deletable.contains_key(&lsn))
            .then(|| self.deletable(source))
            .flatten();
        let floor = self
            .wal
            .segments()
            .first()
            .map_or(lsn, |segment| segment.first_lsn.get());
        {
            let mut ledger = self.shared.ledger();
            if let Some(spans) = spans {
                ledger.admitted.frames.insert(lsn, spans);
            }
            if let Some(frame) = deletable {
                for (trace, span, ..) in &frame.spans {
                    ledger
                        .copies
                        .entry((*trace, *span))
                        .or_default()
                        .insert(lsn);
                }
                ledger.deletable.insert(lsn, frame);
            }
            ledger.stats.spans_deleted += u64::from(admission.deleted);
            ledger.stats.ops_deleted += u64::from(admission.deleted_ops);
            if lsn > self.watermark.get() {
                ledger
                    .admitted
                    .admissions
                    .entry((lsn, floor))
                    .or_default()
                    .extend(admission.stage_a_ordinals.iter().copied());
                if let Some(relied) = admission.relies_on {
                    ledger.admitted.relying.insert(lsn, relied.get());
                }
                // Contract 5.3: a kept span flagged on an earlier frame's support lowers `relies_on` to that
                // LSN, which check_retained then holds the WAL to.
                let support = frame
                    .spans
                    .iter()
                    .any(|span| span.core.usage_aggregate)
                    .then(|| {
                        system_oracle::flag_support(
                            &ledger.admitted.frames,
                            floor,
                            lsn,
                            &admission.stage_a_ordinals,
                        )
                    })
                    .flatten();
                if let Some(support) = support
                    && admission.relies_on.is_none_or(|at| at.get() > support)
                {
                    ledger.violate(Violation::DerivedMismatch(format!(
                        "frame {lsn} keeps a span that the usage in frame {support} flags, and its admission \
                         relies on no frame that early, so garbage collection may remove frame {support} \
                         before frame {lsn} is checkpointed"
                    )));
                }
                // I2: the pairs of a sealed batch hold the same spans whether they land before or after a
                // restart, so this admission keeps and flags what the process before it did. A restart that
                // knows more deletions drops more of the deleted project (contract 5.3), which I9 checks.
                let kept: Vec<(u32, bool)> = admission
                    .stage_a_ordinals
                    .iter()
                    .copied()
                    .zip(frame.spans.iter().map(|span| span.core.usage_aggregate))
                    .collect();
                match ledger.admitted.kept.get(&lsn) {
                    _ if of_deleted => {}
                    Some(before) if lsn <= self.sealed_through && *before != kept => {
                        ledger.violate(Violation::DerivedMismatch(format!(
                            "after a restart stage B keeps or flags other spans of frame {lsn}, which a                              sealed batch holds, than the process before it did"
                        )));
                    }
                    Some(_) => {}
                    None => {
                        ledger.admitted.kept.insert(lsn, kept);
                    }
                }
            }
        }
        self.check_retained();
        self.inner.applied(source, frame, admission);
    }

    fn passed(&self, source: &WalFrame) {
        if self.deleted.is_some() && source.record_type() == RecordType::FactBatch {
            self.record_facts(source);
        }
        self.check_retained();
        self.inner.passed(source);
    }

    fn unshippable(&self, source: &WalFrame, project: Option<ProjectId>, facts: bool) {
        self.inner.unshippable(source, project, facts);
    }

    fn floor(&self) -> Option<Lsn> {
        self.inner.floor()
    }
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
                    // A span of the deleted project that a deletion selects may be gone.
                    let deleted = Some(request.project) == shared.plan.deleted();
                    let missing = request.spans.iter().any(|span| {
                        span.trace == trace
                            && !returned.contains(&span.span)
                            && !(deleted
                                && ledger.deleted_span(&(request.project, span.trace, span.span)))
                    });
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
                let mut ledger = shared.ledger();
                let deleted = Some(request.project) == shared.plan.deleted()
                    && request
                        .spans
                        .iter()
                        .filter(|span| span.trace == trace)
                        .all(|span| ledger.deleted_span(&(request.project, span.trace, span.span)));
                if !deleted {
                    ledger.violate(Violation::SilentMiss {
                        read: "not_found",
                        request: index,
                    });
                }
                drop(ledger);
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
    let project = pick(ctx.rng.as_ref(), ctx.shared.plan.all_projects() as u64) as usize;
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
        let mut generated = Vec::new();
        for _ in 0..burst {
            sent += 1;
            let earlier = ctx.shared.ledger().requests.len() as u64;
            let repeat = plan.mask.has(Dim::ClientRetries) && earlier > 0 && pick(rng, 6) == 0;
            let index = if repeat {
                ctx.shared.ledger().repeats += 1;
                pick(rng, earlier) as usize
            } else {
                if plan.deleted().is_some() {
                    state.named = ctx
                        .shared
                        .ledger()
                        .deletions
                        .iter()
                        .filter_map(|d| match &d.known.tombstone.selector {
                            TombstoneSelector::User { user_id, issuer } => {
                                Some((user_id.to_string(), issuer.as_deref().map(str::to_owned)))
                            }
                            _ => None,
                        })
                        .collect();
                }
                let Some(request) = generate(plan, rng, &mut state) else {
                    continue;
                };
                let mut ledger = ctx.shared.ledger();
                // A span sent again makes the derived tables depend on what each restart remembered.
                ledger.repeats += u64::from(request.partial_repeat);
                ledger.requests.push(request);
                ledger.requests.len() - 1
            };
            generated.push((!repeat).then_some(index));
            waiting.push(exchange(&ctx, index, repeat));
        }
        let acked = futures::future::join_all(waiting).await;
        // The spans of the new requests that were acknowledged: stage B has seen them.
        for (index, _) in generated.iter().zip(&acked).filter(|(_, acked)| **acked) {
            let Some(index) = *index else {
                continue;
            };
            let request = ctx.shared.ledger().requests[index].clone();
            for sent in state.spans.iter_mut() {
                sent.acked |= sent.project == request.project
                    && request
                        .spans
                        .iter()
                        .any(|span| span.trace == sent.info.trace && span.span == sent.info.span);
            }
        }
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
    /// The span keys of the frames above the watermark that the last recovery found.
    unshipped: BTreeSet<McpKey>,
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
                self.unshipped = keys_above(&log, watermark);
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
        let project_of = |id: [u8; 16]| {
            (0..self.plan().all_projects()).find(|&p| project_id(p).to_bytes() == id)
        };
        // A restart that knows more deletions drops more of the deleted project's rows (contract 5.3), so its
        // tables do not agree exactly; I9 checks it instead.
        let deleted = self.plan().deleted();
        let skip = deleted.map(|p| project_id(p).to_bytes());
        let mut core_rows = 0u64;
        let mut core_roots: BTreeSet<(u64, [u8; 8])> = BTreeSet::new();
        let mut core_root_keys: BTreeSet<SpanKey> = BTreeSet::new();
        let salted = self.plan().salt_fail_ppm.is_some();
        let (mut mcp_ops, mut mcp_errors, mut mcp_faults, mut mcp_proxy, mut mcp_subjects) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        // The span tables, and the two plain MCP tables, which hold a row for the spans that are MCP
        // operations (`mcp_content` for those whose content was captured).
        for table in [
            TableId::SpanCore,
            TableId::SpanPayload,
            TableId::McpOps,
            TableId::McpContent,
        ] {
            let of_mcp = matches!(table, TableId::McpOps | TableId::McpContent);
            let expects = |ledger: &Ledger, key: &SpanKey| {
                let op = ledger
                    .mcp
                    .get(&(project_id(key.0).to_bytes(), key.1, key.2));
                match table {
                    TableId::McpOps => op.is_some(),
                    TableId::McpContent => op.is_some_and(|op| op.content.is_some()),
                    _ => true,
                }
            };
            let name = table.name();
            let mut by_frame: BTreeSet<(u64, [u8; 8])> = BTreeSet::new();
            let mut by_key: BTreeMap<SpanKey, u32> = BTreeMap::new();
            for part in model.parts(table) {
                let block = &part.block;
                // Contracts 3.5, 9.3 and 9.4: no row holds what a span carried under a key that must never be
                // stored, in its attributes or its events.
                if system_oracle::holds_text(block, PLANTED.as_bytes()) {
                    found.push(Violation::NeverStored(format!(
                        "a block of {name} holds a value that a span carried under a key that must never be stored"
                    )));
                }
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
                    if sent == 0 || !expects(&ledger, &key) {
                        found.push(Violation::UnknownRow { table: name });
                    }
                    // I8, and the wire contract of 9.1: the row is the exchange the client simulated.
                    if table == TableId::McpOps
                        && let Some(op) =
                            ledger
                                .mcp
                                .get(&(project_id(key.0).to_bytes(), key.1, key.2))
                    {
                        let salt = salted.then(|| project_salt(project_id(key.0)));
                        match system_oracle::mcp_row_matches(block, row, op, salt.as_ref()) {
                            None => found.push(Violation::UnknownRow { table: name }),
                            Some(diff) => {
                                if !diff.classification.is_empty() {
                                    found.push(Violation::Classification(format!(
                                        "the mcp_ops row of an operation of frame {lsn} differs from the \
                                         shared classifier in {:?}: expected {:?}",
                                        diff.classification, op.outcome
                                    )));
                                }
                                if !diff.fields.is_empty() {
                                    found.push(Violation::Aggregate(format!(
                                        "the mcp_ops row of an operation of frame {lsn} differs from the \
                                         exchange in {:?}",
                                        diff.fields
                                    )));
                                }
                            }
                        }
                        mcp_ops += 1;
                        mcp_errors += u64::from(op.outcome.error_class != ErrorClass::None);
                        mcp_faults += u64::from(op.outcome.caller_fault);
                        mcp_proxy += u64::from(matches!(
                            op.capture_source,
                            CaptureSource::ProxyStdio | CaptureSource::ProxyHttp
                        ));
                        mcp_subjects += u64::from(salted && op.subject.is_some());
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
                    if table == TableId::SpanCore && Some(key.0) != deleted {
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
                let of_deleted = Some(request.project) == deleted;
                for span in request.spans.iter() {
                    let key = (request.project, span.trace, span.span);
                    if of_mcp && !expects(&ledger, &key) {
                        continue;
                    }
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
                    if by_key.contains_key(&key) || durable || parked() {
                        continue;
                    }
                    if !of_deleted {
                        found.push(Violation::AckedSpanLost {
                            table: name,
                            request: index,
                        });
                        continue;
                    }
                    // I9: of the deleted project only what a deletion selects is gone.
                    let selected = if of_mcp {
                        ledger.deleted_op(&key)
                    } else {
                        ledger.deleted_span(&key)
                    };
                    if !selected {
                        found.push(Violation::Deletion(format!(
                            "a span of request {index} that no deletion selects is in no row of {name}, no \
                             durable frame above the watermark and no quarantine"
                        )));
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
                    if seen.contains(id) || facts_in_log.contains(id) || parked() {
                        continue;
                    }
                    let project = ledger.fact_writes[*write].project;
                    if Some(project) != deleted {
                        found.push(Violation::AckedFactLost {
                            table: row.table.name(),
                            write: *write,
                        });
                    } else if !ledger.deleted_fact(project, id, &row.trace, &row.session) {
                        found.push(Violation::Deletion(format!(
                            "a fact of write {write} that no deletion selects is in no row of {}, no durable \
                             frame above the watermark and no quarantine",
                            row.table.name()
                        )));
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

        ledger.stats.mcp_ops = mcp_ops;
        ledger.stats.mcp_errors = mcp_errors;
        ledger.stats.mcp_caller_faults = mcp_faults;
        ledger.stats.mcp_proxy_rows = mcp_proxy;
        ledger.stats.mcp_subjects = mcp_subjects;

        // I7: once everything has landed, every rollup equals the reference aggregation over `span_core`, and
        // the MCP aggregating tables equal the reference over `mcp_ops`, per capture source.
        if quiescent && pair_directories == 0 {
            let (keys, wrong) = system_oracle::check_aggregates(model, &ledger.admitted, skip);
            ledger.stats.rollup_keys = keys;
            found.extend(wrong.into_iter().map(Violation::Aggregate));
            let (keys, wrong) = system_oracle::check_mcp(model, &ledger.mcp, skip);
            ledger.stats.mcp_keys = keys;
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
                    if uuid_at(block, "project_id", row) != skip {
                        stats_spans += u64_at(block, "span_count", row).unwrap_or(0);
                    }
                }
            }
            // A quarantined pair is missing from its table alone; without one the tables must agree exactly,
            // also about repeats: a restart drops the repeats that the process before it dropped.
            let exact = directories == 0;
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
                    if Some(request.project) == deleted {
                        continue;
                    }
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
        found.extend(deletions_after_completion(model, &ledger, deleted));
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
        // The frames above the last batch that an earlier incarnation sealed were admitted and never shipped:
        // this incarnation admits them afresh, on the prefix the WAL holds now, and only that admission counts.
        let sealed_through = report
            .unchecked_seals
            .iter()
            .map(|seal| seal.last_lsn.get())
            .max()
            .unwrap_or(0)
            .max(watermark.get());
        {
            let admitted = &mut self.shared.ledger().admitted;
            admitted
                .admissions
                .retain(|(lsn, _), _| *lsn <= sealed_through);
            admitted.kept.retain(|lsn, _| *lsn <= sealed_through);
            admitted.relying.retain(|lsn, _| *lsn <= sealed_through);
        }
        let salts: Arc<dyn ProjectSalts> = match plan.salt_fail_ppm {
            Some(fail_ppm) => Arc::new(SimSalts {
                fail_ppm,
                rng: self.sim.rng(&format!("system-sim/salts-{}", self.boots)),
                shared: Arc::clone(&self.shared),
            }),
            None => Arc::new(NoSalts),
        };
        let deletions: Arc<dyn DeletionRequests> = match plan.deletion_fail_ppm {
            Some(fail_ppm) => Arc::new(SimDeletions {
                fail_ppm,
                rng: self
                    .sim
                    .rng(&format!("system-sim/deletion-reads-{}", self.boots)),
                shared: Arc::clone(&self.shared),
            }),
            None => Arc::new(NoDeletions),
        };
        let hooks = Hooks {
            barriers: barriers.clone(),
            recent: Arc::new(Observed {
                inner: hook,
                wal: wal.clone(),
                shared: Arc::clone(&self.shared),
                watermark,
                sealed_through,
                ctx: NormCtx::new(prices, PayloadCaps::DEFAULT),
                deleted: plan.deleted(),
            }),
            salts,
            deletions,
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
        if self.plan().deleted().is_some() {
            // Requests that completed while the node was down: the executor ran without it, after the inserts
            // it had in flight, so no tombstone of them is in its log, and the node must read them before it
            // ships what it never shipped.
            let deletions = self
                .sim
                .rng(&format!("system-sim/while-down-{}", self.boots));
            let deletions = deletions.as_ref();
            // Always when a frame the node never shipped holds an operation of the project that carries a
            // subject, so that such a request can name it.
            let subjects = self.plan().deleted().is_some_and(|deleted| {
                let project = project_id(deleted).to_bytes();
                self.shared.ledger().mcp.iter().any(|(key, sent)| {
                    key.0 == project && self.unshipped.contains(key) && sent.subject.is_some()
                })
            });
            if subjects || coin(deletions) {
                let done =
                    (self.sim.elapsed().as_micros() as u64).max(self.model.in_flight_until());
                let at = host.env.clock.wall_now();
                for _ in 0..1 + pick(deletions, 2) {
                    make_request(
                        &self.shared,
                        deletions,
                        at,
                        Some(done),
                        Some(&self.unshipped),
                    );
                }
            }
        }
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
        if self.plan().deleted().is_some() {
            let ctx = DeleterCtx {
                shared: Arc::clone(&self.shared),
                wal: wal.clone(),
                model: self.model.clone(),
                env: host.env.clone(),
                rng: self.sim.rng(&format!("system-sim/deleter-{}", self.boots)),
            };
            drop(host.env.spawn("deleter", deleter(ctx)));
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
        // A window in which every directory sync fails: a segment that is created or removed in it is not
        // durably so. Drawn last, so that a mask without the dimension draws what it drew before.
        let dir_window = (has(Dim::DirSyncErrors) && coin(rng)).then(|| {
            (
                pick(rng, run_for),
                Duration::from_micros(1_000 + pick(rng, 400_000)),
            )
        });
        let at_crash_point = {
            let mut dir_start = match dir_window {
                Some((at, _)) => host.env.clock.sleep(Duration::from_micros(at)),
                None => Box::pin(futures::future::pending()),
            }
            .fuse();
            let mut dir_end: futures::future::Fuse<futures::future::BoxFuture<'_, ()>> =
                futures::future::Fuse::terminated();
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
                    () = dir_start => {
                        if let Some((_, length)) = dir_window {
                            self.event("dir_sync.fail", &[length.as_micros() as u64]);
                            self.shared.ledger().stats.dir_sync_windows += 1;
                            self.disk.fail_dir_syncs(true);
                            dir_end = host.env.clock.sleep(length).fuse();
                        }
                    }
                    () = dir_end => self.disk.fail_dir_syncs(false),
                    () = timer => break false,
                }
            }
        };
        self.disk.fail_dir_syncs(false);
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
        if self.plan().deleted().is_some() {
            // The node timer appends the tombstone of every open request before the drain.
            let now = self.sim.elapsed().as_micros() as u64;
            node_timer(&self.shared, &wal, &self.model, now).await;
        }
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
        complete_passed(
            &self.shared,
            &self.model,
            watermark(&wal),
            self.sim.elapsed().as_micros() as u64,
        );
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
        reads::check(&self.shared, node, &self.model, self.plan().projects).await;
    }
}

/// I9 on the write path: no row of the deleted project that a completed deletion request selects lands after the
/// request completed. Before that the executor deletes it; a row that lands later stays, and only stage B keeps
/// it out: by the tombstone of the frames after it, and by the requests it reads before it ships.
fn deletions_after_completion(
    model: &ChModel,
    ledger: &Ledger,
    deleted: Option<usize>,
) -> Vec<Violation> {
    let mut found = Vec::new();
    let completed: Vec<(u64, &Tombstone)> = ledger
        .deletions
        .iter()
        .filter_map(|d| d.completed_at.map(|at| (at, &d.known.tombstone)))
        .collect();
    let (Some(deleted), Some(earliest)) = (deleted, completed.iter().map(|(at, _)| *at).min())
    else {
        return found;
    };
    let project = project_id(deleted).to_bytes();
    for table in [
        TableId::SpanCore,
        TableId::SpanPayload,
        TableId::TraceHeads,
        TableId::McpOps,
        TableId::McpContent,
    ] {
        let of_mcp = matches!(table, TableId::McpOps | TableId::McpContent);
        for part in model.parts(table) {
            if part.landed_at <= earliest {
                continue;
            }
            let block = &part.block;
            for row in 0..block.rows {
                let cells = (|| {
                    let lsn = u64_at(block, "lsn", row)?;
                    let trace: [u8; 16] = bytes_at(block, "trace_id", row)?.try_into().ok()?;
                    let span: [u8; 8] = bytes_at(block, "span_id", row)?.try_into().ok()?;
                    (uuid_at(block, "project_id", row)? == project).then_some((lsn, trace, span))
                })();
                let Some((lsn, trace, span)) = cells else {
                    continue;
                };
                let Some(frame) = ledger.deletable.get(&lsn) else {
                    continue;
                };
                let Some((_, _, user, session)) = frame
                    .spans
                    .iter()
                    .find(|(t, s, ..)| *t == trace && *s == span)
                else {
                    continue;
                };
                let op = of_mcp
                    .then(|| ledger.mcp.get(&(project, trace, span)))
                    .flatten();
                let selected = completed.iter().any(|(at, deletion)| {
                    part.landed_at > *at
                        && (system_oracle::deletion_selects_span(
                            deletion,
                            &project,
                            frame.received_at,
                            &trace,
                            user,
                            session,
                        ) || op.is_some_and(|op| {
                            system_oracle::deletion_names_op(
                                deletion,
                                &project,
                                frame.received_at,
                                op,
                            )
                        }))
                });
                if selected {
                    found.push(Violation::Deletion(format!(
                        "a row of {} of frame {lsn} that a completed deletion request selects landed after the \
                         request completed",
                        table.name()
                    )));
                }
            }
        }
    }
    for table in system_oracle::FACT_TABLES {
        for part in model.parts(table) {
            if part.landed_at <= earliest {
                continue;
            }
            let block = &part.block;
            for row in 0..block.rows {
                let cells = (|| {
                    (uuid_at(block, "project_id", row)? == project).then_some(())?;
                    let trace: [u8; 16] = bytes_at(block, "trace_id", row)?.try_into().ok()?;
                    let session = bytes_at(block, "session_id", row)?;
                    Some((trace, session, i64_at(block, "received_at", row)?))
                })();
                let Some((trace, session, received_at)) = cells else {
                    continue;
                };
                let selected = completed.iter().any(|(at, deletion)| {
                    part.landed_at > *at
                        && system_oracle::deletion_selects_fact(
                            deletion,
                            &project,
                            received_at,
                            &trace,
                            session,
                        )
                });
                if selected {
                    found.push(Violation::Deletion(format!(
                        "a row of {} that a completed deletion request selects landed after the request \
                         completed",
                        table.name()
                    )));
                }
            }
        }
    }
    found
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
    let keys = (0..plan.all_projects())
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
                unshipped: BTreeSet::new(),
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
