//! The read services over `Stores.telemetry` and the recent-span index: trace by id, the trace list and its
//! live tail, the children of a span, and data bounds (contract 8.2, 8.6, 8.7 and 8.8).
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
//! - **Cursors** are plain values ([`PageCursor`], [`HeadCursor`], [`ChildCursor`]); the API signs and encodes
//!   them with `crucible-cursor`.
//! - **Errors** are [`QueryServiceError`], one problem code of contract 12.3 each.

mod bounds;
mod context;
mod cursor;
mod decode;
mod list;
mod service;
mod trace;

pub use bounds::{DataBounds, MinuteRange};
pub use context::{Meta, PartialReason, QueryServiceError, ReadContext};
pub use crucible_recent::Consistency;
/// Re-exported for callers that build a [`ReadContext`] without depending on `crucible-schema`.
pub use crucible_schema::RetentionSettings;
pub use cursor::{ChildCursor, HeadCursor, HeadKey, PageCursor};
pub use decode::TraceHeadView;
pub use list::{
    NEW_COUNT_MAX, TRACE_LIST_DEFAULT_LIMIT, TraceListItem, TraceListPage, TraceListQuery,
    TraceTail, TraceTailQuery,
};
pub use service::{QueryService, TraceSummary};
pub use trace::{SpanChildrenPage, SpanChildrenQuery, SpanSource, TraceDetail, TraceSpan};
