//! Metrics (contract 8.8 and 12.6): `POST projects/{project_id}/metrics/query`, the series of a
//! `MetricsQuery`, and `GET .../metrics/schema`, what the registry lets a query ask for and its caps.

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{
    FieldError, MetricsFilter, MetricsQueryRequest, MetricsResponse, MetricsSchema, Problem,
    ProblemCode, Saturation, usd_from_nanos,
};
use crucible_plan::{
    Aggregation, DimensionId, Fill, Filter, FilterField, Granularity, MeasureId, MetricsError,
    MetricsQuery, Source, SourceSchema, Step, metrics_schema, metrics_shape,
};
use crucible_query::{MetricValue, MetricsRequest};
use crucible_records::{Level, ObservationType, TimeWindow, Timestamp};
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::body::json;
use crate::convert::meta;
use crate::params::{Params, check_window};
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/projects/{project_id}/metrics/query",
            post(query_metrics).fallback(super::post_only),
        )
        .route(
            "/projects/{project_id}/metrics/schema",
            get(get_metrics_schema),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(get_metrics_schema, query_metrics))]
pub(super) struct Paths;

pub(crate) const fn step_name(step: Step) -> wire::Granularity {
    match step {
        Step::Minute => wire::Granularity::Minute,
        Step::FiveMinutes => wire::Granularity::FiveMinutes,
        Step::FifteenMinutes => wire::Granularity::FifteenMinutes,
        Step::Hour => wire::Granularity::Hour,
        Step::Day => wire::Granularity::Day,
        Step::Week => wire::Granularity::Week,
    }
}

fn granularity_text(step: Step) -> String {
    serde_json::to_value(step_name(step))
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The unit of a measure's values on the wire: costs travel in USD (contract 1.3).
fn unit(measure: MeasureId) -> &'static str {
    match measure.unit() {
        "nusd" => "usd",
        other => other,
    }
}

fn time(name: &str, text: &str, errors: &mut Vec<FieldError>) -> Option<Timestamp> {
    let time = Timestamp::parse_rfc3339(text)
        .ok()
        .filter(|t| *t >= Timestamp::EPOCH && *t <= Timestamp::MAX_EXCLUSIVE);
    if time.is_none() {
        let message = "expected an RFC 3339 date-time from 1970 to 2100";
        errors.push(field_error(name, "invalid_timestamp", message));
    }
    time
}

/// A wire filter as the plan's filter, checked against the source's filter fields.
fn filter(
    node: &MetricsFilter,
    schema: &SourceSchema,
    errors: &mut Vec<FieldError>,
) -> Option<Filter> {
    let all = |nodes: &[MetricsFilter], errors: &mut Vec<FieldError>| {
        nodes
            .iter()
            .map(|node| filter(node, schema, errors))
            .collect::<Option<Vec<_>>>()
    };
    match node {
        MetricsFilter::And(nodes) => Some(Filter::And(all(nodes, errors)?)),
        MetricsFilter::Or(nodes) => Some(Filter::Or(all(nodes, errors)?)),
        MetricsFilter::Not(node) => Some(Filter::Not(Box::new(filter(node, schema, errors)?))),
        MetricsFilter::In { field, values } => {
            let found = FilterField::ALL
                .into_iter()
                .find(|f| f.as_str() == field && schema.filter_fields.contains(f));
            let Some(found) = found else {
                let message = format!(
                    "`{field}` is not a filter field of the source; see the metrics schema"
                );
                errors.push(field_error("filter", "unknown_field", &message));
                return None;
            };
            let named = |message: &str, errors: &mut Vec<FieldError>| {
                errors.push(field_error("filter", "invalid_value", message));
            };
            match found {
                FilterField::Text(text) => Some(Filter::Text(text, values.clone())),
                FilterField::Level => {
                    let levels: Option<Vec<Level>> = values
                        .iter()
                        .map(|v| Level::ALL.iter().copied().find(|l| l.as_str() == v))
                        .collect();
                    if levels.is_none() {
                        named("`level` takes debug, default, warning or error", errors);
                    }
                    Some(Filter::Level(levels?))
                }
                FilterField::ObservationType => {
                    let types: Option<Vec<ObservationType>> = values
                        .iter()
                        .map(|v| {
                            ObservationType::ALL
                                .iter()
                                .copied()
                                .find(|t| t.as_str() == v)
                        })
                        .collect();
                    if types.is_none() {
                        named(
                            "`observation_type` takes the observation type names",
                            errors,
                        );
                    }
                    Some(Filter::ObservationType(types?))
                }
                FilterField::Tags | FilterField::Metadata | FilterField::Name => {
                    named("the field takes another operator", errors);
                    None
                }
            }
        }
    }
}

fn parse(
    project: &Project,
    request: &MetricsQueryRequest,
) -> Result<(TimeWindow, MetricsQuery), ApiError> {
    parse_request(
        project.request_id,
        project.settings.max_window_days,
        request,
    )
}

/// Checks the request against the registry, its caps and the project's longest window, and builds the query
/// the compiler takes. Dashboards check their widgets' queries with it too.
pub(crate) fn parse_request(
    rid: crucible_records::RequestId,
    max_window_days: u32,
    request: &MetricsQueryRequest,
) -> Result<(TimeWindow, MetricsQuery), ApiError> {
    let mut errors = Vec::new();
    let (from, to) = (
        time("from", &request.from, &mut errors),
        time("to", &request.to, &mut errors),
    );
    let window = match (from, to) {
        (Some(from), Some(to)) => {
            let window = TimeWindow::new(from, to).ok();
            if window.is_none() {
                errors.push(field_error(
                    "to",
                    "invalid_window",
                    "`to` must be after `from`",
                ));
            }
            window
        }
        _ => None,
    };
    let source = Source::parse(&request.source);
    let schema = source.and_then(SourceSchema::of);
    if schema.is_none() {
        errors.push(field_error(
            "source",
            "invalid",
            "the sources with metrics are spans and traces",
        ));
    }
    let mut measures = Vec::new();
    for (i, m) in request.measures.iter().enumerate() {
        match (
            MeasureId::parse(&m.measure),
            Aggregation::parse(&m.aggregation),
        ) {
            (Some(measure), Some(aggregation)) => measures.push((measure, aggregation)),
            (None, _) => errors.push(field_error(
                &format!("measures/{i}/measure"),
                "invalid",
                "see the metrics schema for the measures",
            )),
            (_, None) => errors.push(field_error(
                &format!("measures/{i}/aggregation"),
                "invalid",
                "expected sum, p50, p90, p95 or p99",
            )),
        }
    }
    let mut dimensions = Vec::new();
    for (i, d) in request.dimensions.iter().flatten().enumerate() {
        match DimensionId::parse(d) {
            Some(d) => dimensions.push(d),
            None => errors.push(field_error(
                &format!("dimensions/{i}"),
                "invalid",
                "see the metrics schema for the dimensions",
            )),
        }
    }
    let filter = match (&request.filter, &schema) {
        (Some(node), Some(schema)) => filter(node, schema, &mut errors),
        _ => Some(Filter::all()),
    };
    if request
        .timezone
        .as_deref()
        .is_some_and(|tz| !matches!(tz, "UTC" | "Etc/UTC"))
    {
        errors.push(field_error(
            "timezone",
            "unsupported",
            "only UTC buckets are supported yet",
        ));
    }
    let granularity = match request.granularity.unwrap_or(wire::Granularity::Auto) {
        wire::Granularity::Auto => Granularity::Auto,
        wire::Granularity::Minute => Granularity::Step(Step::Minute),
        wire::Granularity::FiveMinutes => Granularity::Step(Step::FiveMinutes),
        wire::Granularity::FifteenMinutes => Granularity::Step(Step::FifteenMinutes),
        wire::Granularity::Hour => Granularity::Step(Step::Hour),
        wire::Granularity::Day => Granularity::Step(Step::Day),
        wire::Granularity::Week => Granularity::Step(Step::Week),
    };
    let (Some(window), Some(source), Some(filter), true) =
        (window, source, filter, errors.is_empty())
    else {
        return Err(ApiError::validation(rid, errors));
    };
    check_window(rid, window, max_window_days)?;
    let query = MetricsQuery {
        source,
        measures,
        dimensions,
        filter,
        granularity,
        fill: match request.fill {
            Some(wire::FillMode::None) => Fill::None,
            _ => Fill::Zero,
        },
        top_n: request.top_n,
    };
    metrics_shape(window, &query).map_err(|error| match error {
        MetricsError::Invalid { field, reason } => {
            ApiError::validation(rid, vec![field_error(field, "invalid", reason)])
        }
        MetricsError::TooBroad {
            limit,
            value,
            suggested_granularity,
        } => {
            let detail = match suggested_granularity {
                Some(_) => "the query has more buckets or points than allowed; use `suggested_granularity`",
                None => "the query has more buckets or points than allowed at any granularity; narrow the window",
            };
            ApiError::new(rid, ProblemCode::QueryTooBroad, detail)
                .with_limit(limit, u32::try_from(value).unwrap_or(u32::MAX))
                .with_suggested_granularity(suggested_granularity.map(granularity_text))
        }
        MetricsError::Plan(error) => ApiError::new(rid, ProblemCode::InternalError, error.to_string()),
    })?;
    Ok((window, query))
}

/// Query metrics
///
/// The series of up to four measures of a source, bucketed in time and split by up to two dimensions, with
/// at most 500 buckets per series, 20 keys and 10,000 points. Keys beyond `top_n` form an `other` series for
/// additive measures; for percentiles they are left out and `truncated` is set.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/metrics/query",
    operation_id = "query_metrics",
    tag = "metrics",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    request_body(content = MetricsQueryRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "The series.", body = MetricsResponse),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`payload_too_large`.", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "`unsupported_media_type`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` (with `limit` and `suggested_granularity`) or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn query_metrics(
    State(state): State<ApiState>,
    project: Project,
    headers: HeaderMap,
    raw: Result<Bytes, BytesRejection>,
) -> Result<Json<MetricsResponse>, ApiError> {
    let rid = project.request_id;
    let raw = raw.map_err(|_| {
        let detail = format!("the request body is above {} bytes", crate::BODY_LIMIT);
        ApiError::new(rid, ProblemCode::PayloadTooLarge, detail)
    })?;
    let request: MetricsQueryRequest = json(rid, &headers, &raw)?;
    let (window, query) = parse(&project, &request)?;
    let service = state.service(rid)?;
    let request = MetricsRequest { window, query };
    let result = service.metrics(&project.ctx, request.clone()).await;
    let result = result.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let series = result
        .series
        .iter()
        .map(|series| wire::MetricsSeries {
            measure: series.measure.as_str().to_owned(),
            aggregation: series.aggregation.as_str().to_owned(),
            unit: unit(series.measure).to_owned(),
            key: request
                .query
                .dimensions
                .iter()
                .zip(&series.key)
                .map(|(dimension, value)| wire::SeriesKey {
                    dimension: dimension.as_str().to_owned(),
                    value: value.to_string(),
                })
                .collect(),
            other: series.other,
            points: series
                .points
                .iter()
                .map(|point| wire::MetricsPoint {
                    bucket: point.bucket.to_string(),
                    value: point.value.map(|value| match value {
                        MetricValue::UInt(v) => s.u64(v).get() as f64,
                        MetricValue::Int(nanos) => usd_from_nanos(nanos),
                    }),
                })
                .collect(),
        })
        .collect();
    Ok(Json(MetricsResponse {
        series,
        granularity: step_name(result.step),
        truncated: result.truncated,
        source_tier: result.tier.as_str().to_owned(),
        as_of: result.meta.as_of.to_string(),
        partial_last_bucket: result.partial_last_bucket,
        saturated: s.saturated(),
        meta: meta(&result.meta),
    }))
}

/// Get the metrics schema
///
/// Per source: the measures with their units and aggregations, the dimensions, the filter fields with
/// their operators, the granularities and the caps of a query. Generated from the registry the query
/// compiler uses.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/metrics/schema",
    operation_id = "get_metrics_schema",
    tag = "metrics",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The schema.", body = MetricsSchema),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_metrics_schema(
    project: Project,
    uri: Uri,
) -> Result<Json<MetricsSchema>, ApiError> {
    Params::new(project.request_id, &uri)?.finish()?;
    let sources = metrics_schema()
        .into_iter()
        .map(|schema| wire::SourceSchema {
            source: schema.source.as_str().to_owned(),
            measures: schema
                .measures
                .iter()
                .map(|m| wire::SchemaMeasure {
                    id: m.as_str().to_owned(),
                    label: m.label().to_owned(),
                    unit: unit(*m).to_owned(),
                    aggregations: m
                        .aggregations()
                        .iter()
                        .map(|a| a.as_str().to_owned())
                        .collect(),
                    additive: m.additive(),
                    tier: m.tier().as_str().to_owned(),
                })
                .collect(),
            dimensions: schema
                .dimensions
                .iter()
                .map(|d| wire::SchemaDimension {
                    id: d.as_str().to_owned(),
                    dimension_type: d.ty().as_str().to_owned(),
                    high_cardinality: d.high_cardinality(),
                })
                .collect(),
            filter_fields: schema
                .filter_fields
                .iter()
                .map(|f| wire::SchemaFilterField {
                    field: f.as_str().to_owned(),
                    operator: f.operator().to_owned(),
                })
                .collect(),
            granularities: schema.granularities.iter().map(|s| step_name(*s)).collect(),
            limits: wire::SchemaLimits {
                max_buckets: u32::try_from(schema.limits.max_buckets).unwrap_or(u32::MAX),
                max_series: schema.limits.max_series,
                max_points: u32::try_from(schema.limits.max_points).unwrap_or(u32::MAX),
                max_measures: u32::try_from(schema.limits.max_measures).unwrap_or(u32::MAX),
                max_dimensions: u32::try_from(schema.limits.max_dimensions).unwrap_or(u32::MAX),
            },
        })
        .collect();
    Ok(Json(MetricsSchema { sources }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granularity_names_are_the_wire_enum() {
        let names: Vec<String> = Step::ALL.into_iter().map(granularity_text).collect();
        assert_eq!(
            names,
            [
                "minute",
                "five_minutes",
                "fifteen_minutes",
                "hour",
                "day",
                "week"
            ]
        );
    }
}
