//! `GET projects/{project_id}/filter-values`: the most frequent values of a metrics source's filter field in a
//! window (contract 8.7, 8.8 and 12.6).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{FilterValuesParams, Problem, Saturation};
use crucible_plan::{
    FILTER_VALUES_MAX_LIMIT, FilterField, FilterValuesRequest, Source, SourceSchema,
};
use crucible_query::FilterValuesQuery;
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::convert::meta;
use crate::params::{Params, check_window};
use crate::problem::{ApiError, field_error};

/// The default number of values.
const DEFAULT_LIMIT: u32 = 20;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route(
        "/projects/{project_id}/filter-values",
        get(get_filter_values),
    )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(get_filter_values))]
pub(super) struct Paths;

/// Get filter values
///
/// The values of a filter field of a metrics source in the window, most frequent first, with how many spans
/// carry each. The fields are those of the metrics schema.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/filter-values",
    operation_id = "get_filter_values",
    tag = "metrics",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), FilterValuesParams),
    responses(
        (status = 200, description = "The values.", body = wire::FilterValues),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_filter_values(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::FilterValues>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let window = params.window();
    let limit = params.limit(FILTER_VALUES_MAX_LIMIT);
    let (source, field) = (params.optional("source"), params.optional("field"));
    let prefix = params.optional("prefix");
    params.finish()?;
    let mut errors = Vec::new();
    let source = source.as_deref().and_then(Source::parse);
    let schema = source.and_then(SourceSchema::of);
    if schema.is_none() {
        errors.push(field_error(
            "source",
            "invalid",
            "the sources with filter fields are listed in the metrics schema",
        ));
    }
    let field = field.as_deref().and_then(|name| {
        FilterField::ALL.into_iter().find(|f| {
            f.as_str() == name && schema.as_ref().is_some_and(|s| s.filter_fields.contains(f))
        })
    });
    if field.is_none() && schema.is_some() {
        errors.push(field_error(
            "field",
            "unknown_field",
            "the field is not a filter field of the source; see the metrics schema",
        ));
    }
    let (Some(window), Some(source), Some(field), true) =
        (window, source, field, errors.is_empty())
    else {
        return Err(ApiError::validation(rid, errors));
    };
    check_window(rid, window, project.settings.max_window_days)?;
    let service = state.service(rid)?;
    let query = FilterValuesQuery {
        window,
        request: FilterValuesRequest {
            source,
            field,
            prefix,
            limit: limit.unwrap_or(DEFAULT_LIMIT),
        },
    };
    let values = service.filter_values(&project.ctx, query).await;
    let values = values.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    Ok(Json(wire::FilterValues {
        data: values
            .data
            .iter()
            .map(|(value, count)| wire::FilterValue {
                value: value.to_string(),
                count: s.u64(*count),
            })
            .collect(),
        truncated: values.truncated,
        saturated: s.saturated(),
        meta: meta(&values.meta),
    }))
}
