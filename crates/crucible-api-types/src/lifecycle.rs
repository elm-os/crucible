//! Retention settings and deletion requests: `projects/{project_id}/retention` and
//! `projects/{project_id}/deletion-requests` (contracts 12.6 and 13).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

/// The instants before which a project's data stays hidden because an earlier, shorter retention setting had
/// already hidden it. Lengthening retention never shows that data again.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
pub struct RetentionHidden {
    /// For everything bound to `retention_days` (RFC 3339); null when nothing was hidden.
    #[schema(format = DateTime)]
    pub retention: Option<String>,
    /// For span payloads and corrections.
    #[schema(format = DateTime)]
    pub payload: Option<String>,
    /// For MCP arguments and results.
    #[schema(format = DateTime)]
    pub mcp_content: Option<String>,
}

/// A project's retention settings.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Retention {
    /// Days that data is kept, counted on each span's start time; null keeps data forever.
    pub retention_days: Option<u32>,
    /// Days that span payloads and corrections are kept; at most `retention_days`. Null follows
    /// `retention_days`.
    pub payload_retention_days: Option<u32>,
    /// Days that MCP arguments and results are kept; at most `retention_days`. Null follows `retention_days`.
    pub mcp_content_retention_days: Option<u32>,
    /// What earlier settings had hidden and stays hidden.
    pub hidden_before: RetentionHidden,
    /// The project's version, for `If-Match`.
    pub version: u32,
}

/// The body of `PUT retention`: the three settings, each a number of days or null.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RetentionUpdate {
    /// Days that data is kept (1 to 36,500); null keeps data forever.
    #[schema(minimum = 1, maximum = 36500)]
    pub retention_days: Option<u32>,
    /// Days that span payloads and corrections are kept; at most `retention_days`.
    #[schema(minimum = 1, maximum = 36500)]
    pub payload_retention_days: Option<u32>,
    /// Days that MCP arguments and results are kept; at most `retention_days`.
    #[schema(minimum = 1, maximum = 36500)]
    pub mcp_content_retention_days: Option<u32>,
}

/// What a deletion request selects.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum DeletionKind {
    /// A list of traces.
    Trace,
    /// Everything of one user.
    User,
    /// Everything of one session.
    Session,
    /// Everything of the project: the purge that deleting the project starts. Deleting the project makes this
    /// request; `POST deletion-requests` refuses it.
    Project,
}

/// Where a deletion request stands.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum DeletionState {
    /// Accepted. The data is already hidden from the API and the MCP server.
    Pending,
    /// Every running node has recorded the request in its write-ahead log.
    Logged,
    /// Every delete has run: the data is hidden from every read, the SQL workspace included.
    Applied,
    /// The data is physically removed from the telemetry store.
    Verified,
    /// Every place is accounted for.
    Completed,
    /// A step was given up and the request stopped. Data it had not deleted yet may be readable again; an
    /// operator must look at the server log.
    Failed,
}

/// The body of `POST deletion-requests`. Exactly the members of the chosen `kind` are set.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DeletionRequestCreate {
    /// What to delete.
    pub kind: DeletionKind,
    /// For `trace`: 1 to 10,000 trace ids, each 32 hex characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, max_items = 10000)]
    #[ts(optional)]
    pub trace_ids: Option<Vec<String>>,
    /// For `user`: the `user_id` of the spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub user_id: Option<String>,
    /// For `user`, optional: the token issuer under which the user also appears on MCP rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub issuer: Option<String>,
    /// For `session`: the `session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub session_id: Option<String>,
}

/// Where one place of the reach list stands for a deletion request.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct DeletionPlace {
    /// The place: a telemetry table, or `comments`, `media`, `export_files`, and for a project deletion
    /// `control_plane` (its members, alert rules, channels, saved queries and the rest of its rows).
    pub place: String,
    /// `pending`, `deleted` (hidden from every read), `verified` (physically removed), `failed`, `done`, or
    /// `not_present` for a place this deployment does not have.
    pub status: String,
    /// Deletes planned for the place, one per partition and page of traces; for a project deletion, one per
    /// partition, a partition dropped whole among them.
    pub deletes: u32,
    /// Deletes that have run.
    pub deletes_done: u32,
    /// Deletes verified after the mask cycle.
    pub deletes_verified: u32,
}

/// A deletion request.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct DeletionRequest {
    /// The request id.
    pub id: String,
    /// What it selects.
    pub kind: DeletionKind,
    /// For `trace`: how many traces the request names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub trace_count: Option<u32>,
    /// For `trace`: the first trace ids, at most 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub trace_ids: Option<Vec<String>>,
    /// For `user`: the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub user_id: Option<String>,
    /// For `user`: the issuer, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub issuer: Option<String>,
    /// For `session`: the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// Where the request stands.
    pub state: DeletionState,
    /// Whether the data is verified as physically removed from the telemetry store.
    pub verified: bool,
    /// The point in time of the delete: data received at or after it is kept (RFC 3339).
    #[schema(format = DateTime)]
    pub requested_at: String,
    /// Who asked: `user:<id>`, `key:<id>`, or `system:upgrade` for the project deletion that the server makes
    /// for a project deleted on an earlier version.
    pub requested_by: String,
    /// When the request last changed (RFC 3339).
    #[schema(format = DateTime)]
    pub updated_at: String,
    /// How many traces the request resolved to, once it has been planned.
    pub traces_resolved: u32,
    /// The places of the reach list, once the request has been planned.
    pub places: Vec<DeletionPlace>,
    /// Why the request failed, or what a request that has not ended waits for and how to fix it (for a
    /// deletion by user: a project salt that the server cannot read; for a project deletion: a read of the
    /// telemetry store or a removal from the object store that failed, which the server tries again, or the servers
    /// of an earlier version that it waits for during a rolling upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub error: Option<String>,
    /// How many times a step of the request failed, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub attempts: Option<u32>,
    /// For a project deletion that has ended: how many times the hourly sweep found rows of the project that
    /// reached the telemetry store after the purge, and deleted them, one per partition each time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub late_purges: Option<u32>,
    /// When the sweep last found such rows (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub late_purged_at: Option<String>,
}

/// The answer of `GET deletion-requests`: newest first.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct DeletionRequestList {
    /// The requests.
    pub data: Vec<DeletionRequest>,
    /// The cursor of the next page; null on the last page.
    pub next_cursor: Option<String>,
}
