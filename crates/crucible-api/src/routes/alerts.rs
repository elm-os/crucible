//! Alert rules: `projects/{project_id}/alerts/...` (contract 12.6).
//!
//! Reading rules, their history and a preview needs `read`; creating, changing and deleting a rule needs
//! `edit_content`, which no API key holds (contract 11.3). Every refusal says which member to change.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_alerts::schedule::tick_at;
use crucible_alerts::{
    AlertError, Comparison, NO_DATA_FOR_DEFAULT_SECS, NoDataPolicy, RENOTIFY_DEFAULT_SECS,
    RuleInput, RuleSpec, RuleView, SpecIssue, series_values,
};
use crucible_api_types as wire;
use crucible_api_types::{FieldError, Problem, ProblemCode, SafeI64, Saturation, SeriesKey};
use crucible_control as control;
use crucible_plan::{Aggregation, DimensionId, MeasureId, Source};
use crucible_records::{AlertRuleId, ChannelId, RequestId, Timestamp, Uuid};
use utoipa::OpenApi;

use crate::access::{ApiState, EditContent, Read, Scoped};
use crate::control::{ApiJson, etag, if_match, version};
use crate::params::Params;
use crate::problem::{ApiError, field_error};

/// The default and the largest page of a rule's history (contract 12.4).
const HISTORY_LIMIT: (u32, u32) = (50, 200);

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    let base = "/projects/{project_id}/alerts";
    Router::new()
        .route(base, get(list_alerts).post(create_alert))
        .route(&format!("{base}/preview"), post(preview_alert))
        .route(
            &format!("{base}/{{alert_id}}"),
            get(get_alert).patch(patch_alert).delete(delete_alert),
        )
        .route(
            &format!("{base}/{{alert_id}}/history"),
            get(get_alert_history),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    list_alerts,
    create_alert,
    preview_alert,
    get_alert,
    patch_alert,
    delete_alert,
    get_alert_history,
))]
pub(super) struct Paths;

/// The wire member of each part of a rule that a [`SpecIssue`] names.
fn member(field: &str) -> &str {
    match field {
        "window" => "window_secs",
        "interval" => "interval_secs",
        "for" => "for_secs",
        "no_data_for" => "no_data_for_secs",
        "renotify" => "renotify_secs",
        other => other,
    }
}

fn issues(rid: RequestId, issues: &[SpecIssue]) -> ApiError {
    ApiError::validation(
        rid,
        issues
            .iter()
            .map(|issue| field_error(member(issue.field), "invalid", &issue.message))
            .collect(),
    )
}

/// The problem of a store failure in an alert or channel route.
pub(super) fn store_problem(rid: RequestId, error: &control::TxError) -> ApiError {
    if error.is_retryable() || matches!(error, control::TxError::CommitUnknown) {
        return ApiError::new(
            rid,
            ProblemCode::ControlPlaneUnavailable,
            "the control plane (Postgres) is unavailable; try again shortly",
        )
        .with_retry_after(5);
    }
    tracing::error!(request_id = %rid, %error, "a control-plane call failed");
    ApiError::new(rid, ProblemCode::InternalError, "internal error")
}

fn problem(rid: RequestId) -> impl FnOnce(AlertError) -> ApiError {
    move |error| match error {
        AlertError::NotFound => ApiError::not_found(rid),
        AlertError::VersionMismatch => ApiError::new(
            rid,
            ProblemCode::PreconditionFailed,
            "the rule changed since it was read; read it again and repeat the change with its new ETag",
        ),
        AlertError::Invalid(found) => issues(rid, &found),
        AlertError::NoTelemetry => ApiError::new(
            rid,
            ProblemCode::TelemetryStoreUnavailable,
            "no telemetry store is configured, so a rule cannot be previewed",
        ),
        AlertError::Read(cause) => {
            tracing::warn!(request_id = %rid, %cause, "an alert preview could not read");
            ApiError::new(
                rid,
                ProblemCode::TelemetryStoreUnavailable,
                "the preview could not read the rollup; try again shortly",
            )
            .with_retry_after(5)
        }
        AlertError::Store(error) => store_problem(rid, &error),
    }
}

const fn comparison(c: Comparison) -> wire::AlertComparison {
    match c {
        Comparison::Gt => wire::AlertComparison::Gt,
        Comparison::Gte => wire::AlertComparison::Gte,
        Comparison::Lt => wire::AlertComparison::Lt,
        Comparison::Lte => wire::AlertComparison::Lte,
    }
}

const fn comparison_in(c: wire::AlertComparison) -> Comparison {
    match c {
        wire::AlertComparison::Gt => Comparison::Gt,
        wire::AlertComparison::Gte => Comparison::Gte,
        wire::AlertComparison::Lt => Comparison::Lt,
        wire::AlertComparison::Lte => Comparison::Lte,
    }
}

const fn no_data(p: NoDataPolicy) -> wire::AlertNoDataPolicy {
    match p {
        NoDataPolicy::NoData => wire::AlertNoDataPolicy::NoData,
        NoDataPolicy::Ok => wire::AlertNoDataPolicy::Ok,
        NoDataPolicy::Keep => wire::AlertNoDataPolicy::Keep,
    }
}

const fn no_data_in(p: wire::AlertNoDataPolicy) -> NoDataPolicy {
    match p {
        wire::AlertNoDataPolicy::NoData => NoDataPolicy::NoData,
        wire::AlertNoDataPolicy::Ok => NoDataPolicy::Ok,
        wire::AlertNoDataPolicy::Keep => NoDataPolicy::Keep,
    }
}

const fn status(s: control::AlertStatus) -> wire::AlertStatus {
    match s {
        control::AlertStatus::Ok => wire::AlertStatus::Ok,
        control::AlertStatus::Pending => wire::AlertStatus::Pending,
        control::AlertStatus::Firing => wire::AlertStatus::Firing,
        control::AlertStatus::Resolved => wire::AlertStatus::Resolved,
        control::AlertStatus::NoData => wire::AlertStatus::NoData,
    }
}

/// How severe a state is, for the state of a rule: firing, then no_data, pending, resolved, ok.
const fn severity(s: control::AlertStatus) -> u8 {
    match s {
        control::AlertStatus::Firing => 4,
        control::AlertStatus::NoData => 3,
        control::AlertStatus::Pending => 2,
        control::AlertStatus::Resolved => 1,
        control::AlertStatus::Ok => 0,
    }
}

/// A series key as the wire carries it: one value per dimension of the rule.
fn key(dimensions: &[DimensionId], series_key: &str) -> Vec<SeriesKey> {
    dimensions
        .iter()
        .zip(series_values(series_key))
        .map(|(dimension, value)| SeriesKey {
            dimension: dimension.as_str().to_owned(),
            value,
        })
        .collect()
}

fn rule(view: &RuleView) -> wire::AlertRule {
    let RuleView { rule, spec, series } = view;
    let mut sat = Saturation::default();
    let series = series
        .iter()
        .map(|s| wire::AlertSeries {
            key: key(&spec.dimensions, &s.series_key),
            status: status(s.status),
            since: s.since.to_string(),
            value: s.value.map(|v| sat.i64(v)),
            last_notified_at: s.last_notified_at.map(|t| t.to_string()),
        })
        .collect();
    wire::AlertRule {
        id: rule.id.to_string(),
        name: rule.name.clone(),
        enabled: rule.enabled,
        source: spec.source.as_str().to_owned(),
        measure: spec.measure.as_str().to_owned(),
        aggregation: spec.aggregation.as_str().to_owned(),
        unit: spec.measure.unit().to_owned(),
        dimensions: spec
            .dimensions
            .iter()
            .map(|d| d.as_str().to_owned())
            .collect(),
        comparison: comparison(spec.comparison),
        threshold: sat.i64(spec.threshold),
        window_secs: spec.window_secs,
        interval_secs: spec.interval_secs,
        for_secs: spec.for_secs,
        no_data: no_data(spec.no_data),
        no_data_for_secs: spec.no_data_for_secs,
        renotify_secs: spec.renotify_secs,
        channels: spec.channels.iter().map(ToString::to_string).collect(),
        status: status(
            view.series
                .iter()
                .map(|s| s.status)
                .max_by_key(|s| severity(*s))
                .unwrap_or(control::AlertStatus::Ok),
        ),
        series,
        last_evaluated_at: rule
            .last_eval_seq
            .map(|seq| tick_at(rule.id, rule.interval_secs, seq).to_string()),
        eval_failures: rule.eval_failures,
        skipped_ticks: sat.u64(rule.skipped_ticks),
        created_at: rule.created_at.to_string(),
        updated_at: rule.updated_at.to_string(),
        saturated: sat.saturated(),
        version: version(rule.version),
    }
}

/// The parts of a definition that are names: each must be one the registry knows.
struct Names<'a> {
    source: Option<&'a str>,
    measure: &'a str,
    aggregation: &'a str,
    dimensions: &'a [String],
    channels: &'a [String],
}

struct Parsed {
    source: Source,
    measure: MeasureId,
    aggregation: Aggregation,
    dimensions: Vec<DimensionId>,
    channels: Vec<ChannelId>,
}

fn names(names: &Names<'_>, errors: &mut Vec<FieldError>) -> Option<Parsed> {
    let source = Source::parse(names.source.unwrap_or("spans"));
    if source.is_none() {
        errors.push(field_error(
            "source",
            "unknown",
            "alert rules read the span rollup; use the source `spans`",
        ));
    }
    let measure = MeasureId::parse(names.measure);
    if measure.is_none() {
        errors.push(field_error(
            "measure",
            "unknown",
            "not a measure; GET metrics/schema lists them",
        ));
    }
    let aggregation = Aggregation::parse(names.aggregation);
    if aggregation.is_none() {
        errors.push(field_error(
            "aggregation",
            "unknown",
            "not an aggregation; GET metrics/schema lists the ones each measure allows",
        ));
    }
    let mut dimensions = Vec::new();
    for (i, name) in names.dimensions.iter().enumerate() {
        match DimensionId::parse(name) {
            Some(dimension) => dimensions.push(dimension),
            None => errors.push(field_error(
                &format!("dimensions/{i}"),
                "unknown",
                "not a dimension; GET metrics/schema lists them",
            )),
        }
    }
    let mut channels = Vec::new();
    for (i, id) in names.channels.iter().enumerate() {
        match ChannelId::parse(id) {
            Ok(channel) => channels.push(channel),
            Err(_) => errors.push(field_error(
                &format!("channels/{i}"),
                "invalid",
                "not a channel id; GET channels lists them",
            )),
        }
    }
    Some(Parsed {
        source: source?,
        measure: measure?,
        aggregation: aggregation?,
        dimensions: (dimensions.len() == names.dimensions.len()).then_some(dimensions)?,
        channels: (channels.len() == names.channels.len()).then_some(channels)?,
    })
}

fn input(rid: RequestId, body: &wire::AlertRuleInput) -> Result<RuleInput, ApiError> {
    let mut errors = Vec::new();
    let parsed = names(
        &Names {
            source: body.source.as_deref(),
            measure: &body.measure,
            aggregation: &body.aggregation,
            dimensions: body.dimensions.as_deref().unwrap_or_default(),
            channels: body.channels.as_deref().unwrap_or_default(),
        },
        &mut errors,
    );
    let Some(parsed) = parsed.filter(|_| errors.is_empty()) else {
        return Err(ApiError::validation(rid, errors));
    };
    Ok(RuleInput {
        name: body.name.clone(),
        enabled: body.enabled.unwrap_or(true),
        spec: RuleSpec {
            source: parsed.source,
            measure: parsed.measure,
            aggregation: parsed.aggregation,
            dimensions: parsed.dimensions,
            comparison: comparison_in(body.comparison),
            threshold: body.threshold.get(),
            window_secs: body.window_secs,
            interval_secs: body.interval_secs.unwrap_or(60),
            for_secs: body.for_secs.unwrap_or(0),
            no_data: body.no_data.map_or(NoDataPolicy::Keep, no_data_in),
            no_data_for_secs: body.no_data_for_secs.unwrap_or(NO_DATA_FOR_DEFAULT_SECS),
            renotify_secs: body.renotify_secs.unwrap_or(RENOTIFY_DEFAULT_SECS),
            channels: parsed.channels,
        },
    })
}

/// The rule after `patch`: every member the patch leaves out keeps its value.
fn patched(
    rid: RequestId,
    view: &RuleView,
    patch: &wire::AlertRulePatch,
) -> Result<RuleInput, ApiError> {
    let spec = &view.spec;
    let strings = |ids: Vec<String>| ids;
    let dimensions = patch.dimensions.clone().unwrap_or_else(|| {
        strings(
            spec.dimensions
                .iter()
                .map(|d| d.as_str().to_owned())
                .collect(),
        )
    });
    let channels = patch
        .channels
        .clone()
        .unwrap_or_else(|| strings(spec.channels.iter().map(ToString::to_string).collect()));
    let mut errors = Vec::new();
    let parsed = names(
        &Names {
            source: Some(spec.source.as_str()),
            measure: patch.measure.as_deref().unwrap_or(spec.measure.as_str()),
            aggregation: patch
                .aggregation
                .as_deref()
                .unwrap_or(spec.aggregation.as_str()),
            dimensions: &dimensions,
            channels: &channels,
        },
        &mut errors,
    );
    let Some(parsed) = parsed.filter(|_| errors.is_empty()) else {
        return Err(ApiError::validation(rid, errors));
    };
    Ok(RuleInput {
        name: patch.name.clone().unwrap_or_else(|| view.rule.name.clone()),
        enabled: patch.enabled.unwrap_or(view.rule.enabled),
        spec: RuleSpec {
            source: parsed.source,
            measure: parsed.measure,
            aggregation: parsed.aggregation,
            dimensions: parsed.dimensions,
            comparison: patch.comparison.map_or(spec.comparison, comparison_in),
            threshold: patch.threshold.map_or(spec.threshold, SafeI64::get),
            window_secs: patch.window_secs.unwrap_or(spec.window_secs),
            interval_secs: patch.interval_secs.unwrap_or(spec.interval_secs),
            for_secs: patch.for_secs.unwrap_or(spec.for_secs),
            no_data: patch.no_data.map_or(spec.no_data, no_data_in),
            no_data_for_secs: patch.no_data_for_secs.unwrap_or(spec.no_data_for_secs),
            renotify_secs: patch.renotify_secs.unwrap_or(spec.renotify_secs),
            channels: parsed.channels,
        },
    })
}

fn alert_id<P>(scoped: &Scoped<P>) -> Result<AlertRuleId, ApiError> {
    scoped
        .path
        .get("alert_id")
        .and_then(|text| AlertRuleId::parse(text).ok())
        .ok_or_else(|| ApiError::not_found(scoped.request_id))
}

/// List alert rules
///
/// The rules of the project, each with the state of its series.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/alerts",
    operation_id = "list_alerts",
    tag = "alerts",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 200, description = "The rules.", body = wire::AlertRuleList),
        (status = 404, description = "`not_found`, or `feature_disabled` without a control plane.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []), ("project_key" = [])),
)]
pub(crate) async fn list_alerts(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
) -> Result<Json<wire::AlertRuleList>, ApiError> {
    let rid = scoped.request_id;
    let views = state
        .alerts(rid)?
        .alerts
        .list(scoped.scope.project_id())
        .await
        .map_err(problem(rid))?;
    Ok(Json(wire::AlertRuleList {
        data: views.iter().map(rule).collect(),
    }))
}

/// Create an alert rule
///
/// One measure, at most two dimensions, a comparison with a threshold, and the timing of the evaluation. The
/// first evaluation is the next tick of the rule's schedule.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/alerts",
    operation_id = "create_alert",
    tag = "alerts",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::AlertRuleInput,
    responses(
        (status = 201, description = "The rule; `ETag` is its version.", body = wire::AlertRule),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`: `errors` names each member to change.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_alert(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    ApiJson(body): ApiJson<wire::AlertRuleInput>,
) -> Result<(StatusCode, HeaderMap, Json<wire::AlertRule>), ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let view = api
        .alerts
        .create(scoped.scope.project_id(), input(rid, &body)?)
        .await
        .map_err(problem(rid))?;
    Ok((
        StatusCode::CREATED,
        etag(view.rule.version),
        Json(rule(&view)),
    ))
}

/// Preview an alert rule
///
/// What the rule in the body would see if it were evaluated now: each series of the window that ends at the
/// last whole minute at least 30 s ago, with whether it breaches the threshold. Nothing is stored and nobody is
/// notified. The `name` is not used.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/alerts/preview",
    operation_id = "preview_alert",
    tag = "alerts",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::AlertRuleInput,
    responses(
        (status = 200, description = "The preview.", body = wire::AlertPreview),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []), ("project_key" = [])),
)]
pub(crate) async fn preview_alert(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
    ApiJson(body): ApiJson<wire::AlertRuleInput>,
) -> Result<Json<wire::AlertPreview>, ApiError> {
    let rid = scoped.request_id;
    let spec = input(rid, &body)?.spec;
    let preview = state
        .alerts(rid)?
        .alerts
        .preview(scoped.scope.project_id(), &spec)
        .await
        .map_err(problem(rid))?;
    let mut sat = Saturation::default();
    let series = preview
        .series
        .iter()
        .map(|s| wire::AlertPreviewSeries {
            key: key(&spec.dimensions, &s.key),
            value: sat.i64(s.value),
            breach: s.breach,
        })
        .collect();
    Ok(Json(wire::AlertPreview {
        from: preview.window.from().to_string(),
        to: preview.window.to().to_string(),
        unit: spec.measure.unit().to_owned(),
        series,
        truncated: preview.truncated,
        saturated: sat.saturated(),
    }))
}

/// Get an alert rule
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/alerts/{alert_id}",
    operation_id = "get_alert",
    tag = "alerts",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("alert_id" = String, Path, description = "The rule id."),
    ),
    responses(
        (status = 200, description = "The rule; `ETag` is its version.", body = wire::AlertRule),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []), ("project_key" = [])),
)]
pub(crate) async fn get_alert(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
) -> Result<(HeaderMap, Json<wire::AlertRule>), ApiError> {
    let rid = scoped.request_id;
    let view = state
        .alerts(rid)?
        .alerts
        .get(scoped.scope.project_id(), alert_id(&scoped)?)
        .await
        .map_err(problem(rid))?;
    Ok((etag(view.rule.version), Json(rule(&view))))
}

/// Change an alert rule
///
/// Changes the members in the body. A change to what the rule measures or when starts its series afresh.
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/alerts/{alert_id}",
    operation_id = "patch_alert",
    tag = "alerts",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("alert_id" = String, Path, description = "The rule id."),
        ("If-Match" = String, Header, description = "The rule's `ETag`."),
    ),
    request_body = wire::AlertRulePatch,
    responses(
        (status = 200, description = "The rule.", body = wire::AlertRule),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_alert(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::AlertRulePatch>,
) -> Result<(HeaderMap, Json<wire::AlertRule>), ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let (project_id, id) = (scoped.scope.project_id(), alert_id(&scoped)?);
    let expected = if_match(&headers, rid)?;
    let current = api.alerts.get(project_id, id).await.map_err(problem(rid))?;
    let view = api
        .alerts
        .update(project_id, id, expected, patched(rid, &current, &body)?)
        .await
        .map_err(problem(rid))?;
    Ok((etag(view.rule.version), Json(rule(&view))))
}

/// Delete an alert rule
///
/// Deletes the rule with its state and its history.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/alerts/{alert_id}",
    operation_id = "delete_alert",
    tag = "alerts",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("alert_id" = String, Path, description = "The rule id."),
        ("If-Match" = String, Header, description = "The rule's `ETag`."),
    ),
    responses(
        (status = 204, description = "Deleted."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_alert(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let id = alert_id(&scoped)?;
    let expected = if_match(&headers, rid)?;
    api.alerts
        .delete(scoped.scope.project_id(), id, expected)
        .await
        .map_err(problem(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

/// A history position as a cursor: the time in microseconds and the row id.
fn cursor_text(at: Timestamp, id: Uuid) -> String {
    format!("{}_{id}", at.micros())
}

fn cursor_position(text: &str) -> Option<(Timestamp, Uuid)> {
    let (micros, id) = text.split_once('_')?;
    Some((
        Timestamp::from_micros(micros.parse().ok()?),
        id.parse().ok()?,
    ))
}

/// Get the history of an alert rule
///
/// The rule's state changes, repeated notifications and failing-evaluation notices, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/alerts/{alert_id}/history",
    operation_id = "get_alert_history",
    tag = "alerts",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("alert_id" = String, Path, description = "The rule id."),
        wire::AlertHistoryParams,
    ),
    responses(
        (status = 200, description = "A page of the history.", body = wire::AlertHistory),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []), ("project_key" = [])),
)]
pub(crate) async fn get_alert_history(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
    uri: Uri,
) -> Result<Json<wire::AlertHistory>, ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let (project_id, id) = (scoped.scope.project_id(), alert_id(&scoped)?);
    let mut params = Params::new(rid, &uri)?;
    let (default, most) = HISTORY_LIMIT;
    let limit = match params.optional("limit") {
        None => default,
        Some(text) => match text.parse::<u32>() {
            Ok(limit) if (1..=most).contains(&limit) => limit,
            _ => {
                params.error("limit", "invalid", "a number from 1 to 200");
                default
            }
        },
    };
    let cursor = params.optional("cursor");
    params.finish()?;
    let before = cursor
        .map(|text| {
            cursor_position(&text).ok_or_else(|| {
                ApiError::new(
                    rid,
                    ProblemCode::CursorInvalid,
                    "the cursor is not one this list gave; restart from the first page",
                )
            })
        })
        .transpose()?;
    let view = api.alerts.get(project_id, id).await.map_err(problem(rid))?;
    // One row more than the page tells whether there is another page.
    let mut rows = api
        .alerts
        .history(project_id, id, before, limit + 1)
        .await
        .map_err(problem(rid))?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = rows
        .last()
        .filter(|_| has_more)
        .map(|row| cursor_text(row.at, row.id));
    let mut sat = Saturation::default();
    let data = rows
        .iter()
        .map(|row| wire::AlertEvent {
            id: row.id.to_string(),
            kind: match row.kind {
                control::AlertEventKind::Transition => wire::AlertEventKind::Transition,
                control::AlertEventKind::Renotify => wire::AlertEventKind::Renotify,
                control::AlertEventKind::EvaluationFailing => {
                    wire::AlertEventKind::EvaluationFailing
                }
            },
            key: key(&view.spec.dimensions, &row.series_key),
            from_status: status(row.from_status),
            to_status: status(row.to_status),
            value: row.value.map(|v| sat.i64(v)),
            notified: row.notified,
            at: row.at.to_string(),
        })
        .collect();
    Ok(Json(wire::AlertHistory {
        data,
        next_cursor,
        has_more,
        saturated: sat.saturated(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_cursor_reads_back() {
        let at = Timestamp::from_micros(1_800_000_000_123_456);
        let id = Uuid::from_v7_parts(1_800_000_000_000, 7);
        assert_eq!(cursor_position(&cursor_text(at, id)), Some((at, id)));
        assert_eq!(cursor_position("x"), None);
        assert_eq!(cursor_position("1_not-an-id"), None);
    }

    #[test]
    fn each_issue_names_its_wire_member() {
        assert_eq!(member("window"), "window_secs");
        assert_eq!(member("for"), "for_secs");
        assert_eq!(member("threshold"), "threshold");
    }
}
