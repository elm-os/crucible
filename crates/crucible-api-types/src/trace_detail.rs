//! One trace: `GET projects/{project_id}/traces/{trace_id}` (contract 8.7, 8.8 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::enums::{HeadKind, IoShape, Level, ObservationType, StatusCode};
use crate::facts::TraceFacts;
use crate::meta::Meta;
use crate::wire::SafeU64;

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
    /// How the span's input and output are structured; null for a span the store does not serve yet.
    #[schema(required = true)]
    pub io_shape: Option<IoShape>,
    /// Whether the span's input was cut at the size limit, which leaves it as opaque text (a cut JSON value no
    /// longer parses); null for a span that has not reached ClickHouse.
    #[schema(required = true)]
    pub input_truncated: Option<bool>,
    /// Whether the span's output was cut at the size limit, as `input_truncated` says of the input.
    #[schema(required = true)]
    pub output_truncated: Option<bool>,
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// What a trace's header shows: its name, totals and identity, worked out the way the trace list works out
/// its rows.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TraceSummary {
    /// The head span's name. The head is the trace's root span (the earliest one when there are several), else
    /// its earliest span.
    pub name: String,
    /// The trace name: the root span's, else the latest non-empty one; empty when no span set one.
    pub trace_name: String,
    /// Whether the head is the root span (`root`), or the earliest span of a trace whose root span was not
    /// received (`provisional`).
    pub head_kind: HeadKind,
    /// The trace's start: the start of its earliest span.
    #[schema(format = DateTime)]
    pub start_time: String,
    /// From the trace's first span start to its last span end, in microseconds.
    pub duration_us: SafeU64,
    /// The head span's status.
    pub status_code: StatusCode,
    /// The head span's level.
    pub level: Level,
    /// Spans in the trace: the response's `span_count`.
    pub span_count: SafeU64,
    /// Spans whose status or level is `error`.
    pub error_count: SafeU64,
    /// The trace's total cost in USD. A span whose child spans report usage is left out, so a parent that
    /// repeats its children's usage is not counted twice. The trace list's `total_cost_usd` is summed the same
    /// way.
    pub total_cost_usd: f64,
    /// Input tokens across the trace, summed like the cost.
    pub input_tokens: SafeU64,
    /// Output tokens across the trace, summed like the cost.
    pub output_tokens: SafeU64,
    /// Total tokens across the trace, summed like the cost.
    pub total_tokens: SafeU64,
    /// The trace's user, picked like the trace name.
    pub user_id: String,
    /// The trace's session, picked like the trace name.
    pub session_id: String,
    /// The head span's environment.
    pub environment: String,
    /// The head span's release; empty until the store serves the head span.
    pub release: String,
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
    /// The trace's name, totals and identity, for the whole trace even when `paged` is true; null when `spans`
    /// is empty.
    #[schema(required = true)]
    pub summary: Option<TraceSummary>,
    /// The spans in start order. Empty only when `meta` says the response is partial or lagging.
    pub spans: Vec<Span>,
    /// The trace's facts: effective tags, flags, scores and the correction of its output.
    pub facts: TraceFacts,
    /// How complete and how fresh the response is.
    pub meta: Meta,
    /// Whether `span_count` saturated at 2^53 - 1.
    pub saturated: bool,
}

/// The query of `GET projects/{project_id}/traces/{trace_id}`: no window, because the trace's own time bounds
/// are looked up.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct TraceDetailParams {
    /// When the trace is known to have started, for example from the list (RFC 3339). Accepted and checked,
    /// then unused: the read finds the trace's time bounds itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(format = DateTime)]
    #[ts(optional)]
    pub hint_time: Option<String>,
}
