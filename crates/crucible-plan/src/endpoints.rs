//! The plan builders of each endpoint (contract 8.7 and 8.8), one module per endpoint. Each module is
//! registered here with its `mod` line and its `pub use` line, and the crate root re-exports what this module
//! exports. What the builders share is in `build`.

mod data_bounds;
mod facts;
mod filter_values;
mod metrics;
mod payload;
mod search;
mod sessions;
mod span_children;
mod span_list;
mod trace_bounds;
mod trace_list;
mod trace_spans;
mod users;

pub use data_bounds::{DataBoundsPlans, data_bounds};
pub use facts::{FactTable, FactsOf, fact_page, fact_rows};
pub use filter_values::{FILTER_VALUES_MAX_LIMIT, FilterValuesRequest, filter_values};
pub use metrics::{
    Granularity, MetricsError, MetricsQuery, MetricsShape, metrics_keys, metrics_other,
    metrics_ranking, metrics_series, metrics_shape,
};
pub use payload::{PayloadField, PayloadRangeRequest, payload_range};
pub use search::{
    SEARCH_MAX_LIMIT, SEARCH_SNIPPET, SearchField, SearchKey, SearchPlan, SearchRequest, search,
    search_spans,
};
pub use sessions::{SESSION_LIST_MAX_LIMIT, SessionListRequest, session_detail, session_list};
pub use span_children::{
    SPAN_CHILDREN_DEFAULT_LIMIT, SPAN_CHILDREN_MAX_LIMIT, child_counts, span_children,
};
pub use span_list::{SPAN_LIST_MAX_LIMIT, SpanListRequest, span_list};
pub use trace_bounds::trace_time_bounds;
pub use trace_list::{
    TRACE_LIST_MAX_LIMIT, TRACE_LIST_OVER_READ, TraceListRequest, filtered_trace_list, trace_list,
    trace_page_stats,
};
pub use trace_spans::{
    TRACE_INLINE_SPAN_LIMIT, TRACE_PAGED_SPAN_LIMIT, spans_of_parents, trace_roots, trace_spans,
};
pub use users::{
    USER_LIST_MAX_LIMIT, USER_SESSIONS_MAX_LIMIT, UserListRequest, UserSessionsRequest,
    user_detail, user_list, user_session_counts, user_sessions,
};
