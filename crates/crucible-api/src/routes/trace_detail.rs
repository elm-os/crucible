//! `GET projects/{project_id}/traces/{trace_id}`: one trace (contract 8.7, 8.8 and 12.6).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, Saturation, TraceDetail, TraceDetailParams};
use crucible_query as query;
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::convert::{meta, span};
use crate::params::Params;
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/projects/{project_id}/traces/{trace_id}", get(get_trace))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(get_trace))]
pub(super) struct Paths;

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
    Ok(Json(trace_detail(&detail)))
}

fn trace_detail(detail: &query::TraceDetail) -> wire::TraceDetail {
    let mut s = Saturation::default();
    wire::TraceDetail {
        trace_id: detail.trace_id.to_string(),
        span_count: s.u64(detail.span_count),
        paged: detail.paged,
        spans: detail.spans.iter().map(span).collect(),
        meta: meta(&detail.meta),
        saturated: s.saturated(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible_api_types::{MAX_SAFE_INTEGER, usd_from_nanos};
    use crucible_query::{SpanSource, TraceSpan};
    use crucible_recent::{SpanSummary, WalLocator};
    use crucible_records::{
        Level, NanoUsd, ObservationType, SpanId, StatusCode, Timestamp, TraceId,
    };

    use super::*;

    fn span(tokens: u64, start: i64, end: i64) -> TraceSpan {
        let text = || Arc::from("");
        TraceSpan {
            summary: SpanSummary {
                trace_id: TraceId::from_bytes([1; 16]),
                span_id: SpanId::from_bytes([2; 8]),
                parent_span_id: SpanId::ZERO,
                is_root: true,
                start_time: Timestamp::from_micros(start),
                end_time: Timestamp::from_micros(end),
                observation_type: ObservationType::Span,
                level: Level::Default,
                status_code: StatusCode::Unset,
                name: text(),
                trace_name: text(),
                user_id: text(),
                session_id: text(),
                model: text(),
                environment: text(),
                total_cost_nusd: NanoUsd::MAX,
                input_tokens: tokens,
                output_tokens: 0,
                total_tokens: tokens,
                usage_aggregate: false,
            },
            source: SpanSource::Recent(WalLocator {
                segment: 1,
                offset: 0,
                ordinal: 0,
            }),
            child_count: Some(tokens),
        }
    }

    #[test]
    fn counts_above_two_to_the_53_saturate_and_their_object_says_so() {
        let detail = query::TraceDetail {
            trace_id: TraceId::from_bytes([1; 16]),
            span_count: u64::MAX,
            paged: true,
            spans: vec![span(u64::MAX, 10, 20), span(3, 20, 10)],
            meta: query::Meta {
                as_of: Timestamp::EPOCH,
                partial: false,
                partial_reason: None,
                consistency: query::Consistency::Strong,
            },
        };
        let wire = trace_detail(&detail);
        assert!(wire.saturated);
        assert_eq!(wire.span_count.get(), MAX_SAFE_INTEGER);
        let (big, small) = (&wire.spans[0], &wire.spans[1]);
        assert!(big.saturated);
        assert_eq!(big.input_tokens.get(), MAX_SAFE_INTEGER);
        assert_eq!(big.child_count.map(|c| c.get()), Some(MAX_SAFE_INTEGER));
        assert_eq!(big.output_tokens.get(), 0);
        assert!(!small.saturated);
        // An end before the start is a zero duration, not a wrapped one.
        assert_eq!(small.duration_us.get(), 0);
        assert_eq!(big.total_cost_usd, usd_from_nanos(i64::MAX));
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["span_count"], MAX_SAFE_INTEGER);
        assert_eq!(json["saturated"], true);
        assert_eq!(json["spans"][0]["parent_span_id"], serde_json::Value::Null);
    }
}
