//! `GET projects/{project_id}/traces`: the trace list and its live tail (contract 8.7, 8.8 and 12.6).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, Saturation, TraceListPage, TraceListParams, usd_from_nanos};
use crucible_plan::{Filter, FilterTarget, TRACE_LIST_MAX_LIMIT};
use crucible_query as query;
use crucible_query::{TraceListQuery, TraceTailQuery};
use utoipa::OpenApi;

use super::cursor_invalid;
use crate::access::{ApiState, Project};
use crate::convert::{meta, micros_between, trace_facts};
use crate::cursor::Cursors;
use crate::filters::list_filter;
use crate::params::{Params, check_window};
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/projects/{project_id}/traces", get(list_traces))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(list_traces))]
pub(super) struct Paths;

/// List traces
///
/// The traces whose head (the root span, or the earliest span while the root has not arrived) started in the
/// window, newest first. With `since`, the traces whose head arrived after that head cursor instead. The
/// filter parameters test the head span's values, except `observation_type` and `metadata`, which keep the
/// traces with a matching span in the window; a cursor works only with the filters it was issued for.
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
    let filter = list_filter(&mut params);
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
    if !span_conditions_fit_one_table(&filter) {
        let error = field_error(
            "metadata",
            "conflict",
            "`metadata` cannot be combined with `observation_type` on the trace list;              the span list (`GET .../spans`) filters by both",
        );
        return Err(ApiError::validation(rid, vec![error]));
    }
    check_window(rid, window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let since = since.map(|text| cursors.open_head(&text, window, &filter));
    let since = since.transpose().map_err(cursor_invalid(rid))?;
    let cursor = page.map(|text| cursors.open_page(&text, window, &filter));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    if let Some(since) = since {
        let query = TraceTailQuery {
            window,
            filter: filter.clone(),
            limit,
            since,
        };
        let tail = service.trace_list_since(&project.ctx, query).await;
        let tail = tail.map_err(|e| ApiError::query(rid, &e))?;
        return Ok(Json(trace_tail(&tail, &cursors, &filter)));
    }
    let query = TraceListQuery {
        window,
        filter: filter.clone(),
        limit,
        cursor,
    };
    let page = service.trace_list(&project.ctx, query).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(trace_list(&page, &cursors, &filter)))
}

/// The conditions that heads do not carry are evaluated on one span table, `span_core` or `span_payload`
/// (`crucible_plan::filtered_trace_list`), which holds when they do not test both `observation_type` and
/// `metadata`.
fn span_conditions_fit_one_table(filter: &Filter) -> bool {
    let (_, on_spans) = filter.split(FilterTarget::TraceHeads);
    on_spans.unsupported(FilterTarget::SpanCore).is_none()
        || on_spans.unsupported(FilterTarget::SpanPayload).is_none()
}

fn list_item(item: &query::TraceListItem) -> wire::TraceListItem {
    let (head, summary) = (&item.head, &item.summary);
    let mut s = Saturation::default();
    wire::TraceListItem {
        trace_id: head.trace_id.to_string(),
        name: head.name.to_string(),
        trace_name: summary.trace_name.to_string(),
        head_kind: head.head_kind.into(),
        start_time: head.start_time.to_string(),
        duration_us: s.u64(micros_between(summary.min_start, summary.max_end)),
        status_code: head.status_code.into(),
        level: head.level.into(),
        span_count: s.u64(summary.span_count),
        error_count: s.u64(summary.error_count),
        total_cost_usd: usd_from_nanos(summary.total_cost_nusd.nanos()),
        input_tokens: s.u64(summary.input_tokens),
        output_tokens: s.u64(summary.output_tokens),
        total_tokens: s.u64(summary.total_tokens),
        user_id: summary.user_id.to_string(),
        session_id: summary.session_id.to_string(),
        environment: head.environment.to_string(),
        release: head.release.to_string(),
        tags: item.facts.tags.iter().map(ToString::to_string).collect(),
        facts: trace_facts(&item.facts),
        saturated: s.saturated(),
    }
}

fn trace_list(
    page: &query::TraceListPage,
    cursors: &Cursors<'_>,
    filter: &Filter,
) -> wire::TraceListPage {
    wire::TraceListPage {
        data: page.data.iter().map(list_item).collect(),
        next_cursor: page
            .next_cursor
            .as_ref()
            .map(|c| cursors.seal_page(c, filter)),
        has_more: page.has_more,
        head_cursor: page
            .head_cursor
            .as_ref()
            .map(|c| cursors.seal_head(c, filter)),
        new_count: None,
        meta: meta(&page.meta),
    }
}

fn trace_tail(
    tail: &query::TraceTail,
    cursors: &Cursors<'_>,
    filter: &Filter,
) -> wire::TraceListPage {
    wire::TraceListPage {
        data: tail.data.iter().map(list_item).collect(),
        next_cursor: None,
        has_more: tail.has_more,
        head_cursor: Some(cursors.seal_head(&tail.head_cursor, filter)),
        new_count: Some(tail.new_count),
        meta: meta(&tail.meta),
    }
}
