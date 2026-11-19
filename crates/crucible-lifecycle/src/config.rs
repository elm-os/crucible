//! The settings of the lifecycle jobs (contract 13.2 and 13.3).

use std::time::Duration;

use crucible_schema::Granularity;

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
    /// How far behind the database clock the retention executor computes its cut-offs, so that it never
    /// removes a row that a node with a slower clock still serves.
    pub clock_margin: Duration,
    pub(crate) mutant: Option<Mutant>,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            retention_every: Duration::from_secs(3_600),
            mask_cycle: Duration::from_secs(6 * 3_600),
            deletion_every: Duration::from_secs(30),
            node_every: Duration::from_secs(5),
            live_within: Duration::from_secs(60),
            granularity: Granularity::Day,
            max_attempts: 8,
            clock_margin: Duration::from_secs(3_600),
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
}
