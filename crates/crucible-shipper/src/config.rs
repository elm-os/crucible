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
}

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
}
