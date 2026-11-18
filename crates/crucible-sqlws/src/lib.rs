//! The SQL workspace (contracts 7.4, 7.5 and 12.6): the one place where a user's own SQL reaches ClickHouse.
//!
//! Isolation between projects is carried by ClickHouse access control alone: each project's statements run as
//! that project's own ClickHouse user, behind row policies, a read-only settings profile and a quota (see
//! `crucible-schema` and the `sqliso` suite in `crucible-conformance`). Nothing in this crate is a security
//! boundary. What it adds is guidance and bookkeeping:
//!
//! - [`lexer`]: a small lexical check that turns common mistakes into errors that say how to fix them;
//! - [`params`]: the window parameters `{from}` and `{to}` and the declared parameters of saved queries;
//! - [`access`]: who may run what (contract 11.6), as one pure function;
//! - [`limits`]: 1 concurrent run per actor, 4 per project, 30 per minute per actor;
//! - [`csv`]: CSV export with the spreadsheet formula guard;
//! - [`SqlWorkspace`]: runs, exports, cancels, history and saved queries, each run recorded in `sql_runs`; the
//!   runtime self-probe, which disables the workspace deployment-wide on any breach;
//! - [`reconciler`]: the leader job that provisions, rotates and drops the per-project identities.

pub mod access;
pub mod csv;
pub mod lexer;
pub mod limits;
pub mod params;
mod problem;
pub mod reconciler;
mod service;

pub use access::{SqlAction, SqlRole, allowed};
pub use problem::{SqlProblem, SqlProblemCode};
pub use reconciler::{ReconcileError, ReconcileReport};
pub use service::{
    ExportFormat, MAX_HISTORY, PROBE_EVERY, PROBE_RETRY, RunInput, RunOutput, SavedInput,
    SchemaRelation, SqlCaller, SqlStatus, SqlWorkspace,
};

#[cfg(test)]
mod tests;
