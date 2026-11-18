//! `GET projects/{project_id}/traces/{trace_id}/spans`: the children of a span (contract 8.8 and 12.6).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, SpanPage, SpanPageParams};
use crucible_plan::SPAN_CHILDREN_MAX_LIMIT;
use crucible_query as query;
use crucible_query::SpanChildrenQuery;
use utoipa::OpenApi;

use super::cursor_invalid;
use crate::access::{ApiState, Project};
use crate::convert::{meta, span};
use crate::cursor::Cursors;
use crate::params::Params;
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route(
        "/projects/{project_id}/traces/{trace_id}/spans",
        get(list_span_children),
    )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(list_span_children))]
pub(super) struct Paths;

/// List the children of a span
///
/// One page of the direct children of `parent_span_id`, in start order, each with `child_count`: how large
/// traces are read below their first two levels.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/traces/{trace_id}/spans",
    operation_id = "list_span_children",
    tag = "traces",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("trace_id" = String, Path, description = "The trace id (32 hex characters)."),
        SpanPageParams,
    ),
    responses(
        (status = 200, description = "One page of children.", body = SpanPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no such trace, or the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_span_children(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SpanPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let trace_id = params.trace_id(&project.path);
    let parent = params.span_id("parent_span_id");
    let limit = params.limit(SPAN_CHILDREN_MAX_LIMIT);
    let page = params.optional("cursor");
    params.finish()?;
    let (Some(trace_id), Some(parent)) = (trace_id, parent) else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let cursors = state.cursors(&project);
    let cursor = page.map(|text| cursors.open_child(&text, trace_id, parent));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    let query = SpanChildrenQuery {
        trace_id,
        parent,
        cursor,
        limit,
    };
    let page = service.span_children(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(span_page(&page, &cursors)))
}

fn span_page(page: &query::SpanChildrenPage, cursors: &Cursors<'_>) -> wire::SpanPage {
    wire::SpanPage {
        data: page.data.iter().map(span).collect(),
        next_cursor: page.next_cursor.as_ref().map(|c| cursors.seal_child(c)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }
}
