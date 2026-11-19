//! Sessions: `GET projects/{project_id}/sessions` and `GET projects/{project_id}/sessions/{session_id}`
//! (contract 8.7 and 12.6). A session's traces are `GET .../traces?session_id=`.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::meta::Meta;
use crate::wire::SafeU64;

/// The query of `GET projects/{project_id}/sessions`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    /// Start of the window (RFC 3339). Sessions are counted by UTC day, so the window selects every day it
    /// touches.
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
}

/// The query of `GET projects/{project_id}/sessions/{session_id}` and of the other reads of one entity
/// over a window.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct EntityParams {
    /// Start of the window (RFC 3339); every UTC day it touches is counted.
    #[param(format = DateTime)]
    pub from: String,
    /// End of the window, exclusive (RFC 3339).
    #[param(format = DateTime)]
    pub to: String,
}

/// One session: its spans in the window's days, aggregated.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionItem {
    /// The session id, as the spans name it.
    pub session_id: String,
    /// The earliest span start.
    #[schema(format = DateTime)]
    pub first_seen: String,
    /// The latest span end; the list's sort key.
    #[schema(format = DateTime)]
    pub last_seen: String,
    /// Traces: the session's root spans.
    pub trace_count: SafeU64,
    /// Spans.
    pub span_count: SafeU64,
    /// Spans with an error.
    pub error_count: SafeU64,
    /// The total cost in USD.
    pub total_cost_usd: f64,
    /// Input tokens.
    pub input_tokens: SafeU64,
    /// Output tokens.
    pub output_tokens: SafeU64,
    /// Total tokens.
    pub total_tokens: SafeU64,
    /// The users the session's spans name, ascending; at most 8 are kept.
    pub user_ids: Vec<String>,
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// One page of the session list, last seen first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionPage {
    /// The sessions.
    pub data: Vec<SessionItem>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}

/// One session. A session without spans in the window is `not_found`, unless the answer may be incomplete:
/// then `session` is null and `meta` says why.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionDetail {
    /// The session, or null when the answer may be incomplete.
    #[schema(required = true)]
    pub session: Option<SessionItem>,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}
