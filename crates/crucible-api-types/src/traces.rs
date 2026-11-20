//! The trace list and its live tail: `GET projects/{project_id}/traces` (contract 8.7, 8.8 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::enums::{HeadKind, Level, ObservationType, StatusCode};
use crate::facts::TraceFacts;
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
    /// window and filters. Not combined with `cursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub since: Option<String>,
    /// Keep traces whose head span's environment is one of these. Repeat the parameter for several values;
    /// different parameters must all hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub environment: Option<Vec<String>>,
    /// Keep traces whose head span's release is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub release: Option<Vec<String>>,
    /// Keep traces whose head span's version is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub version: Option<Vec<String>>,
    /// Keep traces whose head span's level is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub level: Option<Vec<Level>>,
    /// Keep traces with at least one span in the window of one of these types. Cannot be combined with
    /// `metadata`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub observation_type: Option<Vec<ObservationType>>,
    /// Keep traces whose head span names one of these users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub user_id: Option<Vec<String>>,
    /// Keep traces whose head span names one of these sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub session_id: Option<Vec<String>>,
    /// Keep traces whose effective tags hold at least one of these: the head span's tags, plus the tags added and
    /// minus the tags removed with `POST tags`. At most 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub tags: Option<Vec<String>>,
    /// Keep traces with at least one span in the window whose metadata holds every `key=value` given (split
    /// at the first `=`). Cannot be combined with `observation_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub metadata: Option<Vec<String>>,
    /// Keep traces whose head span's name contains this text, without regard to ASCII case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
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
    /// The effective trace tags: the head's tags, which are empty until the store serves the head, plus the
    /// tags added by edits, minus the removed ones.
    pub tags: Vec<String>,
    /// The trace's facts. Scores carry no comment or metadata here, and there is no correction.
    pub facts: TraceFacts,
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
