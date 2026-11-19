//! Exports (contracts 11.9, 12.5, 12.6 and 12.8): `GET` and `POST projects/{project_id}/exports`,
//! `GET .../exports/{export_id}` and `GET .../exports/{export_id}/download`.
//!
//! An export is a job. The POST checks the request, creates the export with a fixed `as_of` and answers 202 at
//! once, under the project's limit of 5 per hour and the request's `Idempotency-Key`; the export job writes the
//! file; the download streams it from this node's disk, so it is not cut by the request deadline. Exports need
//! the control plane, which holds them.

use axum::body::Body;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Export, ExportList, ExportRequest, Problem, ProblemCode, Saturation};
use crucible_control::{ExportFormat, ExportJob, TxError};
use crucible_export::{
    CHUNK_BYTES, CreateError, CreateExport, Created, DownloadError, EXPORTS_PER_HOUR, ExportState,
    Exports, MAX_TRACE_IDS, Options, Selector,
};
use crucible_records::{ExportJobId, RequestId, TimeWindow, Timestamp, TraceId};
use utoipa::OpenApi;

use crate::access::{ApiState, CreateExports, Principal, Read, Scoped};
use crate::idempotency;
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/projects/{project_id}/exports",
            get(list_exports).post(create_export),
        )
        .route(
            "/projects/{project_id}/exports/{export_id}",
            get(get_export),
        )
        .route(
            "/projects/{project_id}/exports/{export_id}/download",
            get(download_export),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(list_exports, create_export, get_export, download_export))]
pub(super) struct Paths;

/// The most exports a list shows.
const LIST_LIMIT: u32 = 50;

fn service(state: &ApiState, rid: RequestId) -> Result<&Exports, ApiError> {
    state.exports.as_ref().ok_or_else(|| {
        ApiError::new(
            rid,
            ProblemCode::FeatureDisabled,
            "exports need the control plane, which holds them; set postgres.url (CRUCIBLE_POSTGRES_URL)",
        )
    })
}

fn tx_problem(rid: RequestId, error: &TxError) -> ApiError {
    match error {
        TxError::Unavailable(_) | TxError::CommitUnknown | TxError::SerializationFailure => {
            ApiError::new(
                rid,
                ProblemCode::ControlPlaneUnavailable,
                "the control plane (Postgres) is unavailable; try again shortly",
            )
            .with_retry_after(5)
        }
        other => {
            tracing::error!(request_id = %rid, error = %other, "an export call to the control plane failed");
            ApiError::new(rid, ProblemCode::InternalError, "internal error")
        }
    }
}

fn format_to_wire(format: ExportFormat) -> wire::ExportFormat {
    match format {
        ExportFormat::CrucibleJsonl => wire::ExportFormat::CrucibleJsonl,
        ExportFormat::OtlpJson => wire::ExportFormat::OtlpJson,
    }
}

const fn state_to_wire(state: ExportState) -> wire::ExportState {
    match state {
        ExportState::Queued => wire::ExportState::Queued,
        ExportState::Running => wire::ExportState::Running,
        ExportState::Ready => wire::ExportState::Ready,
        ExportState::Failed => wire::ExportState::Failed,
        ExportState::Expired => wire::ExportState::Expired,
    }
}

/// The DTO of an export at `now`: what the REST routes and the MCP server's export tools show.
pub fn export_dto(job: &ExportJob, now: Timestamp) -> Export {
    let state = ExportState::of(job, now);
    let selector = Selector::from_text(&job.new.selector).ok();
    let options = Options::from_text(&job.new.options).unwrap_or_default();
    let mut saturation = Saturation::default();
    let window = selector.as_ref().map(|s| s.window);
    let id = job.new.id;
    let error_code = match state {
        ExportState::Failed => Some(
            job.error
                .clone()
                .unwrap_or_else(|| "attempts_exhausted".to_owned()),
        ),
        _ => None,
    };
    Export {
        id: id.to_string(),
        format: format_to_wire(job.new.format),
        state: state_to_wire(state),
        from: window.map(|w| w.from().to_string()).unwrap_or_default(),
        to: window.map(|w| w.to().to_string()).unwrap_or_default(),
        trace_count: saturation.u64(selector.as_ref().map_or(0, |s| s.trace_ids.len() as u64)),
        user_id: selector.as_ref().and_then(|s| s.user_id.clone()),
        session_id: selector.as_ref().and_then(|s| s.session_id.clone()),
        gzip: options.gzip,
        keep_truncated: options.keep_truncated,
        created_at: job.new.created_at.to_string(),
        as_of: job.new.as_of.to_string(),
        spans: saturation.u64(job.spans),
        skipped: saturation.u64(job.skipped),
        bytes: saturation.u64(job.bytes),
        checksum: job.checksum.clone().filter(|_| state == ExportState::Ready),
        expires_at: job.expires_at.map(|t| t.to_string()),
        error_code,
        download_path: (state == ExportState::Ready).then(|| {
            format!(
                "{}/projects/{}/exports/{id}/download",
                crate::BASE_PATH,
                job.new.project_id
            )
        }),
        saturated: saturation.saturated(),
    }
}

/// Who creates an export: the principal its `Idempotency-Key` belongs to.
pub(crate) fn created_by(principal: &Principal) -> String {
    match principal {
        Principal::Key(grant) => format!("api_key:{}", grant.principal.api_key_id),
        Principal::Session(session) => format!("user:{}", session.user.id),
    }
}

/// The selection and options of a request, or `validation_failed` naming each wrong field.
pub(crate) fn checked(
    rid: RequestId,
    request: &ExportRequest,
) -> Result<(ExportFormat, Selector, Options), ApiError> {
    let mut errors = Vec::new();
    let mut time = |name: &str, text: &str| {
        Timestamp::parse_rfc3339(text).ok().or_else(|| {
            errors.push(field_error(
                name,
                "invalid_timestamp",
                "write an RFC 3339 time, for example 2026-11-17T09:30:00Z",
            ));
            None
        })
    };
    let (from, to) = (time("from", &request.from), time("to", &request.to));
    let window = match (from, to) {
        (Some(from), Some(to)) => TimeWindow::new(from, to).ok().or_else(|| {
            errors.push(field_error(
                "to",
                "invalid_window",
                "`to` must be after `from`",
            ));
            None
        }),
        _ => None,
    };
    let ids = request.trace_ids.as_deref().unwrap_or_default();
    if ids.len() > MAX_TRACE_IDS {
        errors.push(field_error(
            "trace_ids",
            "too_many",
            &format!(
                "at most {MAX_TRACE_IDS} trace ids; export the window instead, or split the list"
            ),
        ));
    }
    let mut trace_ids = Vec::with_capacity(ids.len().min(MAX_TRACE_IDS));
    for (at, id) in ids.iter().enumerate().take(MAX_TRACE_IDS) {
        match TraceId::parse(id) {
            Ok(id) => trace_ids.push(id),
            Err(_) => errors.push(field_error(
                &format!("trace_ids/{at}"),
                "invalid_trace_id",
                "a trace id is 32 hexadecimal characters",
            )),
        }
    }
    let format = match request.format {
        wire::ExportFormat::CrucibleJsonl => ExportFormat::CrucibleJsonl,
        wire::ExportFormat::OtlpJson => ExportFormat::OtlpJson,
    };
    match window {
        Some(window) if errors.is_empty() => Ok((
            format,
            Selector {
                window,
                trace_ids,
                user_id: request.user_id.clone(),
                session_id: request.session_id.clone(),
            },
            Options {
                gzip: request.gzip.unwrap_or(false),
                keep_truncated: request.keep_truncated.unwrap_or(false),
            },
        )),
        _ => Err(ApiError::validation(rid, errors)),
    }
}

/// The problem of a refused create.
pub(crate) fn create_problem(rid: RequestId, error: &CreateError) -> ApiError {
    match error {
        CreateError::RateLimited { retry_after_s } => ApiError::new(
            rid,
            ProblemCode::RateLimited,
            format!(
                "a project may create {EXPORTS_PER_HOUR} exports per hour; retry after the wait"
            ),
        )
        .with_retry_after(*retry_after_s)
        .with_limit("exports_per_hour", EXPORTS_PER_HOUR as u32),
        CreateError::KeyReused => ApiError::new(
            rid,
            ProblemCode::IdempotencyKeyReused,
            "this `Idempotency-Key` was used for another request; send a new key",
        ),
        CreateError::Store(error) => tx_problem(rid, error),
    }
}

fn path_export(project: &Scoped<impl Send + Sync>) -> Result<ExportJobId, ApiError> {
    project
        .path
        .get("export_id")
        .and_then(|id| ExportJobId::parse(id).ok())
        .ok_or_else(|| no_export(project.request_id))
}

/// `not_found` for an export id that names no export of the project.
fn no_export(rid: RequestId) -> ApiError {
    ApiError::new(
        rid,
        ProblemCode::NotFound,
        "no export of this project has this id; the project's export list has the ids",
    )
}

/// List exports
///
/// The project's newest exports, newest first, at most 50, each with where it is in its life.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/exports",
    operation_id = "list_exports",
    tag = "exports",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The exports.", body = ExportList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project; or `feature_disabled`: no control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_exports(
    State(state): State<ApiState>,
    project: Scoped<Read>,
) -> Result<Json<ExportList>, ApiError> {
    let rid = project.request_id;
    let exports = service(&state, rid)?;
    let jobs = exports
        .list(project.scope, LIST_LIMIT)
        .await
        .map_err(|e| tx_problem(rid, &e))?;
    let now = exports.now();
    Ok(Json(ExportList {
        data: jobs.iter().map(|job| export_dto(job, now)).collect(),
    }))
}

/// Create an export
///
/// Starts an export of the project's spans in a window, optionally narrowed to traces, a user or a session,
/// in the `crucible-jsonl` or `otlp-json` format. It answers at once; read the export to follow it, and download
/// the file once it is ready. Spans received after this request are not included. A project may create 5
/// exports per hour; an `Idempotency-Key` makes a retry answer with the export the first request created.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/exports",
    operation_id = "create_export",
    tag = "exports",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`; a repeat with the same body answers with the export the first request created."),
    ),
    request_body = ExportRequest,
    responses(
        (status = 202, description = "The export, queued.", body = Export),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: creating exports needs the `export` permission (project member, or a key with the `read` scope).", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project; or `feature_disabled`: no control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`: 5 exports per hour per project; `retry_after_s` says when.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn create_export(
    State(state): State<ApiState>,
    project: Scoped<CreateExports>,
    headers: HeaderMap,
    request: Result<Json<ExportRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let key = idempotency::key(&headers).map_err(|()| {
        let message = "`Idempotency-Key` must be 1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`";
        ApiError::validation(
            rid,
            vec![field_error("", "invalid_idempotency_key", message)],
        )
    })?;
    let Json(request) = request.map_err(|rejection| {
        ApiError::validation(
            rid,
            vec![field_error("", "invalid_body", &rejection.body_text())],
        )
    })?;
    let (format, selector, options) = checked(rid, &request)?;
    let exports = service(&state, rid)?;
    let created = exports
        .create(
            project.scope,
            CreateExport {
                format,
                selector,
                options,
                created_by: created_by(&project.principal),
                idempotency_key: key,
            },
        )
        .await
        .map_err(|e| create_problem(rid, &e))?;
    let job = match created {
        Created::New(job) | Created::Replay(job) => job,
    };
    Ok((StatusCode::ACCEPTED, Json(export_dto(&job, exports.now()))).into_response())
}

/// Get an export
///
/// One export of the project: where it is in its life, how far it has come, and, once it is ready, its checksum,
/// expiry and download path.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/exports/{export_id}",
    operation_id = "get_export",
    tag = "exports",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("export_id" = String, Path, description = "The export id (a UUID)."),
    ),
    responses(
        (status = 200, description = "The export.", body = Export),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no export of the project has this id; or `feature_disabled`: no control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_export(
    State(state): State<ApiState>,
    project: Scoped<Read>,
) -> Result<Json<Export>, ApiError> {
    let rid = project.request_id;
    let id = path_export(&project)?;
    let exports = service(&state, rid)?;
    let job = exports
        .get(project.scope, id)
        .await
        .map_err(|e| tx_problem(rid, &e))?
        .ok_or_else(|| no_export(rid))?;
    Ok(Json(export_dto(&job, exports.now())))
}

/// Download an export
///
/// The file of a ready export, streamed from the node that wrote it, with its SHA-256 as the `ETag`.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/exports/{export_id}/download",
    operation_id = "download_export",
    tag = "exports",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("export_id" = String, Path, description = "The export id (a UUID)."),
    ),
    responses(
        (status = 200, description = "The file: JSON lines, gzip-compressed when the export asked for it.", content_type = "application/x-ndjson"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no export of the project has this id, or its file is not on this node (a deployment of several nodes keeps each file on the node that wrote it); or `feature_disabled`: no control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the export is not ready yet, or failed.", body = Problem, content_type = "application/problem+json"),
        (status = 410, description = "`export_expired`: the file expired, or a deletion request the export predates expired it.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn download_export(
    State(state): State<ApiState>,
    project: Scoped<Read>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let id = path_export(&project)?;
    let exports = service(&state, rid)?;
    let download = exports
        .open(project.scope, id)
        .await
        .map_err(|error| match error {
            DownloadError::NotFound => no_export(rid),
            DownloadError::NotReady(state) => ApiError::new(
                rid,
                ProblemCode::Conflict,
                format!("the export is {state}; download it once it is ready"),
            ),
            DownloadError::Expired => ApiError::new(
                rid,
                ProblemCode::ExportExpired,
                "the export file expired, or a deletion request removed it; create a new export",
            ),
            DownloadError::Missing => ApiError::new(
                rid,
                ProblemCode::NotFound,
                "the export file is not on this node; download it from the node that wrote it",
            ),
            DownloadError::Store(error) => tx_problem(rid, &error),
            DownloadError::Disk(error) => {
                tracing::error!(request_id = %rid, %error, "opening an export file failed");
                ApiError::new(rid, ProblemCode::InternalError, "internal error")
            }
        })?;
    let (len, name, checksum, gzip) = (
        download.len,
        download.file_name.clone(),
        download.checksum.clone(),
        download.gzip,
    );
    let chunks = futures::stream::unfold((download, 0u64), move |(download, offset)| async move {
        if offset >= len {
            return None;
        }
        let want = usize::try_from((len - offset).min(CHUNK_BYTES as u64)).unwrap_or(CHUNK_BYTES);
        match download.read(offset, want).await {
            Ok(bytes) if bytes.is_empty() => None,
            Ok(bytes) => {
                let next = offset + bytes.len() as u64;
                Some((Ok::<_, std::io::Error>(bytes), (download, next)))
            }
            Err(error) => Some((
                Err(std::io::Error::other(error.to_string())),
                (download, len),
            )),
        }
    });
    let mut response = (StatusCode::OK, Body::from_stream(chunks)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(if gzip {
            "application/gzip"
        } else {
            "application/x-ndjson"
        }),
    );
    headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("\"{checksum}\"")) {
        headers.insert(ETAG, value);
    }
    Ok(response)
}
