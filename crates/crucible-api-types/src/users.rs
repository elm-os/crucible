//! Users: `GET projects/{project_id}/users`, `GET projects/{project_id}/users/{user_id}` and
//! `GET projects/{project_id}/users/{user_id}/sessions` (contract 8.7 and 12.6). A user's traces are
//! `GET .../traces?user_id=`.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::meta::Meta;
use crate::wire::SafeU64;

/// The query of `GET projects/{project_id}/users` and of the sessions of one user.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct UserListParams {
    /// Start of the window (RFC 3339). Users are counted by UTC day, so the window selects every day it
    /// touches; session counts and a user's sessions use the window as given.
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

/// One user: the spans that name the user in the window's days, aggregated.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct UserItem {
    /// The user id, as the spans name it.
    pub user_id: String,
    /// The earliest span start.
    #[schema(format = DateTime)]
    pub first_seen: String,
    /// The latest span end; the list's sort key.
    #[schema(format = DateTime)]
    pub last_seen: String,
    /// Traces: the user's root spans.
    pub trace_count: SafeU64,
    /// The distinct sessions that the user's traces in the window name.
    pub session_count: SafeU64,
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
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// One page of the user list, last seen first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct UserPage {
    /// The users.
    pub data: Vec<UserItem>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}

/// One user. A user without spans in the window is `not_found`, unless the answer may be incomplete: then
/// `user` is null and `meta` says why.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct UserDetail {
    /// The user, or null when the answer may be incomplete.
    #[schema(required = true)]
    pub user: Option<UserItem>,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One session of a user, from the user's traces in the window.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct UserSessionItem {
    /// The session id.
    pub session_id: String,
    /// The start of the session's earliest trace of the user in the window.
    #[schema(format = DateTime)]
    pub first_trace_at: String,
    /// The start of the latest one; the list's sort key.
    #[schema(format = DateTime)]
    pub last_trace_at: String,
    /// The user's traces in the session.
    pub trace_count: SafeU64,
    /// Whether a count above saturated at 2^53 - 1.
    pub saturated: bool,
}

/// One page of the sessions of a user, latest trace first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct UserSessionPage {
    /// The sessions.
    pub data: Vec<UserSessionItem>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}
