//! Notification channels: `projects/{project_id}/channels/...` (contract 12.6).
//!
//! Listing channels needs `edit_content`, so that whoever edits a rule can pick from them; the URL of a webhook
//! and the recipients of an email channel are shown only to callers with the `channels` permission. Everything
//! else, test-send included, needs `channels`: project admins. No API key holds either permission (contracts
//! 11.3 and 11.6).
//! A channel's secret is never read back. A target the egress policy refuses is `ssrf_blocked`, with the
//! reason as the error's code and the setting that allows it in its message (contract 1.7).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, ProblemCode, SafeU64};
use crucible_auth::roles::ProjectPermission;
use crucible_control as control;
use crucible_notify::{ChannelError, ChannelPatch, ChannelView, Target, TargetError};
use crucible_records::{ChannelId, RequestId};
use serde_json::Value;
use utoipa::OpenApi;

use super::alerts::store_problem;
use crate::access::{ApiState, Channels, EditContent, Principal, Scoped};
use crate::control::{ApiJson, etag, if_match, version};
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    let base = "/projects/{project_id}/channels";
    Router::new()
        .route(base, get(list_channels).post(create_channel))
        .route(
            &format!("{base}/{{channel_id}}"),
            get(get_channel).patch(patch_channel).delete(delete_channel),
        )
        .route(&format!("{base}/{{channel_id}}/test"), post(test_channel))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    list_channels,
    create_channel,
    get_channel,
    patch_channel,
    delete_channel,
    test_channel,
))]
pub(super) struct Paths;

fn problem(rid: RequestId) -> impl FnOnce(ChannelError) -> ApiError {
    move |error| {
        let detail = error.to_string();
        match error {
            ChannelError::NotFound => ApiError::not_found(rid),
            ChannelError::VersionMismatch => ApiError::new(
                rid,
                ProblemCode::PreconditionFailed,
                "the channel changed since it was read; read it again and repeat the change with its new ETag",
            ),
            ChannelError::Target(TargetError::Invalid { field, message }) => {
                ApiError::validation(rid, vec![field_error(field, "invalid", &message)])
            }
            ChannelError::Target(TargetError::Refused(refusal)) => {
                ApiError::new(rid, ProblemCode::SsrfBlocked, detail.clone())
                    .with_errors(vec![field_error("url", refusal.reason(), &detail)])
            }
            ChannelError::Target(TargetError::SmtpNotConfigured) => ApiError::validation(
                rid,
                vec![field_error("kind", "smtp_not_configured", &detail)],
            ),
            ChannelError::Secret(_) => ApiError::new(rid, ProblemCode::Conflict, detail),
            ChannelError::RateLimited { retry_after_secs } => {
                ApiError::new(rid, ProblemCode::RateLimited, detail)
                    .with_retry_after(retry_after_secs)
            }
            ChannelError::Store(error) => store_problem(rid, &error),
        }
    }
}

const fn kind(kind: control::ChannelKind) -> wire::ChannelKind {
    match kind {
        control::ChannelKind::Slack => wire::ChannelKind::Slack,
        control::ChannelKind::Email => wire::ChannelKind::Email,
        control::ChannelKind::Webhook => wire::ChannelKind::Webhook,
    }
}

/// The stored outcome of a test-send, read back.
fn last_test(doc: &control::JsonDoc) -> Option<wire::ChannelTestResult> {
    let value: Value = serde_json::from_str(&doc.to_text()).ok()?;
    let text = |member: &str| value.get(member).and_then(Value::as_str).map(str::to_owned);
    Some(wire::ChannelTestResult {
        ok: value.get("ok")?.as_bool()?,
        status_class: text("status_class"),
        latency_ms: SafeU64::saturating(value.get("latency_ms")?.as_u64()?).0,
        error_code: text("error_code"),
        at: text("at"),
    })
}

/// The channel on the wire. `full` is whether the caller may manage channels: only then are the webhook URL
/// and the recipients part of it.
fn channel(view: &ChannelView, full: bool, signing_secret: Option<String>) -> wire::Channel {
    let row = &view.channel;
    wire::Channel {
        id: row.id.to_string(),
        name: row.name.clone(),
        kind: kind(row.kind),
        enabled: row.enabled,
        disabled_reason: row.disabled_reason.clone(),
        failures: row.failures,
        url: view.url.clone().filter(|_| full),
        url_hint: view.url_hint.clone(),
        to: if full { view.to.clone() } else { Vec::new() },
        last_test: row.last_test.as_ref().and_then(last_test),
        signing_secret,
        version: version(row.version),
    }
}

fn channel_id<P>(scoped: &Scoped<P>) -> Result<ChannelId, ApiError> {
    scoped
        .path
        .get("channel_id")
        .and_then(|text| ChannelId::parse(text).ok())
        .ok_or_else(|| ApiError::not_found(scoped.request_id))
}

/// The target a body names for a channel of `kind`: a URL for Slack and webhooks, recipients for email.
fn target(
    rid: RequestId,
    kind: control::ChannelKind,
    url: Option<String>,
    to: Option<Vec<String>>,
) -> Result<Target, ApiError> {
    let missing = |field: &str, message: &str| {
        ApiError::validation(rid, vec![field_error(field, "required", message)])
    };
    let unused = |field: &str, message: &str| {
        ApiError::validation(rid, vec![field_error(field, "invalid", message)])
    };
    match kind {
        control::ChannelKind::Email => {
            if url.is_some() {
                return Err(unused(
                    "url",
                    "an email channel has recipients (`to`), not a URL",
                ));
            }
            Ok(Target::Email {
                to: to.ok_or_else(|| missing("to", "an email channel needs 1 to 20 recipients"))?,
            })
        }
        control::ChannelKind::Slack | control::ChannelKind::Webhook => {
            if to.is_some() {
                return Err(unused("to", "only an email channel has recipients"));
            }
            let url = url.ok_or_else(|| missing("url", "the channel needs the URL to call"))?;
            Ok(if kind == control::ChannelKind::Slack {
                Target::Slack { url }
            } else {
                Target::Webhook { url }
            })
        }
    }
}

/// List channels
///
/// The channels of the project, for whoever edits alert rules. The URL of a webhook and the recipients of an
/// email channel are left out for callers who may not manage channels.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/channels",
    operation_id = "list_channels",
    tag = "channels",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 200, description = "The channels.", body = wire::ChannelList),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`, or `feature_disabled` without a control plane.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn list_channels(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
) -> Result<Json<wire::ChannelList>, ApiError> {
    let rid = scoped.request_id;
    let project_id = scoped.scope.project_id();
    let full = match &scoped.principal {
        Principal::Session(session) => session
            .authorize(project_id, ProjectPermission::Channels)
            .is_ok(),
        Principal::Key(_) => false,
    };
    let views = state
        .alerts(rid)?
        .channels
        .list(project_id)
        .await
        .map_err(problem(rid))?;
    Ok(Json(wire::ChannelList {
        data: views.iter().map(|view| channel(view, full, None)).collect(),
    }))
}

/// Create a channel
///
/// A Slack incoming webhook, an email channel or a signed webhook. A webhook channel gets a signing secret,
/// which this answer carries once. The target is checked against the egress policy.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/channels",
    operation_id = "create_channel",
    tag = "channels",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::ChannelCreate,
    responses(
        (status = 201, description = "The channel; `ETag` is its version.", body = wire::Channel),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, or `ssrf_blocked` with the reason and the setting that allows the target.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_channel(
    State(state): State<ApiState>,
    scoped: Scoped<Channels>,
    ApiJson(body): ApiJson<wire::ChannelCreate>,
) -> Result<(StatusCode, HeaderMap, Json<wire::Channel>), ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let kind = match body.kind {
        wire::ChannelKind::Slack => control::ChannelKind::Slack,
        wire::ChannelKind::Email => control::ChannelKind::Email,
        wire::ChannelKind::Webhook => control::ChannelKind::Webhook,
    };
    let target = target(rid, kind, body.url, body.to)?;
    let saved = api
        .channels
        .create(scoped.scope.project_id(), &body.name, target)
        .await
        .map_err(problem(rid))?;
    Ok((
        StatusCode::CREATED,
        etag(saved.view.channel.version),
        Json(channel(&saved.view, true, saved.secret)),
    ))
}

/// Get a channel
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/channels/{channel_id}",
    operation_id = "get_channel",
    tag = "channels",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("channel_id" = String, Path, description = "The channel id."),
    ),
    responses(
        (status = 200, description = "The channel; `ETag` is its version.", body = wire::Channel),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn get_channel(
    State(state): State<ApiState>,
    scoped: Scoped<Channels>,
) -> Result<(HeaderMap, Json<wire::Channel>), ApiError> {
    let rid = scoped.request_id;
    let view = state
        .alerts(rid)?
        .channels
        .get(scoped.scope.project_id(), channel_id(&scoped)?)
        .await
        .map_err(problem(rid))?;
    Ok((etag(view.channel.version), Json(channel(&view, true, None))))
}

/// Change a channel
///
/// Renames it, turns it on or off, replaces its target, or replaces the signing secret of a webhook channel.
/// Turning a channel on clears its failure count and the reason it was turned off.
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/channels/{channel_id}",
    operation_id = "patch_channel",
    tag = "channels",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("channel_id" = String, Path, description = "The channel id."),
        ("If-Match" = String, Header, description = "The channel's `ETag`."),
    ),
    request_body = wire::ChannelPatch,
    responses(
        (status = 200, description = "The channel.", body = wire::Channel),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed` or `ssrf_blocked`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_channel(
    State(state): State<ApiState>,
    scoped: Scoped<Channels>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::ChannelPatch>,
) -> Result<(HeaderMap, Json<wire::Channel>), ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let (project_id, id) = (scoped.scope.project_id(), channel_id(&scoped)?);
    let expected = if_match(&headers, rid)?;
    let target = if body.url.is_some() || body.to.is_some() {
        let current = api
            .channels
            .get(project_id, id)
            .await
            .map_err(problem(rid))?;
        Some(target(rid, current.channel.kind, body.url, body.to)?)
    } else {
        None
    };
    let patch = ChannelPatch {
        name: body.name,
        enabled: body.enabled,
        target,
        rotate_secret: body.rotate_secret.unwrap_or(false),
    };
    let saved = api
        .channels
        .update(project_id, id, expected, patch)
        .await
        .map_err(problem(rid))?;
    Ok((
        etag(saved.view.channel.version),
        Json(channel(&saved.view, true, saved.secret)),
    ))
}

/// Delete a channel
///
/// Deletes the channel and its secret. Rules that name it stop notifying it.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/channels/{channel_id}",
    operation_id = "delete_channel",
    tag = "channels",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("channel_id" = String, Path, description = "The channel id."),
        ("If-Match" = String, Header, description = "The channel's `ETag`."),
    ),
    responses(
        (status = 204, description = "Deleted."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_channel(
    State(state): State<ApiState>,
    scoped: Scoped<Channels>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let rid = scoped.request_id;
    let api = state.alerts(rid)?;
    let id = channel_id(&scoped)?;
    let expected = if_match(&headers, rid)?;
    api.channels
        .delete(scoped.scope.project_id(), id, expected)
        .await
        .map_err(problem(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Send a test notification
///
/// One synchronous attempt, at most 6 a minute per channel. The answer says whether the receiver accepted the
/// test, the class of its status and how long it took; it never carries what the receiver answered. A test
/// works on a channel that is turned off, so a fix can be checked before the channel is turned on again.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/channels/{channel_id}/test",
    operation_id = "test_channel",
    tag = "channels",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("channel_id" = String, Path, description = "The channel id."),
    ),
    responses(
        (status = 200, description = "The outcome, also when the receiver refused.", body = wire::ChannelTestResult),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn test_channel(
    State(state): State<ApiState>,
    scoped: Scoped<Channels>,
) -> Result<Json<wire::ChannelTestResult>, ApiError> {
    let rid = scoped.request_id;
    let delivery = state
        .alerts(rid)?
        .channels
        .test(scoped.scope.project_id(), channel_id(&scoped)?)
        .await
        .map_err(problem(rid))?;
    Ok(Json(wire::ChannelTestResult {
        ok: delivery.ok,
        status_class: delivery.status_class.map(str::to_owned),
        latency_ms: SafeU64::saturating(delivery.latency_ms).0,
        error_code: delivery.error_code.map(str::to_owned),
        at: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_test_outcome_reads_back_without_anything_else() {
        let doc = control::JsonDoc::parse(
            r#"{"ok":false,"status_class":"5xx","latency_ms":12,"error_code":"receiver_failed","at":"2026-11-18T00:00:00.000000Z","body":"never shown"}"#,
        )
        .unwrap();
        let outcome = last_test(&doc).unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.status_class.as_deref(), Some("5xx"));
        assert_eq!(outcome.error_code.as_deref(), Some("receiver_failed"));
        assert!(!serde_json::to_string(&outcome).unwrap().contains("never"));
    }
}
