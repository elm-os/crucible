//! `GET projects/{project_id}/users`, `GET projects/{project_id}/users/{user_id}` and
//! `GET projects/{project_id}/users/{user_id}/sessions` (contract 8.7, 3.10 and 12.6). A user's traces are the
//! trace list with `user_id`.

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{
    EntityParams, Problem, Saturation, UserDetail, UserListParams, UserPage, UserSessionPage,
    usd_from_nanos,
};
use crucible_cursor::IdList;
use crucible_plan::{USER_LIST_MAX_LIMIT, USER_SESSIONS_MAX_LIMIT};
use crucible_query as query;
use crucible_query::{UserListQuery, UserSessionsQuery};
use utoipa::OpenApi;

use super::cursor_invalid;
use crate::access::{ApiState, Project};
use crate::convert::meta;
use crate::params::{Params, check_window};
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/projects/{project_id}/users", get(list_users))
        .route("/projects/{project_id}/users/{user_id}", get(get_user))
        .route(
            "/projects/{project_id}/users/{user_id}/sessions",
            get(list_user_sessions),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(list_users, get_user, list_user_sessions))]
pub(super) struct Paths;

/// List users
///
/// The users whose spans fall on the window's UTC days, last seen first, each with its spans aggregated over
/// those days and its distinct session count in the window. Pages are computed when they are read, so a user
/// seen again between two pages can move up.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/users",
    operation_id = "list_users",
    tag = "users",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), UserListParams),
    responses(
        (status = 200, description = "One page of users.", body = UserPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_users(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<UserPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let window = params.window();
    let limit = params.limit(USER_LIST_MAX_LIMIT);
    let page = params.optional("cursor");
    params.finish()?;
    let Some(window) = window else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let cursor = page.map(|text| cursors.open_id_page(&text, IdList::Users, None, window));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    let query = UserListQuery {
        window,
        limit,
        cursor,
    };
    let page = service.user_list(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(UserPage {
        data: page.data.iter().map(user_item).collect(),
        next_cursor: page
            .next_cursor
            .as_ref()
            .and_then(|c| cursors.seal_id_page(IdList::Users, None, c)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }))
}

/// Get a user
///
/// One user's spans on the window's UTC days, aggregated, with the distinct session count in the window. The
/// user's sessions are `GET .../users/{user_id}/sessions`; its traces are the trace list with `user_id`.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/users/{user_id}",
    operation_id = "get_user",
    tag = "users",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("user_id" = String, Path, description = "The user id, 1 to 512 bytes, percent-encoded."),
        EntityParams,
    ),
    responses(
        (status = 200, description = "The user, or null with `meta` saying why the answer may be incomplete.", body = UserDetail),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no span of the user in the window, or the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_user(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<UserDetail>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let user_id = params.text_id(&project.path, "user_id");
    let window = params.window();
    params.finish()?;
    let (Some(user_id), Some(window)) = (user_id, window) else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let service = state.service(rid)?;
    let detail = service.user(&project.ctx, window, &user_id).await;
    let detail = detail.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(UserDetail {
        user: detail.user.as_ref().map(user_item),
        meta: meta(&detail.meta),
    }))
}

/// List the sessions of a user
///
/// The sessions that the user's traces in the window name, latest trace first, from the head span of each
/// trace. Session totals are `GET .../sessions/{session_id}`.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/users/{user_id}/sessions",
    operation_id = "list_user_sessions",
    tag = "users",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("user_id" = String, Path, description = "The user id, 1 to 512 bytes, percent-encoded."),
        UserListParams,
    ),
    responses(
        (status = 200, description = "One page of the user's sessions.", body = UserSessionPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_user_sessions(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<UserSessionPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let user_id = params.text_id(&project.path, "user_id");
    let window = params.window();
    let limit = params.limit(USER_SESSIONS_MAX_LIMIT);
    let page = params.optional("cursor");
    params.finish()?;
    let (Some(user_id), Some(window)) = (user_id, window) else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let list = IdList::UserSessions;
    let cursor = page.map(|text| cursors.open_id_page(&text, list, Some(&user_id), window));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    let query = UserSessionsQuery {
        window,
        user_id: user_id.clone(),
        limit,
        cursor,
    };
    let page = service.user_sessions(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(UserSessionPage {
        data: page
            .data
            .iter()
            .map(|session| {
                let mut s = Saturation::default();
                wire::UserSessionItem {
                    session_id: session.session_id.to_string(),
                    first_trace_at: session.first_trace_at.to_string(),
                    last_trace_at: session.last_trace_at.to_string(),
                    trace_count: s.u64(session.trace_count),
                    saturated: s.saturated(),
                }
            })
            .collect(),
        next_cursor: page
            .next_cursor
            .as_ref()
            .and_then(|c| cursors.seal_id_page(list, Some(&user_id), c)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }))
}

fn user_item(user: &query::UserSummary) -> wire::UserItem {
    let mut s = Saturation::default();
    wire::UserItem {
        user_id: user.user_id.to_string(),
        first_seen: user.first_seen.to_string(),
        last_seen: user.last_seen.to_string(),
        trace_count: s.u64(user.trace_count),
        session_count: s.u64(user.session_count),
        span_count: s.u64(user.span_count),
        error_count: s.u64(user.error_count),
        total_cost_usd: usd_from_nanos(user.total_cost_nusd.nanos()),
        input_tokens: s.u64(user.input_tokens),
        output_tokens: s.u64(user.output_tokens),
        total_tokens: s.u64(user.total_tokens),
        saturated: s.saturated(),
    }
}
