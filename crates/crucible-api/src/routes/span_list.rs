//! `GET projects/{project_id}/spans`: the span list (contract 8.7 and 12.6).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, Saturation, SpanListPage, SpanListParams, usd_from_nanos};
use crucible_plan::{Filter, SPAN_LIST_MAX_LIMIT};
use crucible_query as query;
use crucible_query::SpanListQuery;
use crucible_records::SpanId;
use utoipa::OpenApi;

use super::cursor_invalid;
use crate::access::{ApiState, Project};
use crate::convert::{meta, micros_between};
use crate::cursor::Cursors;
use crate::filters::{FilterList, list_filter};
use crate::params::{Params, check_window};
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/projects/{project_id}/spans", get(list_spans))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(list_spans))]
pub(super) struct Paths;

/// List spans
///
/// The spans that started in the window, newest first, under the filter parameters, each of which tests the
/// span's own value. A cursor works only with the window and filters it was issued for.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/spans",
    operation_id = "list_spans",
    tag = "traces",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), SpanListParams),
    responses(
        (status = 200, description = "One page of spans.", body = SpanListPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_spans(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<SpanListPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let window = params.window();
    let limit = params.limit(SPAN_LIST_MAX_LIMIT);
    let page = params.optional("cursor");
    let filter = list_filter(&mut params, FilterList::Spans);
    params.finish()?;
    let Some(window) = window else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let cursor = page.map(|text| cursors.open_span_page(&text, window, &filter));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    let query = SpanListQuery {
        window,
        filter: filter.clone(),
        limit,
        cursor,
    };
    let page = service.span_list(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(span_page(&page, &cursors, &filter)))
}

fn span_page(
    page: &query::SpanListPage,
    cursors: &Cursors<'_>,
    filter: &Filter,
) -> wire::SpanListPage {
    wire::SpanListPage {
        data: page
            .data
            .iter()
            .map(|span| {
                let mut s = Saturation::default();
                wire::SpanListItem {
                    trace_id: span.trace_id.to_string(),
                    span_id: span.span_id.to_string(),
                    parent_span_id: (span.parent_span_id != SpanId::ZERO)
                        .then(|| span.parent_span_id.to_string()),
                    name: span.name.to_string(),
                    observation_type: span.observation_type.into(),
                    start_time: span.start_time.to_string(),
                    duration_us: s.u64(micros_between(span.start_time, span.end_time)),
                    status_code: span.status_code.into(),
                    level: span.level.into(),
                    model: span.model.to_string(),
                    environment: span.environment.to_string(),
                    user_id: span.user_id.to_string(),
                    session_id: span.session_id.to_string(),
                    total_cost_usd: usd_from_nanos(span.total_cost_nusd.nanos()),
                    total_tokens: s.u64(span.total_tokens),
                    saturated: s.saturated(),
                }
            })
            .collect(),
        next_cursor: page
            .next_cursor
            .as_ref()
            .map(|c| cursors.seal_span_page(c, filter)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }
}
