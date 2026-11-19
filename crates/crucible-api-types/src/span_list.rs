//! The span list: `GET projects/{project_id}/spans` (contract 8.7 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::enums::{Level, ObservationType, StatusCode};
use crate::meta::Meta;
use crate::wire::SafeU64;

/// The query of `GET projects/{project_id}/spans`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct SpanListParams {
    /// Start of the window over the spans' start times, inclusive (RFC 3339).
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
    /// The `next_cursor` of the previous page, with the same window and filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cursor: Option<String>,
    /// Keep spans whose environment is one of these. Repeat the parameter for several values; different
    /// parameters must all hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub environment: Option<Vec<String>>,
    /// Keep spans whose release is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub release: Option<Vec<String>>,
    /// Keep spans whose version is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub version: Option<Vec<String>>,
    /// Keep spans whose level is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub level: Option<Vec<Level>>,
    /// Keep spans of one of these types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub observation_type: Option<Vec<ObservationType>>,
    /// Keep spans that name one of these users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub user_id: Option<Vec<String>>,
    /// Keep spans that name one of these sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub session_id: Option<Vec<String>>,
    /// Keep spans that carry at least one of these tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub tags: Option<Vec<String>>,
    /// Keep spans whose metadata holds every `key=value` given (split at the first `=`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(style = Form, explode)]
    #[ts(optional)]
    pub metadata: Option<Vec<String>>,
    /// Keep spans whose name contains this text, without regard to ASCII case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
}

/// One span of the list.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SpanListItem {
    /// 32 lowercase hex characters.
    pub trace_id: String,
    /// 16 lowercase hex characters.
    pub span_id: String,
    /// The parent span, or null for a root span.
    #[schema(required = true)]
    pub parent_span_id: Option<String>,
    /// The span name.
    pub name: String,
    /// What the span observes.
    pub observation_type: ObservationType,
    /// The span start, the list's sort key.
    #[schema(format = DateTime)]
    pub start_time: String,
    /// `end_time - start_time` in microseconds.
    pub duration_us: SafeU64,
    /// The span's status.
    pub status_code: StatusCode,
    /// The span's level.
    pub level: Level,
    /// The model the provider reported; empty for spans without one.
    pub model: String,
    /// The span's environment.
    pub environment: String,
    /// The user the span names; empty when none.
    pub user_id: String,
    /// The session the span names; empty when none.
    pub session_id: String,
    /// The span's cost in USD.
    pub total_cost_usd: f64,
    /// Total tokens.
    pub total_tokens: SafeU64,
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// One page of the span list, newest first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SpanListPage {
    /// The spans.
    pub data: Vec<SpanListItem>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}
