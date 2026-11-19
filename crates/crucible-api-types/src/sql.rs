//! The SQL workspace: `projects/{project_id}/sql/...` (contracts 7.4 and 12.6).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::wire::SafeU64;

/// The body of `POST sql/run`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SqlRunRequest {
    /// The query text: one `SELECT` or `WITH` statement that uses the window parameters `{from:DateTime64(6)}`
    /// and `{to:DateTime64(6)}`. It is sent to ClickHouse verbatim.
    pub sql: String,
    /// Start of the time window, bound to `{from}` (RFC 3339).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the time window, bound to `{to}` (RFC 3339).
    #[schema(format = DateTime)]
    pub to: String,
    /// Values of the other `{name:Type}` parameters the text uses, by name, in ClickHouse's text form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub params: Option<BTreeMap<String, String>>,
}

/// The body of `POST sql/saved-queries/{query_id}/run`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SavedQueryRunRequest {
    /// Start of the time window (RFC 3339).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the time window (RFC 3339).
    #[schema(format = DateTime)]
    pub to: String,
    /// Values of the query's declared parameters, by name. A parameter with a default may be left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub params: Option<BTreeMap<String, String>>,
}

/// A download format.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SqlExportFormat {
    /// CSV. Text cells that start with `=`, `+`, `-` or `@` get a leading apostrophe.
    Csv,
    /// One JSON object per line.
    Jsonl,
    /// Apache Parquet.
    Parquet,
}

/// The body of `POST sql/export`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SqlExportRequest {
    /// The query text, as in a run.
    pub sql: String,
    /// Start of the time window (RFC 3339).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the time window (RFC 3339).
    #[schema(format = DateTime)]
    pub to: String,
    /// Values of the other parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub params: Option<BTreeMap<String, String>>,
    /// The file format.
    pub format: SqlExportFormat,
}

/// One column of a result.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlColumn {
    /// The column name.
    pub name: String,
    /// The ClickHouse type.
    #[serde(rename = "type")]
    pub column_type: String,
}

/// What the server read to answer a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlRunStats {
    /// Rows read.
    pub rows_read: SafeU64,
    /// Bytes read.
    pub bytes_read: SafeU64,
    /// Server-side time in milliseconds.
    pub elapsed_ms: SafeU64,
    /// Rows the server expected to read before it ran the query.
    pub estimated_rows: SafeU64,
}

/// The result of a run: a typed grid.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlResult {
    /// The run's id; `DELETE sql/runs/{run_id}` cancels it while it runs.
    pub run_id: String,
    /// The columns.
    pub columns: Vec<SqlColumn>,
    /// The rows, one array of cells per row in column order. A cell is a JSON value in ClickHouse's JSON
    /// form: 64-bit integers are strings, dates are ISO strings.
    #[schema(value_type = Vec<Vec<Object>>)]
    #[ts(type = "Array<Array<unknown>>")]
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Whether rows were left out: the grid holds at most 10,000 rows and 16 MiB.
    pub truncated: bool,
    /// How many cells were cut at 64 KiB; a cut cell ends with `…[cut]`.
    pub cut_cells: SafeU64,
    /// Read statistics.
    pub stats: SqlRunStats,
}

/// Whether the workspace serves requests (also in the session's `features.sql_workspace`).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlWorkspaceStatus {
    /// Whether runs are accepted.
    pub enabled: bool,
    /// Why not: `not_configured`, `probe_pending`, `probe_unavailable` or `isolation_probe_failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub reason: Option<String>,
    /// The checks of the isolation self-probe that failed, by name.
    pub failed_checks: Vec<String>,
}

/// One relation a query may read.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlRelation {
    /// The qualified name, for example `crucible_sql.spans`.
    pub name: String,
    /// One sentence about it.
    pub description: String,
    /// The column to filter with the window.
    pub time_column: String,
    /// Whether a query that does not filter on `time_column` is refused.
    pub window_required: bool,
    /// The columns.
    pub columns: Vec<SqlColumn>,
    /// A query to start from.
    pub example: String,
}

/// The limits of a run, for the editor's hints.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlLimits {
    /// Rows a grid holds.
    pub max_rows: u32,
    /// Bytes a grid holds.
    pub max_bytes: u32,
    /// Bytes of one cell before it is cut.
    pub max_cell_bytes: u32,
    /// Seconds a query may run.
    pub max_execution_s: u32,
    /// Bytes of query text.
    pub max_sql_bytes: u32,
}

/// The answer of `GET sql/schema`: what the workspace offers.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlSchema {
    /// Whether the workspace serves requests.
    pub status: SqlWorkspaceStatus,
    /// The readable relations.
    pub relations: Vec<SqlRelation>,
    /// The limits of a run.
    pub limits: SqlLimits,
}

/// Who made something: a signed-in user or an API key.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlActor {
    /// `user` or `api_key`.
    pub kind: SqlActorKind,
    /// The user id or the API key id.
    pub id: String,
}

/// The kind of an actor.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SqlActorKind {
    /// A signed-in user.
    User,
    /// An API key.
    ApiKey,
}

/// What a run was.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SqlRunKind {
    /// Ad hoc text.
    AdHoc,
    /// A saved query.
    Saved,
    /// An export.
    Export,
}

/// How a run ended.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SqlRunOutcome {
    /// Still running.
    Running,
    /// It returned a result.
    Succeeded,
    /// It failed; `error_code` says how.
    Failed,
    /// It was cancelled.
    Cancelled,
}

/// One run of the history.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlRun {
    /// The run id.
    pub id: String,
    /// Who ran it.
    pub actor: SqlActor,
    /// What it was.
    pub kind: SqlRunKind,
    /// The saved query it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub saved_query_id: Option<String>,
    /// The query text.
    pub sql: String,
    /// When it started.
    #[schema(format = DateTime)]
    pub started_at: String,
    /// When it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub finished_at: Option<String>,
    /// How it ended.
    pub outcome: SqlRunOutcome,
    /// The problem code of a failed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub error_code: Option<String>,
    /// Rows returned.
    pub result_rows: SafeU64,
    /// Whether the result was cut at a limit.
    pub truncated: bool,
    /// Rows the server read.
    pub rows_read: SafeU64,
    /// Server-side time in milliseconds.
    pub elapsed_ms: SafeU64,
}

/// The answer of `GET sql/runs`: the caller's recent runs, newest first (a project admin sees everyone's).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SqlRunList {
    /// The runs.
    pub data: Vec<SqlRun>,
}

/// Who may see a saved query.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SavedQueryVisibility {
    /// Its owner only.
    Private,
    /// Everyone who can read the project.
    Project,
}

/// The type of a declared parameter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SavedQueryParameterType {
    /// Text.
    String,
    /// A whole number.
    Int,
    /// A number.
    Float,
    /// `true` or `false`.
    Bool,
    /// An RFC 3339 time.
    Timestamp,
    /// A UUID.
    Uuid,
}

/// One declared parameter of a saved query.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SavedQueryParameter {
    /// The name, as the text uses it in `{name:Type}`.
    pub name: String,
    /// The type a value is checked against.
    #[serde(rename = "type")]
    pub parameter_type: SavedQueryParameterType,
    /// The value used when a run gives none; without it the parameter is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub default: Option<String>,
}

/// The body of `POST sql/saved-queries` and `PATCH sql/saved-queries/{query_id}`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SavedQueryInput {
    /// The display name, 1 to 120 characters.
    pub name: String,
    /// What it is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub description: Option<String>,
    /// Who may see it.
    pub visibility: SavedQueryVisibility,
    /// The query text.
    pub sql: String,
    /// The declared parameters, besides the window. Every `{name:Type}` of the text other than `from` and
    /// `to` must be declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub parameters: Option<Vec<SavedQueryParameter>>,
}

/// A saved query.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SavedQuery {
    /// The id.
    pub id: String,
    /// Who owns it.
    pub owner: SqlActor,
    /// The display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Who may see it.
    pub visibility: SavedQueryVisibility,
    /// The query text.
    pub sql: String,
    /// The declared parameters.
    pub parameters: Vec<SavedQueryParameter>,
    /// The version; send it as `If-Match` to change the query.
    pub version: SafeU64,
    /// When it was created.
    #[schema(format = DateTime)]
    pub created_at: String,
    /// When it last changed.
    #[schema(format = DateTime)]
    pub updated_at: String,
    /// Whether the caller may change or delete it.
    pub can_edit: bool,
    /// Whether the caller may run it.
    pub can_run: bool,
}

/// The answer of `GET sql/saved-queries`: the saved queries the caller may see.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SavedQueryList {
    /// The queries.
    pub data: Vec<SavedQuery>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_reject_unknown_fields() {
        let ok = r#"{"sql":"SELECT 1","from":"2026-01-01T00:00:00Z","to":"2026-01-02T00:00:00Z"}"#;
        let request: SqlRunRequest = serde_json::from_str(ok).unwrap();
        assert!(request.params.is_none());
        let extra = r#"{"sql":"SELECT 1","from":"a","to":"b","project_id":"x"}"#;
        assert!(serde_json::from_str::<SqlRunRequest>(extra).is_err());
        let export = r#"{"sql":"SELECT 1","from":"a","to":"b","format":"jsonl"}"#;
        assert_eq!(
            serde_json::from_str::<SqlExportRequest>(export)
                .unwrap()
                .format,
            SqlExportFormat::Jsonl
        );
        let parameter = r#"{"name":"n","type":"int","default":"10"}"#;
        let parameter: SavedQueryParameter = serde_json::from_str(parameter).unwrap();
        assert_eq!(parameter.parameter_type, SavedQueryParameterType::Int);
    }

    #[test]
    fn a_result_keeps_cells_as_json_values() {
        let result = SqlResult {
            run_id: "r".to_owned(),
            columns: vec![SqlColumn {
                name: "n".to_owned(),
                column_type: "UInt64".to_owned(),
            }],
            rows: vec![
                vec![serde_json::json!("18446744073709551615")],
                vec![serde_json::Value::Null],
            ],
            truncated: false,
            cut_cells: SafeU64::saturating(0).0,
            stats: SqlRunStats {
                rows_read: SafeU64::saturating(1).0,
                bytes_read: SafeU64::saturating(8).0,
                elapsed_ms: SafeU64::saturating(2).0,
                estimated_rows: SafeU64::saturating(1).0,
            },
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["rows"][0][0], "18446744073709551615");
        assert_eq!(json["columns"][0]["type"], "UInt64");
        assert!(json["rows"][1][0].is_null());
    }
}
