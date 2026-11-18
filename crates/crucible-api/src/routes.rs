//! The telemetry routes of milestone 1 (contract 12.6) and their OpenAPI operations. Each handler checks its
//! parameters before it asks for the query service, so a request without a store configured is still
//! authenticated and validated before it is answered with `telemetry_store_unavailable`.

use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use crucible_api_types::{
    DataBounds, Problem, ProblemCode, SpanPage, SpanPageParams, TraceDetail, TraceDetailParams,
    TraceListPage, TraceListParams,
};
use crucible_cursor::CursorError;
use crucible_plan::{Pred, SPAN_CHILDREN_MAX_LIMIT, TRACE_LIST_MAX_LIMIT};
use crucible_query::{SpanChildrenQuery, TraceListQuery, TraceTailQuery};
use crucible_records::RequestId;

use crate::access::{ApiState, Project};
use crate::convert;
use crate::params::{Params, check_window};
use crate::problem::{ApiError, field_error};

fn cursor_invalid(request_id: RequestId) -> impl FnOnce(CursorError) -> ApiError {
    move |error| {
        ApiError::new(
            request_id,
            ProblemCode::CursorInvalid,
            format!("{error}; restart from the first page"),
        )
    }
}

/// List traces
///
/// The traces whose head (the root span, or the earliest span while the root has not arrived) started in the
/// window, newest first. With `since`, the traces whose head arrived after that head cursor instead.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/traces",
    operation_id = "list_traces",
    tag = "traces",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), TraceListParams),
    responses(
        (status = 200, description = "One page of traces.", body = TraceListPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_traces(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<TraceListPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let window = params.window();
    let limit = params.limit(TRACE_LIST_MAX_LIMIT);
    let (page, since) = (params.optional("cursor"), params.optional("since"));
    params.finish()?;
    let Some(window) = window else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    if page.is_some() && since.is_some() {
        let error = field_error(
            "since",
            "conflict",
            "`since` and `cursor` cannot be combined",
        );
        return Err(ApiError::validation(rid, vec![error]));
    }
    check_window(rid, window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let since = since.map(|text| cursors.open_head(&text, window));
    let since = since.transpose().map_err(cursor_invalid(rid))?;
    let cursor = page.map(|text| cursors.open_page(&text, window));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    if let Some(since) = since {
        let query = TraceTailQuery {
            window,
            filter: Pred::all(),
            limit,
            since,
        };
        let tail = service.trace_list_since(&project.ctx, query).await;
        let tail = tail.map_err(|e| ApiError::query(rid, &e))?;
        return Ok(Json(convert::trace_tail(&tail, &cursors)));
    }
    let query = TraceListQuery {
        window,
        filter: Pred::all(),
        limit,
        cursor,
    };
    let page = service.trace_list(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(convert::trace_list(&page, &cursors)))
}

/// Get a trace
///
/// Every span of the trace up to 5,000 spans; above that, the root spans and their direct children with
/// `paged: true`. An empty trace with `partial` or `lagging` in `meta` means the trace may exist but could not
/// be read in full.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/traces/{trace_id}",
    operation_id = "get_trace",
    tag = "traces",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("trace_id" = String, Path, description = "The trace id (32 hex characters)."),
        TraceDetailParams,
    ),
    responses(
        (status = 200, description = "The trace.", body = TraceDetail),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no such trace, or the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_trace(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<TraceDetail>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let trace_id = params.trace_id(&project.path);
    // Contract 12.6 lets a client pass `hint_time` instead of a window. The service finds the trace's time
    // bounds itself, so the hint is checked and then not needed.
    let _hint = params.optional_time("hint_time");
    params.finish()?;
    let Some(trace_id) = trace_id else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let service = state.service(rid)?;
    let detail = service.trace_by_id(&project.ctx, trace_id).await;
    let detail = detail.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(convert::trace_detail(&detail)))
}

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
    Ok(Json(convert::span_page(&page, &cursors)))
}

/// Get data bounds
///
/// Where the project's data begins and ends inside its retention, with the retention horizon, the widest
/// window a request may ask for and the server's clock.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/data-bounds",
    operation_id = "get_data_bounds",
    tag = "traces",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The data bounds.", body = DataBounds),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_data_bounds(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<DataBounds>, ApiError> {
    let rid = project.request_id;
    Params::new(rid, &uri)?.finish()?;
    let service = state.service(rid)?;
    let bounds = service.data_bounds(&project.ctx).await;
    let bounds = bounds.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(convert::data_bounds(
        &bounds,
        project.settings.max_window_days,
    )))
}
