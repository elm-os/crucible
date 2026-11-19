//! `SqlWorkspaceStore` (contract 2.9): the one path that sends user-written SQL to ClickHouse.
//!
//! No method accepts a user name: the store computes the identity from the [`ProjectScope`]. Isolation between
//! projects is carried by ClickHouse access control alone (contract 7.4); everything here is transport, limits
//! and error mapping.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_records::{ProjectId, ProjectScope, Uuid};
use crucible_schema::RetentionSettings;

/// One query parameter, sent as `param_<name>`. The server parses the value by the type the query text declares
/// (`{name:Type}`), so a value can never become SQL.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SqlParam {
    /// The parameter name: an ASCII identifier.
    pub name: String,
    /// The value in ClickHouse's text form.
    pub value: String,
}

/// One statement to send as a project's workspace identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SqlQuery {
    /// The user's text. It is the whole statement, sent verbatim.
    pub sql: String,
    /// The parameter values.
    pub params: Vec<SqlParam>,
    /// The identity's password generation, from `ch_sql_identities`.
    pub generation: u32,
    /// The id the statement carries, so it can be cancelled.
    pub query_id: Uuid,
}

/// The application-side bounds of one run (contract 7.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SqlLimits {
    /// Rows kept in the grid.
    pub max_rows: usize,
    /// Bytes of cell text kept in the grid.
    pub max_bytes: usize,
    /// Bytes kept of one cell; a longer cell is cut and ends with [`CELL_CUT_MARKER`].
    pub max_cell_bytes: usize,
    /// The Rust-side deadline.
    pub deadline: Duration,
}

/// What a cut cell ends with.
pub const CELL_CUT_MARKER: &str = "…[cut]";

impl SqlLimits {
    /// The interactive limits: 10,000 rows, 16 MiB, 64 KiB per cell, and the profile's 30 s plus slack.
    pub const INTERACTIVE: Self = Self {
        max_rows: 10_000,
        max_bytes: 16 << 20,
        max_cell_bytes: 64 << 10,
        deadline: Duration::from_secs(35),
    };
}

/// One column of a grid.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SqlColumn {
    /// The column name.
    pub name: String,
    /// The ClickHouse type.
    pub ty: String,
}

/// What the server read to answer a run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct SqlStats {
    /// Rows read.
    pub rows_read: u64,
    /// Bytes read.
    pub bytes_read: u64,
    /// Server-side time, in milliseconds.
    pub elapsed_ms: u64,
}

/// One cell: a JSON value in ClickHouse's JSON output form (64-bit integers are strings).
pub type SqlCell = serde_json::Value;

/// The typed result of a run.
#[derive(Clone, PartialEq, Debug)]
pub struct SqlGrid {
    /// The columns.
    pub columns: Vec<SqlColumn>,
    /// The rows; each has one cell per column.
    pub rows: Vec<Vec<SqlCell>>,
    /// Whether rows were left out because a row or byte limit was reached.
    pub truncated: bool,
    /// How many cells were cut at the cell limit.
    pub cut_cells: u64,
    /// Read statistics.
    pub stats: SqlStats,
}

/// What `EXPLAIN ESTIMATE` expects a query to read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct SqlEstimate {
    /// Parts.
    pub parts: u64,
    /// Rows.
    pub rows: u64,
    /// Marks.
    pub marks: u64,
}

/// The export formats the server writes itself. CSV is written from a [`SqlGrid`], so the formula guard can be
/// applied to it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SqlExportFormat {
    /// One JSON object per line.
    JsonLines,
    /// Apache Parquet.
    Parquet,
}

/// A failed workspace call. Guardrail errors are never retried.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum SqlError {
    /// The server could not parse or analyse the text.
    #[error("the query is not valid: {message}")]
    Invalid {
        /// The server code.
        code: u32,
        /// The server's description, without its code prefix and version suffix.
        message: String,
        /// The line of a syntax error, one-based.
        line: Option<u32>,
        /// The column of a syntax error, one-based.
        column: Option<u32>,
    },
    /// The server refused the statement: it writes, changes a setting or reads something the role may not.
    #[error("the query is not allowed: {message}")]
    Forbidden {
        /// The server code.
        code: u32,
        /// The server's description.
        message: String,
    },
    /// The statement does not narrow the table's time column, so the window backstop refused it.
    #[error("the query does not filter on the time window")]
    TimeWindowRequired,
    /// A read, result or complexity limit of the profile was reached.
    #[error("the query reads too much: {message}")]
    TooBroad {
        /// The server code; 0 when an application-side bound was reached.
        code: u32,
        /// The server's description.
        message: String,
    },
    /// The time limit passed.
    #[error("the query exceeded its time limit")]
    Timeout,
    /// The memory limit was exceeded.
    #[error("the query exceeded its memory limit")]
    MemoryExceeded,
    /// The hourly quota or the concurrency limit of the identity was reached.
    #[error("the workspace quota is exhausted: {message}")]
    QuotaExceeded {
        /// The server's description.
        message: String,
    },
    /// The statement was cancelled.
    #[error("the query was cancelled")]
    Cancelled,
    /// The workspace cannot serve the project now: the store is unreachable, or the identity is not there.
    #[error("the SQL workspace is unavailable: {0}")]
    Unavailable(String),
    /// Anything else.
    #[error("the SQL workspace failed: {0}")]
    Internal(String),
}

impl SqlError {
    /// The problem code of contract 12.3.
    pub const fn problem_code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "sql_invalid",
            Self::Forbidden { .. } => "sql_forbidden",
            Self::TimeWindowRequired => "sql_time_window_required",
            Self::TooBroad { .. } => "query_too_broad",
            Self::Timeout => "query_timeout",
            Self::MemoryExceeded => "query_memory_exceeded",
            Self::QuotaExceeded { .. } => "sql_quota_exceeded",
            Self::Cancelled => "conflict",
            Self::Unavailable(_) => "sql_workspace_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }
}

/// One statement of the runtime self-probe that did what it must not.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ProbeBreach {
    /// The check's stable name, for example `cross_project_read`.
    pub check: &'static str,
    /// What was observed. It never quotes row data.
    pub detail: String,
}

/// The result of the runtime self-probe (contract 7.5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProbeReport {
    /// The server version the probe ran against.
    pub version: String,
    /// How many statements ran.
    pub statements: u32,
    /// The statements that succeeded where they must fail, or answered what they must not. Empty when the
    /// access scheme holds.
    pub breaches: Vec<ProbeBreach>,
}

/// The one path that sends user-written SQL to ClickHouse.
#[async_trait]
pub trait SqlWorkspaceStore: fmt::Debug + Send + Sync {
    /// Runs `EXPLAIN AST` and `EXPLAIN ESTIMATE` as the project identity: the server's own parser and access
    /// checks decide, and nothing is read.
    async fn validate(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
    ) -> Result<SqlEstimate, SqlError>;

    /// Sends the text verbatim as the whole statement and returns its result as a grid within `limits`.
    /// Dropping the future closes the connection, which cancels the statement, and issues `KILL QUERY`.
    async fn run(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        limits: SqlLimits,
    ) -> Result<SqlGrid, SqlError>;

    /// Sends the text verbatim and returns the server's own encoding of the result, at most
    /// `limits.max_bytes` long.
    async fn export(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        format: SqlExportFormat,
        limits: SqlLimits,
    ) -> Result<Bytes, SqlError>;

    /// Issues `KILL QUERY` for a statement of this project through `crucible_maint`.
    async fn cancel(&self, scope: ProjectScope, query_id: Uuid) -> Result<(), SqlError>;

    /// Creates or updates the project's identity (contract 7.4): its policies with the current retention, its
    /// password of this generation, then the role grant. Idempotent. Runs as `crucible_admin`.
    async fn provision(
        &self,
        project_id: ProjectId,
        generation: u32,
        retention: &RetentionSettings,
    ) -> Result<(), SqlError>;

    /// Removes the project's identity and policies. Idempotent.
    async fn deprovision(&self, project_id: ProjectId) -> Result<(), SqlError>;

    /// The runtime self-probe (contract 7.5), as the first probe identity.
    async fn probe(&self) -> Result<ProbeReport, SqlError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_its_problem_code() {
        let invalid = SqlError::Invalid {
            code: 62,
            message: String::new(),
            line: Some(1),
            column: Some(8),
        };
        assert_eq!(invalid.problem_code(), "sql_invalid");
        let forbidden = SqlError::Forbidden {
            code: 164,
            message: String::new(),
        };
        assert_eq!(forbidden.problem_code(), "sql_forbidden");
        assert_eq!(
            SqlError::TimeWindowRequired.problem_code(),
            "sql_time_window_required"
        );
        let quota = SqlError::QuotaExceeded {
            message: String::new(),
        };
        assert_eq!(quota.problem_code(), "sql_quota_exceeded");
        assert_eq!(
            SqlError::Unavailable(String::new()).problem_code(),
            "sql_workspace_unavailable"
        );
    }

    #[test]
    fn the_interactive_limits_are_the_contract_values() {
        let limits = SqlLimits::INTERACTIVE;
        assert_eq!(limits.max_rows, 10_000);
        assert_eq!(limits.max_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.max_cell_bytes, 64 * 1024);
    }
}
