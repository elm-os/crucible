//! The settings of the lifecycle jobs (contract 13.2 and 13.3).

use std::time::Duration;

use crucible_schema::Granularity;

/// The default of `ingest.max_event_age` (contract 1.2): 400 days.
pub const DEFAULT_MAX_EVENT_AGE: Duration = Duration::from_secs(400 * 86_400);

/// A seeded defect. The simulation suite switches each one on and must catch it.
///
/// Without the cargo feature `test-mutants` this type has no value, so no defect can be switched on. Only
/// `crucible-sim` enables the feature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// The deletion executor issues its deletes as soon as a request is logged, without waiting for every
    /// live node's checkpoint to pass its tombstone.
    #[cfg(feature = "test-mutants")]
    SkipCheckpointWait,
    /// The mask scheduler never plans a mask application.
    #[cfg(feature = "test-mutants")]
    DropMaskCycle,
    /// An action is sent to ClickHouse without being recorded under the fencing token first.
    #[cfg(feature = "test-mutants")]
    DeleteWithoutFencing,
    /// A deletion by user reaches the MCP tables only through the traces its spans resolve to, never by the
    /// operations' own `user_id` and `user_subject_hash`.
    #[cfg(feature = "test-mutants")]
    McpByTraceOnly,
    /// A project deletion drops every partition it reaches, whatever other projects hold rows there.
    #[cfg(feature = "test-mutants")]
    DropSharedPartitions,
    /// A project deletion drops a partition in which every project with rows is deleted without the event-age
    /// bound, although a live project can still write rows there.
    #[cfg(feature = "test-mutants")]
    DropWithinEventAge,
    /// The purge sweep never runs, so rows of a deleted project that land after its purge stay.
    #[cfg(feature = "test-mutants")]
    SweepDisabled,
    /// The purge sweep removes the rows it finds without the project predicate: it drops their partition.
    #[cfg(feature = "test-mutants")]
    SweepWithoutProject,
}

/// The settings of the lifecycle jobs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LifecycleConfig {
    /// How often the retention executor plans (contract 13.2: hourly).
    pub retention_every: Duration,
    /// `lifecycle.mask_cycle` (contract 13.2: 6 h): masks are applied at most once per partition per cycle.
    pub mask_cycle: Duration,
    /// How often the deletion executor runs (contract 13.3: every 30 s). It also issues and polls the
    /// actions the other two jobs planned.
    pub deletion_every: Duration,
    /// How often a node heartbeats, refreshes its pending-deletion set and records tombstones (contract
    /// 13.3: at most every 5 s).
    pub node_every: Duration,
    /// A node is live while its heartbeat is this recent (contract 13.3: 60 s).
    pub live_within: Duration,
    /// The partition granularity of the deployment.
    pub granularity: Granularity,
    /// How many definite failures an action survives before it is given up as `failed`.
    pub max_attempts: u32,
    /// How far a node's clock may be from the database clock. The retention executor computes its cut-offs this
    /// far behind the database clock, so that it removes no row that a node with a slower clock still serves, and
    /// a project deletion takes it into the instant before which no project can write, the end of what it may drop.
    /// While a live node's clock is more than half of it off, a project deletion drops nothing.
    pub clock_margin: Duration,
    /// Whether the deployment stores media (an object store is configured): a completed deletion request
    /// then reports the media place `done`, otherwise `not_present`.
    pub media: bool,
    /// `ingest.max_event_age` (contract 1.2): ingest takes no span that starts longer than this before it arrives,
    /// so a project deletion may drop a partition only where no project can write any more. The binary passes the
    /// value its ingest runs with.
    pub max_event_age: Duration,
    /// How often the purge sweep looks again for rows of each project deleted in the last
    /// [`SWEEP_RECENT`](crate::SWEEP_RECENT); older deletions are looked at every
    /// [`SWEEP_OLD_PERIODS`](crate::SWEEP_OLD_PERIODS) of these.
    pub sweep_every: Duration,
    pub(crate) mutant: Option<Mutant>,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            retention_every: Duration::from_secs(3_600),
            mask_cycle: Duration::from_secs(6 * 3_600),
            deletion_every: Duration::from_secs(30),
            node_every: Duration::from_secs(5),
            live_within: crucible_control::LIVE_WITHIN,
            granularity: Granularity::Day,
            max_attempts: 8,
            clock_margin: Duration::from_secs(3_600),
            media: false,
            max_event_age: DEFAULT_MAX_EVENT_AGE,
            sweep_every: Duration::from_secs(3_600),
            mutant: None,
        }
    }
}

impl LifecycleConfig {
    /// The same settings with a seeded defect.
    #[cfg(feature = "test-mutants")]
    #[must_use]
    pub const fn with_mutant(mut self, mutant: Mutant) -> Self {
        self.mutant = Some(mutant);
        self
    }

    /// Whether the checkpoint wait of the deletion executor is skipped (a seeded defect).
    pub(crate) fn skips_checkpoint_wait(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::SkipCheckpointWait);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether the mask scheduler plans nothing (a seeded defect).
    pub(crate) fn drops_mask_cycle(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::DropMaskCycle);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether actions are sent without their fenced record (a seeded defect).
    pub(crate) fn issues_unfenced(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::DeleteWithoutFencing);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether a deletion by user skips the MCP rows of its own columns (a seeded defect).
    pub(crate) fn skips_mcp_identity(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::McpByTraceOnly);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether a project deletion drops partitions that other projects share (a seeded defect).
    pub(crate) fn drops_shared_partitions(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::DropSharedPartitions);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether a project deletion drops partitions without the event-age bound (a seeded defect).
    pub(crate) fn drops_within_event_age(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::DropWithinEventAge);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether the purge sweep does nothing (a seeded defect).
    pub(crate) fn skips_sweep(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::SweepDisabled);
        #[cfg(not(feature = "test-mutants"))]
        false
    }

    /// Whether the purge sweep drops the partitions it finds rows in (a seeded defect).
    pub(crate) fn sweeps_without_project(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::SweepWithoutProject);
        #[cfg(not(feature = "test-mutants"))]
        false
    }
}
