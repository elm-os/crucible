//! The SQL workspace: `projects/{project_id}/sql/...` (contracts 7.4 and 12.6).
//!
//! The handlers only translate: the workspace service authorizes, checks, limits, runs and records. Every
//! refusal carries a sentence that says how to fix the query (contract 1.7), and a query error carries
//! ClickHouse's own message beside it, with the line and column when the server gave them.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, IF_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{
    FieldError, Problem, ProblemCode, SafeU64, SavedQuery, SavedQueryInput, SavedQueryList,
    SavedQueryRunRequest, SqlExportRequest, SqlResult, SqlRunList, SqlRunRequest, SqlSchema,
};
use crucible_records::{RequestId, SavedQueryId, SqlRunId, Timestamp};
use crucible_sqlws as sqlws;
use crucible_sqlws::{
    ExportFormat, RunInput, RunOutput, SavedInput, SqlAction, SqlActor, SqlCaller, SqlProblem,
    SqlRole, SqlWorkspace,
};
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::params::Params;
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    let base = "/projects/{project_id}/sql";
    Router::new()
        .route(&format!("{base}/schema"), get(get_sql_schema))
        .route(&format!("{base}/run"), post(run_sql))
        .route(&format!("{base}/export"), post(export_sql))
        .route(&format!("{base}/runs"), get(list_sql_runs))
        .route(&format!("{base}/runs/{{run_id}}"), delete(cancel_sql_run))
        .route(
            &format!("{base}/saved-queries"),
            get(list_saved_queries).post(create_saved_query),
        )
        .route(
            &format!("{base}/saved-queries/{{query_id}}"),
            get(get_saved_query)
                .patch(update_saved_query)
                .delete(delete_saved_query),
        )
        .route(
            &format!("{base}/saved-queries/{{query_id}}/run"),
            post(run_saved_query),
        )
}

/// The methods a SQL path supports, for the `Allow` header of a 405.
pub(crate) fn allowed_methods(path: &str) -> Option<&'static str> {
    let (_, rest) = path.split_once("/sql/")?;
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        ["schema"] | ["runs"] => Some("GET, HEAD"),
        ["run"] | ["export"] | ["saved-queries", _, "run"] => Some("POST"),
        ["runs", _] => Some("DELETE"),
        ["saved-queries"] => Some("GET, HEAD, POST"),
        ["saved-queries", _] => Some("GET, HEAD, PATCH, DELETE"),
        _ => None,
    }
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    get_sql_schema,
    run_sql,
    export_sql,
    list_sql_runs,
    cancel_sql_run,
    list_saved_queries,
    create_saved_query,
    get_saved_query,
    update_saved_query,
    delete_saved_query,
    run_saved_query,
))]
pub(super) struct Paths;

/// The caller of a request. A project key acts as a member of its project: it runs ad hoc SQL and owns what it
/// saves.
fn caller(project: &Project) -> SqlCaller {
    SqlCaller {
        scope: project.ctx.scope(),
        actor: SqlActor::ApiKey(project.api_key_id),
        role: SqlRole::Member,
    }
}

/// The problem document of a workspace refusal: the fix is the detail; ClickHouse's own message, when there is
/// one, is the first entry of `errors`, pointing at the text.
fn sql_problem(request_id: RequestId, problem: SqlProblem) -> ApiError {
    let code = ProblemCode::parse(problem.code.as_str()).unwrap_or(ProblemCode::InternalError);
    if code == ProblemCode::InternalError {
        tracing::error!(%request_id, detail = %problem.fix, "the SQL workspace failed");
        return ApiError::new(request_id, code, "the SQL workspace failed");
    }
    let mut error = ApiError::new(request_id, code, problem.fix);
    if let Some(message) = problem.server_message {
        error = error.with_errors(vec![FieldError {
            pointer: "/sql".to_owned(),
            code: "clickhouse".to_owned(),
            message,
        }]);
    }
    error
        .with_position(problem.line, problem.column)
        .with_retry_after(problem.retry_after)
        .with_reason(problem.reason)
}

fn workspace(state: &ApiState, request_id: RequestId) -> Result<&SqlWorkspace, ApiError> {
    state.sql.as_ref().ok_or_else(|| {
        sql_problem(
            request_id,
            SqlProblem::unavailable(
                "not_configured",
                "The SQL workspace is not configured on this server. It needs ClickHouse with access management \
                 and the Postgres control plane; an operator sets both and runs `crucible migrate`.",
            ),
        )
    })
}

/// A JSON body, or `validation_failed` that says what is wrong with it.
fn body<T>(request_id: RequestId, body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|rejection| {
        let error = field_error("", "invalid_body", &rejection.body_text());
        ApiError::validation(request_id, vec![error])
    })
}

fn timestamp(request_id: RequestId, name: &str, text: &str) -> Result<Timestamp, ApiError> {
    Timestamp::parse_rfc3339(text).map_err(|_| {
        let error = field_error(
            name,
            "invalid_timestamp",
            "write an RFC 3339 time, for example 2026-01-31T12:00:00Z",
        );
        ApiError::validation(request_id, vec![error])
    })
}

fn path_id<T>(
    project: &Project,
    name: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T, ApiError> {
    project
        .path
        .get(name)
        .and_then(|text| parse(text))
        .ok_or_else(|| ApiError::not_found(project.request_id))
}

fn safe(value: u64) -> SafeU64 {
    SafeU64::saturating(value).0
}

fn actor(actor: SqlActor) -> wire::SqlActor {
    match actor {
        SqlActor::User(id) => wire::SqlActor {
            kind: wire::SqlActorKind::User,
            id: id.to_string(),
        },
        SqlActor::ApiKey(id) => wire::SqlActor {
            kind: wire::SqlActorKind::ApiKey,
            id: id.to_string(),
        },
    }
}

fn result(output: RunOutput) -> SqlResult {
    SqlResult {
        run_id: output.run_id.to_string(),
        columns: output
            .grid
            .columns
            .into_iter()
            .map(|column| wire::SqlColumn {
                name: column.name,
                column_type: column.ty,
            })
            .collect(),
        rows: output.grid.rows,
        truncated: output.grid.truncated,
        cut_cells: safe(output.grid.cut_cells),
        stats: wire::SqlRunStats {
            rows_read: safe(output.grid.stats.rows_read),
            bytes_read: safe(output.grid.stats.bytes_read),
            elapsed_ms: safe(output.grid.stats.elapsed_ms),
            estimated_rows: safe(output.estimate.rows),
        },
    }
}

fn saved_query(caller: SqlCaller, query: sqlws::SavedQuery) -> SavedQuery {
    let can_edit = sqlws::allowed(caller.role, caller.actor, SqlAction::EditSaved(&query));
    let can_run = sqlws::allowed(caller.role, caller.actor, SqlAction::RunSaved(&query));
    SavedQuery {
        id: query.id.to_string(),
        owner: actor(query.owner),
        name: query.name,
        description: query.description,
        visibility: match query.visibility {
            sqlws::SqlVisibility::Private => wire::SavedQueryVisibility::Private,
            sqlws::SqlVisibility::Project => wire::SavedQueryVisibility::Project,
        },
        sql: query.sql,
        parameters: query
            .parameters
            .into_iter()
            .map(|parameter| wire::SavedQueryParameter {
                name: parameter.name,
                parameter_type: match parameter.ty {
                    sqlws::SqlParamType::String => wire::SavedQueryParameterType::String,
                    sqlws::SqlParamType::Int => wire::SavedQueryParameterType::Int,
                    sqlws::SqlParamType::Float => wire::SavedQueryParameterType::Float,
                    sqlws::SqlParamType::Bool => wire::SavedQueryParameterType::Bool,
                    sqlws::SqlParamType::Timestamp => wire::SavedQueryParameterType::Timestamp,
                    sqlws::SqlParamType::Uuid => wire::SavedQueryParameterType::Uuid,
                },
                default: parameter.default,
            })
            .collect(),
        version: safe(query.version),
        created_at: query.created_at.to_string(),
        updated_at: query.updated_at.to_string(),
        can_edit,
        can_run,
    }
}

fn saved_input(input: SavedQueryInput) -> SavedInput {
    SavedInput {
        name: input.name,
        description: input.description.unwrap_or_default(),
        visibility: match input.visibility {
            wire::SavedQueryVisibility::Private => sqlws::SqlVisibility::Private,
            wire::SavedQueryVisibility::Project => sqlws::SqlVisibility::Project,
        },
        sql: input.sql,
        parameters: input
            .parameters
            .unwrap_or_default()
            .into_iter()
            .map(|parameter| sqlws::SavedParam {
                name: parameter.name,
                ty: match parameter.parameter_type {
                    wire::SavedQueryParameterType::String => sqlws::SqlParamType::String,
                    wire::SavedQueryParameterType::Int => sqlws::SqlParamType::Int,
                    wire::SavedQueryParameterType::Float => sqlws::SqlParamType::Float,
                    wire::SavedQueryParameterType::Bool => sqlws::SqlParamType::Bool,
                    wire::SavedQueryParameterType::Timestamp => sqlws::SqlParamType::Timestamp,
                    wire::SavedQueryParameterType::Uuid => sqlws::SqlParamType::Uuid,
                },
                default: parameter.default,
            })
            .collect(),
    }
}

fn run_input(
    request_id: RequestId,
    sql: String,
    from: &str,
    to: &str,
    params: Option<BTreeMap<String, String>>,
) -> Result<RunInput, ApiError> {
    Ok(RunInput {
        sql,
        from: timestamp(request_id, "from", from)?,
        to: timestamp(request_id, "to", to)?,
        params: params.unwrap_or_default(),
    })
}

/// Get the SQL schema
///
/// The relations a query may read, each with its columns, its time column and a query to start from; the
/// limits of a run; and whether the workspace is available.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/sql/schema",
    operation_id = "get_sql_schema",
    tag = "sql",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The schema.", body = SqlSchema),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_sql_schema(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SqlSchema>, ApiError> {
    Params::new(project.request_id, &uri)?.finish()?;
    let status = match &state.sql {
        Some(workspace) => {
            let status = workspace.status();
            wire::SqlWorkspaceStatus {
                enabled: status.enabled,
                reason: status.reason.map(str::to_owned),
                failed_checks: status
                    .failed_checks
                    .iter()
                    .map(|c| (*c).to_owned())
                    .collect(),
            }
        }
        None => wire::SqlWorkspaceStatus {
            enabled: false,
            reason: Some("not_configured".to_owned()),
            failed_checks: Vec::new(),
        },
    };
    let limits = sqlws::INTERACTIVE_LIMITS;
    let narrow = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    Ok(Json(SqlSchema {
        status,
        relations: sqlws::schema()
            .into_iter()
            .map(|relation| wire::SqlRelation {
                name: relation.name,
                description: relation.description.to_owned(),
                time_column: relation.time_column.to_owned(),
                window_required: relation.window_required,
                columns: relation
                    .columns
                    .into_iter()
                    .map(|(name, ty)| wire::SqlColumn {
                        name: name.to_owned(),
                        column_type: ty,
                    })
                    .collect(),
                example: relation.example,
            })
            .collect(),
        limits: wire::SqlLimits {
            max_rows: narrow(limits.max_rows),
            max_bytes: narrow(limits.max_bytes),
            max_cell_bytes: narrow(limits.max_cell_bytes),
            max_execution_s: 30,
            max_sql_bytes: narrow(sqlws::lexer::MAX_SQL_BYTES),
        },
    }))
}

/// Run SQL
///
/// Runs one read-only statement as the project's own ClickHouse identity and returns the result as a typed
/// grid of at most 10,000 rows. The text must filter on the time window with `{from:DateTime64(6)}` and
/// `{to:DateTime64(6)}`. Every refusal says how to fix the query in `detail`; a ClickHouse error is in
/// `errors[0].message`, with `line` and `column` when the server gave them.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/sql/run",
    operation_id = "run_sql",
    tag = "sql",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    request_body = SqlRunRequest,
    responses(
        (status = 200, description = "The result grid.", body = SqlResult),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`sql_forbidden`: the statement writes, changes a setting or reads something the workspace does not offer. `forbidden`: the caller's role is too low.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the run was cancelled.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`sql_invalid`, `sql_time_window_required`, `query_too_broad`, `query_memory_exceeded` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`sql_quota_exceeded`: a concurrency, rate or hourly limit; see `retry_after_s`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable`, with a `reason`; or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn run_sql(
    State(state): State<ApiState>,
    project: Project,
    request: Result<Json<SqlRunRequest>, JsonRejection>,
) -> Result<Json<SqlResult>, ApiError> {
    let rid = project.request_id;
    let request = body(rid, request)?;
    let input = run_input(rid, request.sql, &request.from, &request.to, request.params)?;
    let output = workspace(&state, rid)?
        .run(caller(&project), &input)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(Json(result(output)))
}

/// Export SQL
///
/// Runs the statement like `sql/run` and returns the result as a file: CSV, JSON lines or Parquet, under the
/// interactive limits. In CSV, text cells that start with `=`, `+`, `-` or `@` get a leading apostrophe, so
/// a spreadsheet does not evaluate them.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/sql/export",
    operation_id = "export_sql",
    tag = "sql",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    request_body = SqlExportRequest,
    responses(
        (status = 200, description = "The file.", content_type = "application/octet-stream"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`sql_forbidden` or `forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`sql_invalid`, `sql_time_window_required`, `query_too_broad`, `query_memory_exceeded` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`sql_quota_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn export_sql(
    State(state): State<ApiState>,
    project: Project,
    request: Result<Json<SqlExportRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let request = body(rid, request)?;
    let format = match request.format {
        wire::SqlExportFormat::Csv => ExportFormat::Csv,
        wire::SqlExportFormat::Jsonl => ExportFormat::JsonLines,
        wire::SqlExportFormat::Parquet => ExportFormat::Parquet,
    };
    let input = run_input(rid, request.sql, &request.from, &request.to, request.params)?;
    let bytes = workspace(&state, rid)?
        .export(caller(&project), &input, format)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    let disposition = format!(
        "attachment; filename=\"crucible-sql.{}\"",
        format.extension()
    );
    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// List SQL runs
///
/// The caller's recent runs, newest first, with how each ended.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/sql/runs",
    operation_id = "list_sql_runs",
    tag = "sql",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("limit" = Option<u32>, Query, description = "How many runs: 1 to 200, default 50.", minimum = 1, maximum = 200),
    ),
    responses(
        (status = 200, description = "The runs.", body = SqlRunList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_sql_runs(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SqlRunList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let limit = params.limit(sqlws::MAX_HISTORY);
    params.finish()?;
    let runs = workspace(&state, rid)?
        .history(caller(&project), limit.unwrap_or(50))
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    let data = runs
        .into_iter()
        .map(|run| {
            let end = run.end.as_ref();
            wire::SqlRun {
                id: run.id.to_string(),
                actor: actor(run.actor),
                kind: match run.kind {
                    sqlws::SqlRunKind::AdHoc => wire::SqlRunKind::AdHoc,
                    sqlws::SqlRunKind::Saved => wire::SqlRunKind::Saved,
                    sqlws::SqlRunKind::Export => wire::SqlRunKind::Export,
                },
                saved_query_id: run.saved_query_id.map(|id| id.to_string()),
                started_at: run.started_at.to_string(),
                finished_at: end.map(|end| end.finished_at.to_string()),
                outcome: match end.map(|end| end.outcome) {
                    None | Some(sqlws::SqlRunOutcome::Running) => wire::SqlRunOutcome::Running,
                    Some(sqlws::SqlRunOutcome::Succeeded) => wire::SqlRunOutcome::Succeeded,
                    Some(sqlws::SqlRunOutcome::Failed) => wire::SqlRunOutcome::Failed,
                    Some(sqlws::SqlRunOutcome::Cancelled) => wire::SqlRunOutcome::Cancelled,
                },
                error_code: end.and_then(|end| end.error_code.clone()),
                result_rows: safe(end.map_or(0, |end| end.result_rows)),
                truncated: end.is_some_and(|end| end.truncated),
                rows_read: safe(end.map_or(0, |end| end.rows_read)),
                elapsed_ms: safe(end.map_or(0, |end| end.elapsed_ms)),
                sql: run.sql,
            }
        })
        .collect();
    Ok(Json(SqlRunList { data }))
}

/// Cancel a SQL run
///
/// Stops a running statement. The run's own request then ends with `conflict`. Cancelling a run that has
/// ended does nothing.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/sql/runs/{run_id}",
    operation_id = "cancel_sql_run",
    tag = "sql",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("run_id" = String, Path, description = "The run id (a UUID)."),
    ),
    responses(
        (status = 204, description = "The cancel was issued, or the run had ended."),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: only the run's starter or a project admin cancels it.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn cancel_sql_run(
    State(state): State<ApiState>,
    project: Project,
) -> Result<StatusCode, ApiError> {
    let rid = project.request_id;
    let run_id = path_id(&project, "run_id", |text| SqlRunId::parse(text).ok())?;
    workspace(&state, rid)?
        .cancel(caller(&project), run_id)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(StatusCode::NO_CONTENT)
}

/// List saved queries
///
/// The saved queries the caller may see: the project-visible ones and its own.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/sql/saved-queries",
    operation_id = "list_saved_queries",
    tag = "sql",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The saved queries.", body = SavedQueryList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_saved_queries(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SavedQueryList>, ApiError> {
    let rid = project.request_id;
    Params::new(rid, &uri)?.finish()?;
    let caller = caller(&project);
    let queries = workspace(&state, rid)?
        .list_saved(caller)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(Json(SavedQueryList {
        data: queries
            .into_iter()
            .map(|query| saved_query(caller, query))
            .collect(),
    }))
}

fn with_etag(query: SavedQuery, status: StatusCode) -> Response {
    let etag = format!("\"{}\"", query.version.get());
    let mut response = (status, Json(query)).into_response();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(ETAG, value);
    }
    response
}

/// Create a saved query
///
/// Saves a query with its declared parameters. The text is checked like a run's, and every `{name:Type}` of
/// the text other than the window must be declared.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/sql/saved-queries",
    operation_id = "create_saved_query",
    tag = "sql",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    request_body = SavedQueryInput,
    responses(
        (status = 201, description = "The saved query; `ETag` is its version.", body = SavedQuery),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: saving needs the member role.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`sql_invalid`, `sql_time_window_required` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn create_saved_query(
    State(state): State<ApiState>,
    project: Project,
    request: Result<Json<SavedQueryInput>, JsonRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let input = saved_input(body(rid, request)?);
    let caller = caller(&project);
    let query = workspace(&state, rid)?
        .create_saved(caller, input)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(with_etag(saved_query(caller, query), StatusCode::CREATED))
}

/// Get a saved query
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}",
    operation_id = "get_saved_query",
    tag = "sql",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("query_id" = String, Path, description = "The saved query id (a UUID)."),
    ),
    responses(
        (status = 200, description = "The saved query; `ETag` is its version.", body = SavedQuery),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no such query, or one the caller may not see.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_saved_query(
    State(state): State<ApiState>,
    project: Project,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let id = path_id(&project, "query_id", |text| SavedQueryId::parse(text).ok())?;
    let caller = caller(&project);
    let query = workspace(&state, rid)?
        .saved(caller, id)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(with_etag(saved_query(caller, query), StatusCode::OK))
}

/// The version an `If-Match` header names: `"3"`, or `3`.
fn if_match(request_id: RequestId, headers: &HeaderMap) -> Result<u64, ApiError> {
    let Some(value) = headers.get(IF_MATCH) else {
        return Err(ApiError::new(
            request_id,
            ProblemCode::PreconditionRequired,
            "send the saved query's version as `If-Match`, as its `ETag` gave it",
        ));
    };
    value
        .to_str()
        .ok()
        .and_then(|text| text.trim().trim_matches('"').parse().ok())
        .ok_or_else(|| {
            ApiError::new(
                request_id,
                ProblemCode::PreconditionFailed,
                "`If-Match` is not a version; send the `ETag` of the saved query",
            )
        })
}

/// Update a saved query
///
/// Replaces the query's fields if its version is still the one in `If-Match`.
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}",
    operation_id = "update_saved_query",
    tag = "sql",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("query_id" = String, Path, description = "The saved query id (a UUID)."),
        ("If-Match" = String, Header, description = "The version the change is based on, as `ETag` gave it."),
    ),
    request_body = SavedQueryInput,
    responses(
        (status = 200, description = "The changed query; `ETag` is its new version.", body = SavedQuery),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: only the owner, or a project admin for a project-visible query.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`: the query changed since that version.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`sql_invalid`, `sql_time_window_required` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`: `If-Match` is missing.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn update_saved_query(
    State(state): State<ApiState>,
    project: Project,
    headers: HeaderMap,
    request: Result<Json<SavedQueryInput>, JsonRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let id = path_id(&project, "query_id", |text| SavedQueryId::parse(text).ok())?;
    let version = if_match(rid, &headers)?;
    let input = saved_input(body(rid, request)?);
    let caller = caller(&project);
    let query = workspace(&state, rid)?
        .update_saved(caller, id, version, input)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(with_etag(saved_query(caller, query), StatusCode::OK))
}

/// Delete a saved query
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}",
    operation_id = "delete_saved_query",
    tag = "sql",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("query_id" = String, Path, description = "The saved query id (a UUID)."),
    ),
    responses(
        (status = 204, description = "Deleted."),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: only the owner, or a project admin for a project-visible query.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn delete_saved_query(
    State(state): State<ApiState>,
    project: Project,
) -> Result<StatusCode, ApiError> {
    let rid = project.request_id;
    let id = path_id(&project, "query_id", |text| SavedQueryId::parse(text).ok())?;
    workspace(&state, rid)?
        .delete_saved(caller(&project), id)
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Run a saved query
///
/// Runs the stored text with the window and the declared parameters only. Viewers may run project-visible
/// saved queries.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}/run",
    operation_id = "run_saved_query",
    tag = "sql",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("query_id" = String, Path, description = "The saved query id (a UUID)."),
    ),
    request_body = SavedQueryRunRequest,
    responses(
        (status = 200, description = "The result grid.", body = SqlResult),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`sql_forbidden` or `forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no such query, or one the caller may not see.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the run was cancelled.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`sql_invalid`, `sql_time_window_required`, `query_too_broad`, `query_memory_exceeded` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`sql_quota_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`sql_workspace_unavailable` or `control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn run_saved_query(
    State(state): State<ApiState>,
    project: Project,
    request: Result<Json<SavedQueryRunRequest>, JsonRejection>,
) -> Result<Json<SqlResult>, ApiError> {
    let rid = project.request_id;
    let id = path_id(&project, "query_id", |text| SavedQueryId::parse(text).ok())?;
    let request = body(rid, request)?;
    let from = timestamp(rid, "from", &request.from)?;
    let to = timestamp(rid, "to", &request.to)?;
    let output = workspace(&state, rid)?
        .run_saved(
            caller(&project),
            id,
            from,
            to,
            &request.params.unwrap_or_default(),
        )
        .await
        .map_err(|problem| sql_problem(rid, problem))?;
    Ok(Json(result(output)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_sql_path_names_its_methods() {
        let base = "/api/v1/projects/p/sql";
        for (path, methods) in [
            ("schema", "GET, HEAD"),
            ("runs", "GET, HEAD"),
            ("run", "POST"),
            ("export", "POST"),
            ("runs/r", "DELETE"),
            ("saved-queries", "GET, HEAD, POST"),
            ("saved-queries/q", "GET, HEAD, PATCH, DELETE"),
            ("saved-queries/q/run", "POST"),
        ] {
            assert_eq!(
                allowed_methods(&format!("{base}/{path}")),
                Some(methods),
                "{path}"
            );
        }
        assert_eq!(allowed_methods("/api/v1/projects/p/traces"), None);
        assert_eq!(allowed_methods(&format!("{base}/nope")), None);
    }

    #[test]
    fn if_match_reads_a_quoted_or_bare_version() {
        let rid = RequestId::from_bytes([1; 16]);
        let mut headers = HeaderMap::new();
        assert!(if_match(rid, &headers).is_err());
        headers.insert(IF_MATCH, HeaderValue::from_static("\"3\""));
        assert_eq!(if_match(rid, &headers).ok(), Some(3));
        headers.insert(IF_MATCH, HeaderValue::from_static("4"));
        assert_eq!(if_match(rid, &headers).ok(), Some(4));
        headers.insert(IF_MATCH, HeaderValue::from_static("*"));
        assert!(if_match(rid, &headers).is_err());
    }
}
