//! Shipper settings (contract sections 5.1, 5.4, 5.5 and 5.6).

use std::time::Duration;

use crucible_chfmt::{Compression, MAX_BLOCK_ROWS};
use crucible_env::StoreFlavour;
use crucible_normalize::{DUPLICATE_WINDOW_KEYS, FIRST_SEEN_TRACES};
use crucible_records::PayloadCaps;
use crucible_schema::Granularity;

/// How long a probe that read absent authorizes a resend (contract 5.5).
pub const PROBE_VALID_FOR: Duration = Duration::from_secs(60);

/// Identical data errors after which a pair is quarantined (contract 5.6).
pub const POISON_LIMIT: u16 = 3;

/// A seeded defect. The simulation suite switches each one on and must catch it.
///
/// Without the cargo feature `test-mutants` this type has no value, so [`ShipperConfig::mutant`] is always `None`
/// and no defect can be switched on. Only `crucible-sim` enables the feature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// Retry after an unknown outcome without the existence probe.
    #[cfg(feature = "test-mutants")]
    SkipProbeAfterUnknown,
    /// Send inserts without waiting for the `BatchSeal` frame to be durable.
    #[cfg(feature = "test-mutants")]
    SendBeforeSeal,
    /// Give every attempt a new deduplication identity.
    #[cfg(feature = "test-mutants")]
    DropToken,
    /// Append the checkpoint before the batch's pairs are resolved.
    #[cfg(feature = "test-mutants")]
    CheckpointBeforeResolution,
    /// Quarantine a pair that is in doubt without a probe.
    #[cfg(feature = "test-mutants")]
    QuarantineWithoutProbe,
    /// Let a rejection clear the doubt an earlier unknown outcome left (the narrow reading of contract 5.5
    /// that the ingest model refuted).
    #[cfg(feature = "test-mutants")]
    RejectionClearsDoubt,
    /// Count one row of every rollup block twice.
    #[cfg(feature = "test-mutants")]
    DoubleCountRollup,
    /// Count a pair of a fact table as landed when its insert is retried, without sending it again.
    #[cfg(feature = "test-mutants")]
    DropFactOnRetry,
    /// Quarantine a frame that does not normalize without copying it to the quarantine root.
    #[cfg(feature = "test-mutants")]
    FrameQuarantineWithoutCopy,
    /// Count the tokens and cost of a usage aggregate (a span with a descendant that carries usage) in the sums.
    #[cfg(feature = "test-mutants")]
    CountAggregateUsage,
    /// Collect garbage without regard for what the admission of an unchecked frame relied on.
    #[cfg(feature = "test-mutants")]
    CollectWhatFramesRelyOn,
    /// Report an invalid-params error on a server-side MCP row as the server's fault (a defect of the
    /// normalizer's projector, contract 9.2).
    #[cfg(feature = "test-mutants")]
    MisclassifyCallerFault,
    /// Give every `mcp_tool_rollup` partial one capture source, so that rows of different capture sources are
    /// summed (contract 9.5).
    #[cfg(feature = "test-mutants")]
    SumCaptureSources,
    /// Send the blocks of `mcp_tool_rollup` without their marker row (contract 5.5).
    #[cfg(feature = "test-mutants")]
    DropRollupMarker,
    /// Flag the usage aggregates of a frame after its exact repeats were dropped (a defect of stage B, contract
    /// 5.3).
    #[cfg(feature = "test-mutants")]
    MarkAfterDuplicateDrop,
}

/// A seeded fault of the normalizer, for the simulation suite: a share of the frames does not normalize, as a
/// frame that a binary cannot read would. The choice is a function of the frame's LSN and the salt alone, so
/// every replay of a frame decides the same (contract 5.6).
///
/// Without the cargo feature `test-mutants` this type cannot be built, so [`ShipperConfig::frame_fault`] is
/// always `None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameFault {
    salt: u64,
    ppm: u32,
}

impl FrameFault {
    /// A fault that fails `ppm` frames in a million, chosen by `salt`.
    #[cfg(feature = "test-mutants")]
    pub const fn new(salt: u64, ppm: u32) -> Self {
        Self { salt, ppm }
    }

    /// Whether the frame at `lsn` fails (SplitMix64 of the LSN and the salt).
    pub(crate) const fn fails(&self, lsn: u64) -> bool {
        let mut z = (lsn ^ self.salt).wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        z % 1_000_000 < self.ppm as u64
    }
}

/// Default `shipper.encoded_bytes` (default; revisit): several times the rows of the largest frame.
pub const DEFAULT_ENCODED_BYTES: u64 = 1 << 30;

/// The settings of one shipper.
#[derive(Clone, Debug)]
pub struct ShipperConfig {
    /// A batch seals this long after its first frame arrived.
    pub linger: Duration,
    /// The linger under pressure.
    pub max_linger: Duration,
    /// A batch seals at this many span rows.
    pub seal_rows: usize,
    /// The row limit under pressure. Never above the block limit.
    pub max_seal_rows: usize,
    /// A batch seals at this many raw frame bytes.
    pub seal_bytes: u64,
    /// An insert slower than this counts as pressure and widens the limits (default; revisit).
    pub pressure_latency: Duration,
    /// Batches in flight at once.
    pub max_batches_in_flight: usize,
    /// Inserts and probes in flight at once.
    pub connections: usize,
    /// Compression of insert bodies (`clickhouse.insert_compression`).
    pub compression: Compression,
    /// The store's flavour, for the per-insert settings.
    pub flavour: StoreFlavour,
    /// The partition granularity the tables were provisioned with.
    pub granularity: Granularity,
    /// The payload caps the normalizer applies.
    pub caps: PayloadCaps,
    /// How long a probe that read absent authorizes a resend.
    pub probe_valid_for: Duration,
    /// An insert that has not answered after this long has an unknown outcome. Above `max_execution_time`.
    pub insert_timeout: Duration,
    /// The smallest backoff.
    pub backoff_min: Duration,
    /// The largest backoff.
    pub backoff_max: Duration,
    /// Identical data errors after which a pair is quarantined.
    pub poison_limit: u16,
    /// How often the shipper looks for new durable frames when it found none.
    pub tail_poll: Duration,
    /// How often garbage collection runs between checkpoints, so that replay retention can elapse.
    pub gc_interval: Duration,
    /// Keys of the duplicate window.
    pub duplicate_window_keys: usize,
    /// Traces of the first-seen set.
    pub first_seen_traces: usize,
    /// `shipper.encoded_bytes`: the budget for the rows the shipper holds, from a frame's placement in a batch
    /// until the batch's pairs are resolved. Rows are charged at their estimated size ([`crate::value::Row`]),
    /// and the encoded blocks that replace them at the seal keep that charge. While the budget is exhausted
    /// the shipper reads no further frame: the open batch seals and the next frame waits for a batch in flight
    /// to resolve. Frames are never split, so one frame whose rows alone exceed the budget is taken by itself.
    /// A batch that recovery found keeps its recorded boundaries: it takes its next frame once the budget has
    /// any room or nothing else is held, so the rows held never exceed the budget by more than one frame.
    pub encoded_bytes: u64,
    /// A seeded normalizer fault; `None` outside the simulation suite.
    pub frame_fault: Option<FrameFault>,
    /// A seeded defect; `None` outside the simulation suite.
    pub mutant: Option<Mutant>,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self {
            linger: Duration::from_millis(500),
            max_linger: Duration::from_secs(2),
            seal_rows: 131_072,
            max_seal_rows: MAX_BLOCK_ROWS,
            seal_bytes: 128 << 20,
            pressure_latency: Duration::from_secs(2),
            max_batches_in_flight: 2,
            connections: 8,
            compression: Compression::Lz4,
            flavour: StoreFlavour::SingleNode,
            granularity: Granularity::Day,
            caps: PayloadCaps::DEFAULT,
            probe_valid_for: PROBE_VALID_FOR,
            insert_timeout: Duration::from_secs(90),
            backoff_min: Duration::from_millis(100),
            backoff_max: Duration::from_secs(30),
            poison_limit: POISON_LIMIT,
            tail_poll: Duration::from_millis(5),
            gc_interval: Duration::from_secs(1),
            duplicate_window_keys: DUPLICATE_WINDOW_KEYS,
            first_seen_traces: FIRST_SEEN_TRACES,
            encoded_bytes: DEFAULT_ENCODED_BYTES,
            frame_fault: None,
            mutant: None,
        }
    }
}

macro_rules! mutant_flags {
    ($($name:ident => $variant:ident,)+) => {
        impl ShipperConfig {$(
            pub(crate) const fn $name(&self) -> bool {
                #[cfg(feature = "test-mutants")]
                {
                    matches!(self.mutant, Some(Mutant::$variant))
                }
                #[cfg(not(feature = "test-mutants"))]
                {
                    false
                }
            }
        )+}
    };
}

mutant_flags! {
    skips_probe_after_unknown => SkipProbeAfterUnknown,
    sends_before_seal => SendBeforeSeal,
    drops_token => DropToken,
    checkpoints_before_resolution => CheckpointBeforeResolution,
    quarantines_without_probe => QuarantineWithoutProbe,
    rejection_clears_doubt => RejectionClearsDoubt,
    double_counts_rollups => DoubleCountRollup,
    drops_fact_on_retry => DropFactOnRetry,
    quarantines_frames_without_copy => FrameQuarantineWithoutCopy,
    counts_aggregate_usage => CountAggregateUsage,
    collects_what_frames_rely_on => CollectWhatFramesRelyOn,
    sums_capture_sources => SumCaptureSources,
    drops_rollup_marker => DropRollupMarker,
}

impl ShipperConfig {
    /// The seeded defect of the normalizer that this configuration switches on.
    pub(crate) const fn normalizer_mutant(&self) -> Option<crucible_normalize::Mutant> {
        #[cfg(feature = "test-mutants")]
        {
            match self.mutant {
                Some(Mutant::MisclassifyCallerFault) => {
                    Some(crucible_normalize::Mutant::MisclassifyCallerFault)
                }
                Some(Mutant::MarkAfterDuplicateDrop) => {
                    Some(crucible_normalize::Mutant::MarkAfterDuplicateDrop)
                }
                _ => None,
            }
        }
        #[cfg(not(feature = "test-mutants"))]
        {
            None
        }
    }
}
