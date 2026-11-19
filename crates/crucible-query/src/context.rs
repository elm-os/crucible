//! What every read carries in and out: the caller's context, the response `meta` and the errors.

use crucible_env::QueryError;
use crucible_plan::{Caller, PlanError, RetentionFloors};
use crucible_recent::{Consistency, ReadState};
use crucible_records::{ProjectScope, TableId, Timestamp};
use crucible_schema::RetentionSettings;

/// Who reads, and under which retention. Every service method takes one, so no read can be made without the
/// project's retention settings: the plans are built clamped to them, and recent entries are filtered with the
/// same cut-off (contract 8.3 and 13.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadContext {
    pub(crate) scope: ProjectScope,
    pub(crate) retention: RetentionSettings,
    pub(crate) floors: RetentionFloors,
    pub(crate) caller: Caller,
}

impl ReadContext {
    /// A read of the scope's project, whose retention settings are `retention`, by a session or an agent.
    pub const fn new(scope: ProjectScope, retention: RetentionSettings, caller: Caller) -> Self {
        Self {
            scope,
            retention,
            floors: RetentionFloors {
                retention: None,
                payload: None,
                mcp_content: None,
            },
            caller,
        }
    }

    /// The same read that also never goes below `floors`: what earlier, shorter retention settings of the
    /// project had hidden (contract 13.1).
    #[must_use]
    pub const fn with_floors(mut self, floors: RetentionFloors) -> Self {
        self.floors = floors;
        self
    }

    /// The project scope.
    pub const fn scope(&self) -> ProjectScope {
        self.scope
    }
}

/// Why a response is partial (contract 8.6 and invariant I3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PartialReason {
    /// The store did not answer, or the read could touch an open index gap: spans dropped from the recent
    /// index during a store outage before the store served them.
    StoreUnavailable,
    /// The request deadline passed before every part answered.
    Deadline,
    /// The read could touch a span of a quarantined (table, partition) pair.
    Quarantined,
}

impl PartialReason {
    /// The wire name.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::StoreUnavailable => "store_unavailable",
            Self::Deadline => "deadline",
            Self::Quarantined => "quarantined",
        }
    }
}

/// The `meta` object of every telemetry list and detail response (contract 8.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    /// The wall-clock instant the response describes; for a list, the `as_of` its first page fixed.
    pub as_of: Timestamp,
    /// Whether rows may be missing for the reason given.
    pub partial: bool,
    /// Why, when `partial`. An unreachable store or an open gap takes precedence over a quarantine.
    pub partial_reason: Option<PartialReason>,
    /// `Strong` when the response reflects every span this node acknowledged before the request arrived.
    pub consistency: Consistency,
}

impl Meta {
    /// Whether the response is neither partial nor lagging: only then does "nothing found" mean "does not
    /// exist".
    pub(crate) fn is_whole(&self) -> bool {
        !self.partial && self.consistency == Consistency::Strong
    }

    /// The meta of a read that used `tables`: partial when the store was down, when the project has an open
    /// gap, or when it has rows in a quarantined pair of one of the tables.
    pub(crate) fn new(
        as_of: Timestamp,
        consistency: Consistency,
        state: ReadState,
        tables: &[TableId],
        store_down: bool,
    ) -> Self {
        let partial_reason = if store_down || state.gap {
            Some(PartialReason::StoreUnavailable)
        } else if state.quarantined(tables) {
            Some(PartialReason::Quarantined)
        } else {
            None
        };
        Self {
            as_of,
            partial: partial_reason.is_some(),
            partial_reason,
            consistency,
        }
    }
}

/// A failed read. Each variant is one problem code of contract 12.3.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum QueryServiceError {
    /// The entity does not exist, is not visible to the project, or has expired.
    #[error("not found")]
    NotFound,
    /// The cursor belongs to another project, trace or window.
    #[error("the cursor does not match the request")]
    CursorInvalid,
    /// A request parameter is out of range.
    #[error("invalid request: {0}")]
    Validation(&'static str),
    /// The store failed the read.
    #[error(transparent)]
    Store(#[from] QueryError),
    /// A defect: a plan that could not be built, or a row that does not follow its plan's output.
    #[error("internal error: {0}")]
    Internal(String),
}

impl QueryServiceError {
    /// The problem code of contract 12.3.
    pub const fn problem_code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::CursorInvalid => "cursor_invalid",
            Self::Validation(_) => "validation_failed",
            Self::Store(error) => error.problem_code(),
            Self::Internal(_) => "internal_error",
        }
    }
}

impl From<PlanError> for QueryServiceError {
    fn from(error: PlanError) -> Self {
        match error {
            PlanError::Limit => Self::Validation("limit"),
            // A filter condition the read cannot evaluate is the request's to change, not a defect.
            PlanError::UnsupportedFilter { field, .. } => Self::Validation(field),
            other => Self::Internal(other.to_string()),
        }
    }
}
