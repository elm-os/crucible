//! `GET projects/{project_id}/sessions` and `GET projects/{project_id}/sessions/{session_id}` (contract 8.7
//! and 12.6). A session's traces are the trace list with `session_id`.

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{
    EntityParams, Problem, Saturation, SessionDetail, SessionListParams, SessionPage,
    usd_from_nanos,
};
use crucible_cursor::IdList;
use crucible_plan::SESSION_LIST_MAX_LIMIT;
use crucible_query as query;
use crucible_query::SessionListQuery;
use utoipa::OpenApi;

use super::cursor_invalid;
use crate::access::{ApiState, Project};
use crate::convert::meta;
use crate::cursor::Cursors;
use crate::params::{Params, check_window};
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/projects/{project_id}/sessions", get(list_sessions))
        .route(
            "/projects/{project_id}/sessions/{session_id}",
            get(get_session),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(list_sessions, get_session))]
pub(super) struct Paths;

/// List sessions
///
/// The sessions whose spans fall on the window's UTC days, last seen first, each with its spans aggregated
/// over those days. Pages are computed when they are read, so a session seen again between two pages can
/// move up.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/sessions",
    operation_id = "list_sessions",
    tag = "sessions",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), SessionListParams),
    responses(
        (status = 200, description = "One page of sessions.", body = SessionPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_sessions(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SessionPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let window = params.window();
    let limit = params.limit(SESSION_LIST_MAX_LIMIT);
    let page = params.optional("cursor");
    params.finish()?;
    let Some(window) = window else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let cursor = page.map(|text| cursors.open_id_page(&text, IdList::Sessions, None, window));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    let query = SessionListQuery {
        window,
        limit,
        cursor,
    };
    let page = service.session_list(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(session_page(&page, &cursors)))
}

/// Get a session
///
/// One session's spans on the window's UTC days, aggregated. Its traces are the trace list with
/// `session_id`.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/sessions/{session_id}",
    operation_id = "get_session",
    tag = "sessions",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("session_id" = String, Path, description = "The session id, 1 to 512 bytes, percent-encoded."),
        EntityParams,
    ),
    responses(
        (status = 200, description = "The session, or null with `meta` saying why the answer may be incomplete.", body = SessionDetail),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no span of the session in the window, or the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_session(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SessionDetail>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let session_id = params.text_id(&project.path, "session_id");
    let window = params.window();
    params.finish()?;
    let (Some(session_id), Some(window)) = (session_id, window) else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let service = state.service(rid)?;
    let detail = service.session(&project.ctx, window, &session_id).await;
    let detail = detail.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(SessionDetail {
        session: detail.session.as_ref().map(session_item),
        meta: meta(&detail.meta),
    }))
}

fn session_item(session: &query::SessionSummary) -> wire::SessionItem {
    let mut s = Saturation::default();
    wire::SessionItem {
        session_id: session.session_id.to_string(),
        first_seen: session.first_seen.to_string(),
        last_seen: session.last_seen.to_string(),
        trace_count: s.u64(session.trace_count),
        span_count: s.u64(session.span_count),
        error_count: s.u64(session.error_count),
        total_cost_usd: usd_from_nanos(session.total_cost_nusd.nanos()),
        input_tokens: s.u64(session.input_tokens),
        output_tokens: s.u64(session.output_tokens),
        total_tokens: s.u64(session.total_tokens),
        user_ids: session.user_ids.iter().map(ToString::to_string).collect(),
        saturated: s.saturated(),
    }
}

fn session_page(page: &query::SessionListPage, cursors: &Cursors<'_>) -> SessionPage {
    SessionPage {
        data: page.data.iter().map(session_item).collect(),
        next_cursor: page
            .next_cursor
            .as_ref()
            .and_then(|c| cursors.seal_id_page(IdList::Sessions, None, c)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }
}
