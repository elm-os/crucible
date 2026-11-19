//! Facts (contract 3.8, 12.5 and 12.6): `POST projects/{project_id}/feedback`, `.../tags`, `.../flags` and
//! `.../corrections` append one fact each through the WAL and answer once it is durable; `GET .../facts` reads
//! the effective facts of one target.
//!
//! A fact is filed under its target's earliest known start, which the route looks up: a target the project has
//! not sent is `not_found` when the lookup is whole and strong, and `telemetry_store_unavailable` with
//! `Retry-After` while it cannot be told. Writes honour `Idempotency-Key`.

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{
    CorrectionRequest, FactWritten, FactsParams, FeedbackRequest, FieldError, FlagName,
    FlagRequest, Problem, ProblemCode, SafeU64, ScoreValue, TagRequest,
};
use crucible_query::{Consistency, FactTarget, Facts, Meta, TargetFacts};
use crucible_records::{
    CorrectionBody, FactBody, FlagBody, ScoreBody, ScoreDataType, ScoreSource, SpanId, TagBody,
    TraceId, caps,
};
use utoipa::OpenApi;

use crate::access::{ApiState, Project, ProjectWrite};
use crate::body::json;
use crate::convert;
use crate::idempotency::{self, Begin};
use crate::params::Params;
use crate::problem::{ApiError, field_error};
use crate::writer::{FactRefusal, NewFact};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/projects/{project_id}/corrections",
            post(post_correction).fallback(super::post_only),
        )
        .route("/projects/{project_id}/facts", get(get_facts))
        .route(
            "/projects/{project_id}/feedback",
            post(post_feedback).fallback(super::post_only),
        )
        .route(
            "/projects/{project_id}/flags",
            post(post_flag).fallback(super::post_only),
        )
        .route(
            "/projects/{project_id}/tags",
            post(post_tag).fallback(super::post_only),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(get_facts, post_correction, post_feedback, post_flag, post_tag))]
pub(super) struct Paths;

/// One fact before the target's start is looked up.
struct Draft {
    target: FactTarget,
    key: String,
    retracted: bool,
    body: FactBody,
}

fn invalid(errors: &mut Vec<FieldError>, name: &str, code: &str, message: &str) {
    errors.push(field_error(name, code, message));
}

/// The target named by `trace_id`, `span_id` and `session_id`.
fn target(
    trace_id: Option<&str>,
    span_id: Option<&str>,
    session_id: Option<&str>,
    errors: &mut Vec<FieldError>,
) -> Option<FactTarget> {
    if session_id.is_some() && (trace_id.is_some() || span_id.is_some()) {
        let message = "`session_id` cannot be combined with `trace_id` or `span_id`";
        invalid(errors, "session_id", "conflict", message);
        return None;
    }
    match (trace_id, span_id, session_id) {
        (None, None, None) => {
            let message =
                "name the target with `trace_id`, `trace_id` and `span_id`, or `session_id`";
            invalid(errors, "trace_id", "required", message);
            None
        }
        (None, Some(_), None) => {
            invalid(
                errors,
                "trace_id",
                "required",
                "a span is named with its `trace_id`",
            );
            None
        }
        (_, _, Some(session)) => {
            if session.is_empty() || session.len() > caps::TRACE_ATTRIBUTE_BYTES {
                invalid(errors, "session_id", "invalid", "expected 1 to 512 bytes");
                return None;
            }
            Some(FactTarget::Session(session.into()))
        }
        (Some(trace), span, _) => {
            let trace = TraceId::parse(trace).ok().filter(|id| *id != TraceId::ZERO);
            if trace.is_none() {
                invalid(
                    errors,
                    "trace_id",
                    "invalid_id",
                    "expected 32 hex characters, not all zero",
                );
            }
            let span = match span {
                Some(text) => {
                    let span = SpanId::parse(text).ok().filter(|id| *id != SpanId::ZERO);
                    if span.is_none() {
                        invalid(
                            errors,
                            "span_id",
                            "invalid_id",
                            "expected 16 hex characters, not all zero",
                        );
                    }
                    Some(span?)
                }
                None => None,
            };
            Some(match span {
                Some(span) => FactTarget::Span(trace?, span),
                None => FactTarget::Trace(trace?),
            })
        }
    }
}

fn checked(
    request_id: crucible_records::RequestId,
    errors: Vec<FieldError>,
) -> Result<(), ApiError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation(request_id, errors))
    }
}

/// Whether a lookup that found nothing means the target does not exist.
fn whole(meta: &Meta) -> bool {
    !meta.partial && meta.consistency == Consistency::Strong
}

fn unknown_target(project: &Project, target: &FactTarget, meta: &Meta) -> ApiError {
    let rid = project.request_id;
    if !whole(meta) {
        return ApiError::new(
            rid,
            ProblemCode::TelemetryStoreUnavailable,
            "the target cannot be looked up while the telemetry store catches up; retry",
        )
        .with_retry_after(1);
    }
    let detail = match target {
        FactTarget::Session(_) => {
            "no session with this id is stored; sessions are stored about ten seconds after their spans"
        }
        _ => "the project has not sent this trace; send its spans before writing facts about it",
    };
    ApiError::new(rid, ProblemCode::NotFound, detail)
}

fn refusal(project: &Project, refusal: FactRefusal) -> ApiError {
    let rid = project.request_id;
    match refusal {
        FactRefusal::Invalid(reason) => {
            ApiError::validation(rid, vec![field_error("", "invalid", reason)])
        }
        FactRefusal::TooLarge(reason) => ApiError::new(rid, ProblemCode::PayloadTooLarge, reason),
        FactRefusal::Throttled { retry_after_s } => ApiError::new(
            rid,
            ProblemCode::RateLimited,
            "the server is taking too many writes; retry after `retry_after_s`",
        )
        .with_retry_after(retry_after_s),
        FactRefusal::Unavailable { retry_after_s } => ApiError::new(
            rid,
            ProblemCode::IngestUnavailable,
            "the write-ahead log cannot take the fact now; retry after `retry_after_s`",
        )
        .with_retry_after(retry_after_s),
        FactRefusal::Internal(reason) => {
            tracing::error!(request_id = %rid, reason, "a fact write failed");
            ApiError::new(rid, ProblemCode::InternalError, "the fact was not written")
        }
    }
}

/// Looks up the target's start and appends the fact.
async fn append(
    state: &ApiState,
    project: &Project,
    draft: Draft,
) -> Result<FactWritten, ApiError> {
    let rid = project.request_id;
    let service = state.service(rid)?;
    let writer = state.facts.as_ref().ok_or_else(|| {
        ApiError::new(
            rid,
            ProblemCode::IngestUnavailable,
            "no fact writer is configured",
        )
    })?;
    let bounds = service
        .fact_target_bounds(&project.ctx, &draft.target)
        .await;
    let bounds = bounds.map_err(|e| ApiError::query(rid, &e))?;
    let Some(target_time) = bounds.start else {
        return Err(unknown_target(project, &draft.target, &bounds.meta));
    };
    let (trace_id, span_id, session_id) = match &draft.target {
        FactTarget::Trace(trace) => (*trace, SpanId::ZERO, "".into()),
        FactTarget::Span(trace, span) => (*trace, *span, "".into()),
        FactTarget::Session(session) => (TraceId::ZERO, SpanId::ZERO, Box::from(&**session)),
    };
    let fact = NewFact {
        target_kind: draft.target.kind(),
        trace_id,
        span_id,
        session_id,
        target_time,
        key: draft.key.clone().into(),
        retracted: draft.retracted,
        body: draft.body,
    };
    let author = project.author;
    let accepted = writer.append(project.ctx.scope(), author, vec![fact]).await;
    let accepted = accepted.map_err(|e| refusal(project, e))?;
    let accepted = accepted.first().ok_or_else(|| {
        ApiError::new(rid, ProblemCode::InternalError, "the fact was not written")
    })?;
    Ok(FactWritten {
        fact_id: accepted.fact_id.to_string(),
        key: draft.key,
        version: SafeU64::saturating(accepted.version).0,
        created_at: accepted.created_at.to_string(),
        target_time: target_time.to_string(),
    })
}

fn answer(status: StatusCode, body: Vec<u8>) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body).into_response()
}

/// Writes one fact under the request's `Idempotency-Key`, if it has one.
async fn write(
    state: &ApiState,
    project: &Project,
    headers: &HeaderMap,
    raw: &Bytes,
    route: &'static str,
    draft: Draft,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let key = idempotency::key(headers).map_err(|()| {
        let message = "`Idempotency-Key` must be 1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`";
        ApiError::validation(
            rid,
            vec![field_error("", "invalid_idempotency_key", message)],
        )
    })?;
    let reservation = match &key {
        Some(key) => {
            let slot = (project.author, route, key.as_str());
            let hash = idempotency::request_hash(route, raw);
            match state
                .idempotency
                .begin(slot, hash, state.env.clock.wall_now())
            {
                Begin::Run(reservation) => Some(reservation),
                Begin::Replay(status, body) => {
                    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::CREATED);
                    return Ok(answer(status, body));
                }
                Begin::InFlight => {
                    let detail = "a request with this `Idempotency-Key` is still running; retry once it answers";
                    return Err(ApiError::new(rid, ProblemCode::Conflict, detail));
                }
                Begin::Reused => {
                    let detail =
                        "this `Idempotency-Key` was used for another request; send a new key";
                    return Err(ApiError::new(
                        rid,
                        ProblemCode::IdempotencyKeyReused,
                        detail,
                    ));
                }
            }
        }
        None => None,
    };
    let written = append(state, project, draft).await?;
    // Serializing these plain fields cannot fail.
    let body = serde_json::to_vec(&written).unwrap_or_default();
    if let Some(reservation) = reservation {
        reservation.complete(StatusCode::CREATED.as_u16(), body.clone());
    }
    Ok(answer(StatusCode::CREATED, body))
}

fn body(project: &Project, raw: Result<Bytes, BytesRejection>) -> Result<Bytes, ApiError> {
    raw.map_err(|_| {
        let detail = format!("the request body is above {} bytes", crate::BODY_LIMIT);
        ApiError::new(project.request_id, ProblemCode::PayloadTooLarge, detail)
    })
}

fn target_of(
    trace_id: Option<&String>,
    span_id: Option<&String>,
    session_id: Option<&String>,
    errors: &mut Vec<FieldError>,
) -> Option<FactTarget> {
    target(
        trace_id.map(String::as_str),
        span_id.map(String::as_str),
        session_id.map(String::as_str),
        errors,
    )
}

/// Record feedback
///
/// Writes a score with `source = feedback` about a trace, a span or a session. A score with the same name
/// (and `score_id`) replaces the earlier one; `retracted: true` removes it.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/feedback",
    operation_id = "post_feedback",
    tag = "facts",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`; a repeat with the same body answers as the first request did."),
    ),
    request_body(content = FeedbackRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "The fact is durable.", body = FactWritten),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project, or the project has not sent the target.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused` or `conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`payload_too_large`.", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "`unsupported_media_type`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`ingest_unavailable` or `telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn post_feedback(
    State(state): State<ApiState>,
    ProjectWrite(project): ProjectWrite,
    headers: HeaderMap,
    raw: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let raw = body(&project, raw)?;
    let request: FeedbackRequest = json(rid, &headers, &raw)?;
    let mut errors = Vec::new();
    let target = target_of(
        request.trace_id.as_ref(),
        request.span_id.as_ref(),
        request.session_id.as_ref(),
        &mut errors,
    );
    if request.name.is_empty() || request.name.contains('#') {
        invalid(
            &mut errors,
            "name",
            "invalid",
            "a score name is not empty and has no `#`",
        );
    }
    let key = match &request.score_id {
        Some(id) => format!("{}#{id}", request.name),
        None => request.name.clone(),
    };
    if key.len() > caps::FACT_KEY_BYTES {
        invalid(
            &mut errors,
            "name",
            "too_long",
            "the name and `score_id` together are above 255 bytes",
        );
    }
    use ScoreDataType as T;
    let value = match (&request.value, request.data_type.map(ScoreDataType::from)) {
        (ScoreValue::Number(n), None | Some(T::Numeric)) if n.is_finite() => {
            Some((T::Numeric, *n, String::new()))
        }
        (ScoreValue::Bool(b), None | Some(T::Boolean)) => {
            Some((T::Boolean, if *b { 1.0 } else { 0.0 }, String::new()))
        }
        (ScoreValue::Text(text), None | Some(T::Categorical)) => {
            Some((T::Categorical, 0.0, text.clone()))
        }
        (ScoreValue::Text(text), Some(T::Text)) => Some((T::Text, 0.0, text.clone())),
        (ScoreValue::Number(_), None | Some(T::Numeric)) => {
            invalid(&mut errors, "value", "invalid", "expected a finite number");
            None
        }
        _ => {
            let message =
                "`numeric` takes a number, `boolean` a boolean, `categorical` and `text` a string";
            invalid(&mut errors, "data_type", "mismatch", message);
            None
        }
    };
    let metadata = match &request.metadata {
        None => String::new(),
        Some(object @ serde_json::Value::Object(_)) => object.to_string(),
        Some(_) => {
            invalid(&mut errors, "metadata", "invalid", "expected a JSON object");
            String::new()
        }
    };
    checked(rid, errors)?;
    let (Some(target), Some((data_type, value_num, value_str))) = (target, value) else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let draft = Draft {
        target,
        key,
        retracted: request.retracted.unwrap_or(false),
        body: FactBody::Score(ScoreBody {
            data_type,
            value_num,
            value_str: value_str.into(),
            comment: request.comment.unwrap_or_default().into(),
            source: ScoreSource::Feedback,
            metadata: Bytes::from(metadata),
            config_id: None,
        }),
    };
    write(&state, &project, &headers, &raw, "feedback", draft).await
}

/// Edit a tag
///
/// Adds a tag to a trace, a span or a session, or removes it. A trace's effective tags are the tags it was
/// sent with, plus the added ones, minus the removed ones.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/tags",
    operation_id = "post_tag",
    tag = "facts",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`; a repeat with the same body answers as the first request did."),
    ),
    request_body(content = TagRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "The fact is durable.", body = FactWritten),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project, or the project has not sent the target.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused` or `conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`payload_too_large`.", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "`unsupported_media_type`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`ingest_unavailable` or `telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn post_tag(
    State(state): State<ApiState>,
    ProjectWrite(project): ProjectWrite,
    headers: HeaderMap,
    raw: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let raw = body(&project, raw)?;
    let request: TagRequest = json(rid, &headers, &raw)?;
    let mut errors = Vec::new();
    let target = target_of(
        request.trace_id.as_ref(),
        request.span_id.as_ref(),
        request.session_id.as_ref(),
        &mut errors,
    );
    if request.tag.is_empty() || request.tag.len() > caps::FACT_KEY_BYTES {
        invalid(&mut errors, "tag", "invalid", "expected 1 to 256 bytes");
    }
    checked(rid, errors)?;
    let Some(target) = target else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let draft = Draft {
        target,
        key: request.tag,
        retracted: false,
        body: FactBody::Tag(TagBody {
            op: request.op.into(),
        }),
    };
    write(&state, &project, &headers, &raw, "tags", draft).await
}

/// Set a flag
///
/// Sets the `bookmarked` or `public` flag of a trace, a span or a session.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/flags",
    operation_id = "post_flag",
    tag = "facts",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`; a repeat with the same body answers as the first request did."),
    ),
    request_body(content = FlagRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "The fact is durable.", body = FactWritten),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project, or the project has not sent the target.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused` or `conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`payload_too_large`.", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "`unsupported_media_type`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`ingest_unavailable` or `telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn post_flag(
    State(state): State<ApiState>,
    ProjectWrite(project): ProjectWrite,
    headers: HeaderMap,
    raw: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let raw = body(&project, raw)?;
    let request: FlagRequest = json(rid, &headers, &raw)?;
    let mut errors = Vec::new();
    let target = target_of(
        request.trace_id.as_ref(),
        request.span_id.as_ref(),
        request.session_id.as_ref(),
        &mut errors,
    );
    checked(rid, errors)?;
    let Some(target) = target else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let key = match request.flag {
        FlagName::Bookmarked => FlagBody::BOOKMARKED,
        FlagName::Public => FlagBody::PUBLIC,
    };
    let draft = Draft {
        target,
        key: key.to_owned(),
        retracted: false,
        body: FactBody::Flag(FlagBody {
            value: request.value,
        }),
    };
    write(&state, &project, &headers, &raw, "flags", draft).await
}

/// Correct an output
///
/// Records the corrected output of a trace, a span or a session; `retracted: true` removes the correction.
/// The body is at most 1 MiB.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/corrections",
    operation_id = "post_correction",
    tag = "facts",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`; a repeat with the same body answers as the first request did."),
    ),
    request_body(content = CorrectionRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "The fact is durable.", body = FactWritten),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project, or the project has not sent the target.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused` or `conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`payload_too_large`.", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "`unsupported_media_type`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`ingest_unavailable` or `telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn post_correction(
    State(state): State<ApiState>,
    ProjectWrite(project): ProjectWrite,
    headers: HeaderMap,
    raw: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let raw = body(&project, raw)?;
    let request: CorrectionRequest = json(rid, &headers, &raw)?;
    let mut errors = Vec::new();
    let target = target_of(
        request.trace_id.as_ref(),
        request.span_id.as_ref(),
        request.session_id.as_ref(),
        &mut errors,
    );
    checked(rid, errors)?;
    let Some(target) = target else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let draft = Draft {
        target,
        key: CorrectionBody::KEY.to_owned(),
        retracted: request.retracted.unwrap_or(false),
        body: FactBody::Correction(CorrectionBody {
            corrected: Bytes::from(request.corrected.to_string()),
            note: request.note.unwrap_or_default().into(),
        }),
    };
    write(&state, &project, &headers, &raw, "corrections", draft).await
}

/// Get the facts of a target
///
/// The effective facts of a trace, a span or a session: per key the latest write, without retracted keys.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/facts",
    operation_id = "get_facts",
    tag = "facts",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), FactsParams),
    responses(
        (status = 200, description = "The facts.", body = wire::Facts),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project, or the project has not sent the target.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_facts(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::Facts>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let trace_id = params.optional("trace_id");
    let span_id = params.optional("span_id");
    let session_id = params.optional("session_id");
    params.finish()?;
    let mut errors = Vec::new();
    let target = target_of(
        trace_id.as_ref(),
        span_id.as_ref(),
        session_id.as_ref(),
        &mut errors,
    );
    checked(rid, errors)?;
    let Some(target) = target else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let service = state.service(rid)?;
    let bounds = service.fact_target_bounds(&project.ctx, &target).await;
    let bounds = bounds.map_err(|e| ApiError::query(rid, &e))?;
    let facts = match bounds.window {
        Some(window) => {
            let facts = service.facts(&project.ctx, &target, window).await;
            facts.map_err(|e| ApiError::query(rid, &e))?
        }
        None if whole(&bounds.meta) => return Err(ApiError::not_found(rid)),
        None => TargetFacts {
            facts: Facts::default(),
            meta: bounds.meta,
        },
    };
    Ok(Json(convert::facts(&facts)))
}
