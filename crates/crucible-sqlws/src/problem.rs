//! The workspace's errors as the API reports them (contracts 1.7 and 12.3): a stable code and a sentence that
//! says how to fix the query. The server's own description travels beside it, because for a query error it is
//! the precise part.

use std::time::Duration;

use crucible_control::TxError;
use crucible_env::SqlError;

use crate::lexer::LexError;
use crate::limits::LimitHit;
use crate::params::BindError;

/// The problem codes the workspace answers with (contract 12.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SqlProblemCode {
    /// `sql_invalid`.
    Invalid,
    /// `sql_forbidden`.
    SqlForbidden,
    /// `sql_time_window_required`.
    TimeWindowRequired,
    /// `sql_quota_exceeded`.
    QuotaExceeded,
    /// `sql_workspace_unavailable`.
    Unavailable,
    /// `query_too_broad`.
    TooBroad,
    /// `query_timeout`.
    Timeout,
    /// `query_memory_exceeded`.
    MemoryExceeded,
    /// `validation_failed`.
    Validation,
    /// `forbidden`: the caller's role is too low.
    Forbidden,
    /// `not_found`.
    NotFound,
    /// `conflict`.
    Conflict,
    /// `precondition_failed`.
    PreconditionFailed,
    /// `control_plane_unavailable`.
    ControlPlaneUnavailable,
    /// `internal_error`.
    Internal,
}

impl SqlProblemCode {
    /// The stable code.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Invalid => "sql_invalid",
            Self::SqlForbidden => "sql_forbidden",
            Self::TimeWindowRequired => "sql_time_window_required",
            Self::QuotaExceeded => "sql_quota_exceeded",
            Self::Unavailable => "sql_workspace_unavailable",
            Self::TooBroad => "query_too_broad",
            Self::Timeout => "query_timeout",
            Self::MemoryExceeded => "query_memory_exceeded",
            Self::Validation => "validation_failed",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PreconditionFailed => "precondition_failed",
            Self::ControlPlaneUnavailable => "control_plane_unavailable",
            Self::Internal => "internal_error",
        }
    }
}

/// One refusal of the workspace.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SqlProblem {
    /// The code.
    pub code: SqlProblemCode,
    /// How to fix it, in one or two sentences.
    pub fix: String,
    /// ClickHouse's own description of a query error.
    pub server_message: Option<String>,
    /// The line of the error in the query text, one-based.
    pub line: Option<u32>,
    /// The column of the error in the query text, one-based.
    pub column: Option<u32>,
    /// When trying again can work.
    pub retry_after: Option<Duration>,
    /// Why the workspace is unavailable: a stable word such as `isolation_probe_failed`.
    pub reason: Option<String>,
}

impl SqlProblem {
    /// A problem with a code and a fix.
    pub fn new(code: SqlProblemCode, fix: impl Into<String>) -> Self {
        Self {
            code,
            fix: fix.into(),
            server_message: None,
            line: None,
            column: None,
            retry_after: None,
            reason: None,
        }
    }

    /// The workspace is unavailable for `reason`.
    pub fn unavailable(reason: &str, fix: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.to_owned()),
            ..Self::new(SqlProblemCode::Unavailable, fix)
        }
    }

    /// The caller's role is too low.
    pub fn role(fix: &str) -> Self {
        Self::new(SqlProblemCode::Forbidden, fix)
    }

    /// No such entity, or one the caller cannot see.
    pub fn not_found(what: &str) -> Self {
        Self::new(
            SqlProblemCode::NotFound,
            format!("There is no {what} with this id in the project."),
        )
    }
}

impl From<LexError> for SqlProblem {
    fn from(error: LexError) -> Self {
        let code = if error.kind.is_window() {
            SqlProblemCode::TimeWindowRequired
        } else {
            SqlProblemCode::Invalid
        };
        Self {
            line: Some(error.line),
            column: Some(error.column),
            ..Self::new(code, error.kind.fix())
        }
    }
}

impl From<BindError> for SqlProblem {
    fn from(error: BindError) -> Self {
        Self::new(SqlProblemCode::Validation, error.to_string())
    }
}

impl From<LimitHit> for SqlProblem {
    fn from(hit: LimitHit) -> Self {
        match hit {
            LimitHit::ActorConcurrency => Self {
                retry_after: Some(Duration::from_secs(1)),
                ..Self::new(
                    SqlProblemCode::QuotaExceeded,
                    "You already have a query running. Wait for it to finish or cancel it, then run this one.",
                )
            },
            LimitHit::ProjectConcurrency => Self {
                retry_after: Some(Duration::from_secs(2)),
                ..Self::new(
                    SqlProblemCode::QuotaExceeded,
                    "Four queries of this project are running, which is the limit. Try again when one finishes.",
                )
            },
            LimitHit::Rate { retry_after } => Self {
                retry_after: Some(retry_after),
                ..Self::new(
                    SqlProblemCode::QuotaExceeded,
                    format!(
                        "You started 30 queries in the last minute, which is the limit. Try again in {} s.",
                        retry_after.as_secs().max(1)
                    ),
                )
            },
        }
    }
}

impl From<TxError> for SqlProblem {
    fn from(error: TxError) -> Self {
        match error {
            TxError::Unavailable(_) | TxError::CommitUnknown | TxError::SerializationFailure => {
                Self {
                    retry_after: Some(Duration::from_secs(2)),
                    ..Self::new(
                        SqlProblemCode::ControlPlaneUnavailable,
                        "The control plane database did not answer. Try again shortly.",
                    )
                }
            }
            other => Self::new(SqlProblemCode::Internal, other.to_string()),
        }
    }
}

/// How to fix a statement the server refused as invalid, by its code.
fn invalid_fix(code: u32) -> &'static str {
    match code {
        62 => "Fix the syntax at the marked position.",
        47 | 8 | 16 => {
            "Check the column names against the schema browser: the query names one that the table does not have."
        }
        60 | 81 => "Use a table from the schema browser, for example crucible_sql.spans.",
        46 | 63 => "Check the function name: ClickHouse does not know this function.",
        42 | 43 | 53 => {
            "Check the arguments of the function named in the message: their number or types do not fit."
        }
        456 | 457 => "Give every {name:Type} parameter a value that fits its declared type.",
        215 => "Add the column to GROUP BY, or wrap it in an aggregate function such as any().",
        6 | 27 | 38 | 41 | 69 | 70 | 72 => {
            "Check the value or cast named in the message: it cannot be converted to the target type."
        }
        153 => "Guard the division: the divisor is zero for some row.",
        _ => "Correct the query as the message from ClickHouse describes.",
    }
}

fn forbidden_fix(code: u32) -> &'static str {
    match code {
        164 => {
            "The workspace is read-only: remove SET, SETTINGS clauses and statements that write."
        }
        497 | 291 => {
            "Read only the relations in the schema browser (database crucible_sql). System tables, other \
             databases and table functions that reach outside ClickHouse are not available."
        }
        _ => {
            "The workspace does not allow this statement. Keep to SELECT over the relations in the schema browser."
        }
    }
}

fn too_broad_fix(code: u32) -> &'static str {
    match code {
        158 | 307 | 396 => {
            "Narrow the time window or add filters: the query would read more rows or bytes than the workspace allows."
        }
        565 => {
            "Narrow the time window: the query spans more days of data than the workspace reads at once."
        }
        160 => "Remove sleep functions, or narrow the query: it is estimated to run too long.",
        161 | 162 | 165 | 166 | 167 | 168 => {
            "Simplify the query: it has too many columns, subquery levels or expression elements."
        }
        191 => {
            "Filter before joining or before IN: the joined or looked-up set is larger than the workspace allows."
        }
        _ => {
            "Select fewer columns or rows, or aggregate: the result is larger than the workspace returns."
        }
    }
}

impl From<SqlError> for SqlProblem {
    fn from(error: SqlError) -> Self {
        match error {
            SqlError::Invalid {
                code,
                message,
                line,
                column,
            } => Self {
                server_message: Some(message),
                line,
                column,
                ..Self::new(SqlProblemCode::Invalid, invalid_fix(code))
            },
            SqlError::Forbidden { code, message } => Self {
                server_message: Some(message),
                ..Self::new(SqlProblemCode::SqlForbidden, forbidden_fix(code))
            },
            SqlError::TimeWindowRequired => Self::new(
                SqlProblemCode::TimeWindowRequired,
                "Filter the table on its time column with the window, for example WHERE start_time >= \
                 {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}. The schema browser names each \
                 relation's time column.",
            ),
            SqlError::TooBroad { code, message } => Self {
                server_message: Some(message),
                ..Self::new(SqlProblemCode::TooBroad, too_broad_fix(code))
            },
            SqlError::Timeout => Self::new(
                SqlProblemCode::Timeout,
                "The query ran longer than 30 s. Narrow the time window, filter earlier, or aggregate over a \
                 rollup relation such as crucible_sql.span_metrics.",
            ),
            SqlError::MemoryExceeded => Self::new(
                SqlProblemCode::MemoryExceeded,
                "The query needed more than 2 GiB of memory. Narrow the time window, select fewer columns, or \
                 group by fewer keys.",
            ),
            SqlError::QuotaExceeded { message } => Self {
                server_message: Some(message),
                retry_after: Some(Duration::from_secs(60)),
                ..Self::new(
                    SqlProblemCode::QuotaExceeded,
                    "The project's SQL allowance for this hour is used up, or too many of its queries are running. \
                     Wait and run the query again.",
                )
            },
            SqlError::Cancelled => Self::new(SqlProblemCode::Conflict, "The run was cancelled."),
            SqlError::Unavailable(_) => Self {
                retry_after: Some(Duration::from_secs(5)),
                ..Self::unavailable(
                    "store_unavailable",
                    "ClickHouse did not accept the workspace's connection. Try again shortly; if it persists, an \
                     operator should check the server and run `crucible migrate`.",
                )
            },
            SqlError::Internal(message) => Self::new(SqlProblemCode::Internal, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::check;

    #[test]
    fn every_store_error_has_a_code_and_a_fix() {
        let errors = [
            SqlError::Invalid {
                code: 62,
                message: "Syntax error".to_owned(),
                line: Some(1),
                column: Some(8),
            },
            SqlError::Forbidden {
                code: 164,
                message: "readonly".to_owned(),
            },
            SqlError::TimeWindowRequired,
            SqlError::TooBroad {
                code: 158,
                message: "rows".to_owned(),
            },
            SqlError::Timeout,
            SqlError::MemoryExceeded,
            SqlError::QuotaExceeded {
                message: "quota".to_owned(),
            },
            SqlError::Cancelled,
            SqlError::Unavailable("down".to_owned()),
        ];
        for error in errors {
            let expected = error.problem_code();
            let problem = SqlProblem::from(error);
            assert_eq!(problem.code.as_str(), expected);
            assert!(problem.fix.len() > 15, "{problem:?}");
        }
        let syntax = SqlProblem::from(SqlError::Invalid {
            code: 62,
            message: "Syntax error".to_owned(),
            line: Some(2),
            column: Some(5),
        });
        assert_eq!((syntax.line, syntax.column), (Some(2), Some(5)));
        assert_eq!(syntax.server_message.as_deref(), Some("Syntax error"));
    }

    #[test]
    fn lexical_and_limit_refusals_map_to_their_codes() {
        let window = SqlProblem::from(check("SELECT 1").unwrap_err());
        assert_eq!(window.code, SqlProblemCode::TimeWindowRequired);
        let invalid = SqlProblem::from(check("DROP TABLE x").unwrap_err());
        assert_eq!(invalid.code, SqlProblemCode::Invalid);
        assert_eq!((invalid.line, invalid.column), (Some(1), Some(1)));
        let rate = SqlProblem::from(LimitHit::Rate {
            retry_after: Duration::from_secs(12),
        });
        assert_eq!(rate.code, SqlProblemCode::QuotaExceeded);
        assert_eq!(rate.retry_after, Some(Duration::from_secs(12)));
        assert!(rate.fix.contains("12 s"));
        let down = SqlProblem::from(TxError::Unavailable("x".to_owned()));
        assert_eq!(down.code, SqlProblemCode::ControlPlaneUnavailable);
    }
}
