//! Traces: the list, one trace, and the children of a span (contract 8.7, 8.8 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::enums::{HeadKind, Level, ObservationType, StatusCode};
use crate::meta::Meta;
use crate::wire::SafeU64;

/// The query of `GET projects/{project_id}/traces`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct TraceListParams {
    /// Start of the window over the traces' start times, inclusive (RFC 3339).
    #[param(format = DateTime)]
    pub from: String,
    /// End of the window, exclusive (RFC 3339). The window may span at most the project's maximum (default
    /// 30 days).
    #[param(format = DateTime)]
    pub to: String,
    /// Page size: 1 to 200, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 200)]
    #[ts(optional)]
    pub limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cursor: Option<String>,
    /// Live tail: the `head_cursor` of a first page or of the previous live-tail response, with the same
    /// window. Not combined with `cursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub since: Option<String>,
}

/// One trace of the list: its head (the root span, or the earliest span while the root has not arrived) and
/// the trace's merged aggregates.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TraceListItem {
    /// 32 lowercase hex characters.
    pub trace_id: String,
    /// The head span's name.
    pub name: String,
    /// The trace name: the root span's, else the latest non-empty one; empty when no span set one.
    pub trace_name: String,
    /// Whether the head is the root span.
    pub head_kind: HeadKind,
    /// The head span's start, the list's sort key.
    #[schema(format = DateTime)]
    pub start_time: String,
    /// From the trace's first span start to its last span end, in microseconds.
    pub duration_us: SafeU64,
    /// The head span's status.
    pub status_code: StatusCode,
    /// The head span's level.
    pub level: Level,
    /// Spans in the trace.
    pub span_count: SafeU64,
    /// Spans of level `error`.
    pub error_count: SafeU64,
    /// The trace's total cost in USD.
    pub total_cost_usd: f64,
    /// Input tokens across the trace.
    pub input_tokens: SafeU64,
    /// Output tokens across the trace.
    pub output_tokens: SafeU64,
    /// Total tokens across the trace.
    pub total_tokens: SafeU64,
    /// The trace's user, picked like the trace name.
    pub user_id: String,
    /// The trace's session, picked like the trace name.
    pub session_id: String,
    /// The head span's environment; empty until the store serves the head.
    pub environment: String,
    /// The head span's release; empty until the store serves the head.
    pub release: String,
    /// The trace tags; empty until the store serves the head.
    pub tags: Vec<String>,
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// One page of the trace list, newest first; or, for a `since` request, the traces that arrived after the
/// head cursor.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TraceListPage {
    /// The traces.
    pub data: Vec<TraceListItem>,
    /// The cursor of the next page; null on the last page and on live-tail responses.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists, or, for live tail, whether more traces arrived than `data` holds.
    pub has_more: bool,
    /// The cursor to pass as `since` for live tail; set on first pages and live-tail responses, else null.
    #[schema(required = true)]
    pub head_cursor: Option<String>,
    /// For live tail only: how many traces arrived, saturating at 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub new_count: Option<u32>,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}

/// One span of a trace.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Span {
    /// 16 lowercase hex characters.
    pub span_id: String,
    /// The parent span, or null for a root span.
    #[schema(required = true)]
    pub parent_span_id: Option<String>,
    /// The span name.
    pub name: String,
    /// What the span observes.
    pub observation_type: ObservationType,
    /// The span start.
    #[schema(format = DateTime)]
    pub start_time: String,
    /// The span end.
    #[schema(format = DateTime)]
    pub end_time: String,
    /// `end_time - start_time` in microseconds.
    pub duration_us: SafeU64,
    /// The span's status.
    pub status_code: StatusCode,
    /// The span's level.
    pub level: Level,
    /// The model the provider reported; empty for spans without one.
    pub model: String,
    /// The span's cost in USD.
    pub total_cost_usd: f64,
    /// Input tokens.
    pub input_tokens: SafeU64,
    /// Output tokens.
    pub output_tokens: SafeU64,
    /// Total tokens.
    pub total_tokens: SafeU64,
    /// The number of direct children, on paged traces and on children pages; null otherwise.
    #[schema(required = true)]
    pub child_count: Option<SafeU64>,
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// One trace. Up to 5,000 spans it holds every span; above that it holds the root spans and
/// their direct children, and `GET .../spans` pages the rest.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TraceDetail {
    /// 32 lowercase hex characters.
    pub trace_id: String,
    /// Spans in the trace. Exact when `paged` is false.
    pub span_count: SafeU64,
    /// False: `spans` holds every span. True: the root spans and their direct children, at most 1,000, each
    /// with `child_count`.
    pub paged: bool,
    /// The spans in start order. Empty only when `meta` says the response is partial or lagging.
    pub spans: Vec<Span>,
    /// How complete and how fresh the response is.
    pub meta: Meta,
    /// Whether `span_count` saturated at 2^53 - 1.
    pub saturated: bool,
}

/// The query of `GET projects/{project_id}/traces/{trace_id}/spans`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct SpanPageParams {
    /// The span whose direct children to list (16 hex characters).
    pub parent_span_id: String,
    /// The `next_cursor` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cursor: Option<String>,
    /// Page size: 1 to 2,000, default 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 2000)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// One page of the direct children of a span, in start order. Each carries `child_count`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SpanPage {
    /// The children.
    pub data: Vec<Span>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}
