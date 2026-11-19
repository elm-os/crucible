//! The read services over `Stores.telemetry` and the recent-span index: trace by id, the trace list and its
//! live tail, the children of a span, data bounds, the span list, sessions and users, full-text search and
//! payload byte ranges (contract 8.2, 8.6, 8.7 and 8.8).
//!
//! - **Scope and retention.** Every method of [`QueryService`] takes a [`ReadContext`]: a `ProjectScope` and the
//!   project's retention settings. Plans come only from the builders of `crucible-plan`, which clamp each
//!   window to the retention cut-off, and recent-index entries are filtered with the same cut-off, so no read
//!   returns an expired row.
//! - **Post-operators.** The merge with the recent index, span de-duplication by `span_id`, pick resolution and
//!   provisional-head filtering run here, above the store, so they are the same code over ClickHouse and over
//!   the simulation's store. Fact decoration joins them when facts exist.
//! - **`meta`.** Every response carries [`Meta`]: `partial` with its reason when the store did not answer, when
//!   the project has an open index gap, or when it has rows in a quarantined pair; and `consistency`, which is
//!   `lagging` when stage B had not applied every acknowledged frame.
//! - **Cursors** are plain values ([`PageCursor`], [`HeadCursor`], [`ChildCursor`], [`SpanListCursor`],
//!   [`IdCursor`]); the API signs and encodes them with `crucible-cursor`.
//! - **Errors** are [`QueryServiceError`], one problem code of contract 12.3 each.
//! - **Facts** resolve in Rust with `crucible_records::fold_facts` (contract 3.8): the effective facts of one
//!   target, and the decoration of trace detail and of trace list pages.
//!
//! Each service is one module (`trace_detail`, `trace_list`, `live_tail`, `span_children`, `data_bounds`,
//! `span_list`, `sessions`, `users`, `search`, `payload`), registered here with its `mod` line and its
//! `pub use` line. What the services share is in `service` (the store calls and the per-trace aggregates),
//! `spans` (the merge and de-duplication of a trace's spans), `heads` (the merge of trace heads), `context`
//! (`meta` and errors), `cursor` and `decode`.

mod context;
mod cursor;
mod data_bounds;
mod decode;
mod export;
mod facts;
mod filter_values;
mod heads;
mod live_tail;
mod metrics;
mod payload;
mod search;
mod service;
mod sessions;
mod span_children;
mod span_list;
mod spans;
mod trace_detail;
mod trace_list;
mod users;

pub use context::{Meta, PartialReason, QueryServiceError, ReadContext};
pub use crucible_recent::Consistency;
/// Re-exported for callers that build a [`ReadContext`] without depending on `crucible-schema`.
pub use crucible_schema::RetentionSettings;
pub use cursor::{ChildCursor, HeadCursor, HeadKey, IdCursor, PageCursor, SpanListCursor};
pub use data_bounds::{DataBounds, MinuteRange};
pub use decode::TraceHeadView;
pub use export::{
    ExportColumn, ExportFact, ExportLimits, ExportMcp, ExportPage, ExportQuery, ExportRecord,
    ExportWait,
};
pub use facts::{
    CorrectionFact, FactTarget, Facts, FlagFact, Resolved, ScoreFact, TagEdit, TargetBounds,
    TargetFacts, TraceFacts,
};
pub use filter_values::{FilterValues, FilterValuesQuery};
pub use heads::{TRACE_LIST_DEFAULT_LIMIT, TraceListItem};
pub use live_tail::{NEW_COUNT_MAX, TraceTail, TraceTailQuery};
pub use metrics::{
    MetricValue, MetricsPoint, MetricsRequest, MetricsResult, MetricsSeries, MetricsTotal,
    MetricsTotals,
};
pub use payload::{PayloadBytes, PayloadQuery};
pub use search::{
    Fragment, SEARCH_DEFAULT_LIMIT, SEARCH_MAX_TOKENS, SearchCursor, SearchHit, SearchMatch,
    SearchPage, SearchQuery, search_tokens,
};
pub use service::{QueryService, TraceSummary};
pub use sessions::{SessionDetail, SessionListPage, SessionListQuery, SessionSummary};
pub use span_children::{SpanChildrenPage, SpanChildrenQuery};
pub use span_list::{SpanListPage, SpanListQuery};
pub use spans::{SpanSource, TraceSpan};
pub use trace_detail::TraceDetail;
pub use trace_list::{TraceListPage, TraceListQuery};
pub use users::{
    UserDetail, UserListPage, UserListQuery, UserSession, UserSessionsPage, UserSessionsQuery,
    UserSummary,
};
