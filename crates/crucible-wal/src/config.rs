//! WAL settings (contract sections 4.5 and 4.7).

use std::time::Duration;

/// Default `wal.segment_bytes`.
pub const DEFAULT_SEGMENT_BYTES: u64 = 128 << 20;

/// Default `wal.max_bytes`.
pub const DEFAULT_MAX_BYTES: u64 = 16 << 30;

/// Default `wal.min_free_bytes`.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 1 << 30;

/// Default `wal.replay_retention`.
pub const DEFAULT_REPLAY_RETENTION: Duration = Duration::from_secs(15 * 60);

/// The hard cap on `wal.replay_retention`.
pub const MAX_REPLAY_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// The most bytes one commit group holds. A larger single frame forms its own group.
pub const GROUP_BYTES: usize = 8 << 20;

/// `wal.durability`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Durability {
    /// Acknowledge only after `sync_data` for the frame's group returned `Ok`. The only mode under which no
    /// acknowledged write is lost.
    #[default]
    Group,
    /// UNSAFE, benchmarks only: acknowledge after the append and sync on a timer.
    Interval(Duration),
    /// UNSAFE, benchmarks only: acknowledge after the append and never sync frames.
    None,
}

impl Durability {
    /// Whether an acknowledged frame can be lost in a crash. Health output must then say `durability: unsafe`.
    pub const fn is_unsafe(self) -> bool {
        !matches!(self, Self::Group)
    }
}

/// A seeded defect. The simulation suite switches each one on and must catch it.
#[cfg(feature = "test-mutants")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// Complete a group's waiters before its `sync_data`.
    AckBeforeSync,
    /// Let garbage collection remove segments above the checkpoint watermark.
    GcAboveCheckpoint,
    /// Resume appending after a torn tail without zeroing it.
    KeepTornTail,
}

/// The settings of one WAL directory.
#[derive(Clone, Debug)]
pub struct WalConfig {
    /// Size every segment is preallocated to.
    pub segment_bytes: u64,
    /// Cap on total segment bytes; the high-water mark is 90% of it.
    pub max_bytes: u64,
    /// The free-disk floor below which ingest is refused.
    pub min_free_bytes: u64,
    /// When a frame is acknowledged.
    pub durability: Durability,
    /// How long checkpointed segments are kept for replay; clamped to [`MAX_REPLAY_RETENTION`]. Retained
    /// segments also never exceed 25% of `max_bytes` and are evicted first above the high-water mark.
    pub replay_retention: Duration,
    /// The seeded defect to run with.
    #[cfg(feature = "test-mutants")]
    pub mutant: Option<Mutant>,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            max_bytes: DEFAULT_MAX_BYTES,
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
            durability: Durability::Group,
            replay_retention: DEFAULT_REPLAY_RETENTION,
            #[cfg(feature = "test-mutants")]
            mutant: None,
        }
    }
}

impl WalConfig {
    /// The total segment bytes at which ingest is refused: 90% of `max_bytes`.
    pub const fn high_water_bytes(&self) -> u64 {
        self.max_bytes / 10 * 9
    }

    /// The byte budget of replay retention: 25% of `max_bytes`.
    pub const fn replay_budget_bytes(&self) -> u64 {
        self.max_bytes / 4
    }

    pub(crate) fn replay_retention_micros(&self) -> i64 {
        let capped = self.replay_retention.min(MAX_REPLAY_RETENTION);
        i64::try_from(capped.as_micros()).unwrap_or(i64::MAX)
    }

    #[cfg(feature = "test-mutants")]
    fn has(&self, mutant: Mutant) -> bool {
        self.mutant == Some(mutant)
    }

    pub(crate) fn ack_before_sync(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.has(Mutant::AckBeforeSync);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    pub(crate) fn gc_above_checkpoint(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.has(Mutant::GcAboveCheckpoint);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    pub(crate) fn keep_torn_tail(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.has(Mutant::KeepTornTail);
        #[cfg(not(feature = "test-mutants"))]
        false
    }
}
