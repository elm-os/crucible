//! Exports: `projects/{project_id}/exports...` (contracts 12.6 and 12.8).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::wire::SafeU64;

/// An export format (contract 12.8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "kebab-case")]
pub enum ExportFormat {
    /// One JSON object per span with the keys `schema`, `span` and `facts`: every field of the record, its MCP
    /// operation and the effective facts of the span and its trace.
    CrucibleJsonl,
    /// One OTLP JSON document per span, with the OTel GenAI keys and every residual attribute.
    OtlpJson,
}

/// The body of `POST exports`: what to export and how.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// The format.
    pub format: ExportFormat,
    /// Start of the window over span start times (RFC 3339, inclusive).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the window (RFC 3339, exclusive).
    #[schema(format = DateTime)]
    pub to: String,
    /// Only spans of these traces (32 hexadecimal characters each), at most 10,000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub trace_ids: Option<Vec<String>>,
    /// Only spans of this user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub user_id: Option<String>,
    /// Only spans of this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// Compress the file with gzip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub gzip: Option<bool>,
    /// With `otlp-json`, write spans that have a field cut by a size cap, as opaque text, instead of skipping
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub keep_truncated: Option<bool>,
}

/// Where an export is in its life (contract 12.8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ExportState {
    /// Waiting for a node to start it.
    Queued,
    /// A node is writing the file.
    Running,
    /// The file is written and can be downloaded until `expires_at`.
    Ready,
    /// It ended without a file; `error_code` says why.
    Failed,
    /// The file expired, or a deletion request the export predates expired it.
    Expired,
}

/// An export.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Export {
    /// The export id.
    pub id: String,
    /// The format.
    pub format: ExportFormat,
    /// Where it is in its life.
    pub state: ExportState,
    /// Start of the window over span start times.
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the window.
    #[schema(format = DateTime)]
    pub to: String,
    /// How many traces it is narrowed to; 0 when it is not narrowed to traces.
    pub trace_count: SafeU64,
    /// Only spans of this user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub user_id: Option<String>,
    /// Only spans of this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// Whether the file is gzip-compressed.
    pub gzip: bool,
    /// Whether spans with a field cut by a size cap are kept (`otlp-json`).
    pub keep_truncated: bool,
    /// When it was created.
    #[schema(format = DateTime)]
    pub created_at: String,
    /// The arrival bound of its reads: only spans received at or before it are exported.
    #[schema(format = DateTime)]
    pub as_of: String,
    /// Spans written so far, or in all once it is ready.
    pub spans: SafeU64,
    /// Spans the format left out (`otlp-json` skips spans with a cut field unless asked to keep them).
    pub skipped: SafeU64,
    /// Bytes written so far, or the file's length once it is ready.
    pub bytes: SafeU64,
    /// SHA-256 of the file, lowercase hex, once it is ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub checksum: Option<String>,
    /// When the file stops being downloadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub expires_at: Option<String>,
    /// Why a failed export ended without a file: `query_timeout`, `query_memory_exceeded`, `query_too_broad`
    /// or `export_too_large` (narrow the window or the selection), `project_deleted`, `attempts_exhausted` or
    /// `internal_error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub error_code: Option<String>,
    /// The attempts the export job has made at writing the file; 0 before the first. A failed attempt is tried
    /// again after a pause, up to 12 attempts.
    pub attempts: u32,
    /// How long the export has waited for its file, in whole seconds since it was created, while it is queued or
    /// running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub waited_secs: Option<SafeU64>,
    /// What went wrong in the latest failed attempt of a queued or running export, which a later attempt tries
    /// again, or why a failed export ended: what happened and what to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub last_error: Option<String>,
    /// The path of the download, under the same origin, once it is ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub download_path: Option<String>,
    /// Whether a count was cut to 2^53 - 1.
    pub saturated: bool,
}

/// The answer of `GET exports`: the project's newest exports, newest first, at most 50.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ExportList {
    /// The exports.
    pub data: Vec<Export>,
}
