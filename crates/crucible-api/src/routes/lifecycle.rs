//! Retention settings and deletion requests: `projects/{project_id}/retention` and
//! `projects/{project_id}/deletion-requests` (contracts 12.6 and 13).
//!
//! Each route names its project permission through [`Scoped`] (contract 11.6): reading needs `read`, changing
//! retention `manage` (which no API key holds), a deletion request `delete_traces`, and one by user or session
//! also `delete_by_user`. Every refusal says what to send instead (contract 1.7).

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, ProblemCode};
use crucible_auth::roles::ProjectPermission;
use crucible_control::{DeletionRequest, DeletionSelector, TxError};
use crucible_lifecycle::{
    MAX_DELETION_PAGE, NewDeletion, Progress, Retention, Service, ServiceError,
};
use crucible_query::RetentionSettings;
use crucible_records::{DeletionRequestId, DeletionState, RequestId, TraceId};
use utoipa::OpenApi;

use crate::access::{ApiState, DeleteTraces, DeletionReads, Manage, Principal, Read, Scoped};
use crate::control::{ApiJson, etag, if_match, version};
use crate::idempotency;
use crate::params::Params;
use crate::problem::{ApiError, field_error};

/// How many trace ids a request shows of those it names.
const SHOWN_TRACE_IDS: usize = 20;
/// The default page of `GET deletion-requests`.
const DEFAULT_PAGE: u32 = 50;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    let base = "/projects/{project_id}";
    Router::new()
        .route(
            &format!("{base}/retention"),
            get(get_retention).put(put_retention),
        )
        .route(
            &format!("{base}/deletion-requests"),
            get(list_deletion_requests).post(create_deletion_request),
        )
        .route(
            &format!("{base}/deletion-requests/{{request_id}}"),
            get(get_deletion_request),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    get_retention,
    put_retention,
    list_deletion_requests,
    create_deletion_request,
    get_deletion_request,
))]
pub(super) struct Paths;

/// The lifecycle service, or `feature_disabled` with how to turn it on.
fn service(state: &ApiState, rid: RequestId) -> Result<&Service, ApiError> {
    state.lifecycle.as_ref().ok_or_else(|| {
        ApiError::new(
            rid,
            ProblemCode::FeatureDisabled,
            "retention settings and deletion requests need a control plane; set postgres.url (CRUCIBLE_POSTGRES_URL)",
        )
    })
}

fn failed(rid: RequestId) -> impl FnOnce(ServiceError) -> ApiError {
    move |error| {
        match error {
        ServiceError::Invalid { field, reason } => ApiError::validation(
            rid,
            vec![field_error(field, "invalid_value", &reason)],
        ),
        ServiceError::VersionMismatch { current } => ApiError::new(
            rid,
            ProblemCode::PreconditionFailed,
            format!(
                "the project changed since you read it (it is at version {current}); read the retention again and send its ETag in If-Match"
            ),
        ),
        ServiceError::NotFound => ApiError::not_found(rid),
        ServiceError::KeyReused => ApiError::new(
            rid,
            ProblemCode::IdempotencyKeyReused,
            "this `Idempotency-Key` was used for another request; send a new key",
        ),
        ServiceError::Log(_) => ApiError::new(
            rid,
            ProblemCode::IngestUnavailable,
            "the request is recorded, and this node's write-ahead log could not take its tombstone yet; the node retries within seconds, and sending the request again under the same Idempotency-Key is safe",
        )
        .with_retry_after(5),
        ServiceError::Store(TxError::Invalid(_)) => ApiError::new(
            rid,
            ProblemCode::InternalError,
            "the control plane refused the write",
        ),
        ServiceError::Store(_) => ApiError::new(
            rid,
            ProblemCode::ControlPlaneUnavailable,
            "the control plane (Postgres) is unavailable; try again shortly",
        )
        .with_retry_after(1),
    }
    }
}

fn retention(retention: &Retention) -> wire::Retention {
    let time = |t: Option<crucible_records::Timestamp>| t.map(|t| t.to_string());
    wire::Retention {
        retention_days: retention.settings.retention_days,
        payload_retention_days: retention.settings.payload_retention_days,
        mcp_content_retention_days: retention.settings.mcp_content_retention_days,
        hidden_before: wire::RetentionHidden {
            retention: time(retention.floors.retention),
            payload: time(retention.floors.payload),
            mcp_content: time(retention.floors.mcp_content),
        },
        version: version(retention.version),
    }
}

/// Get retention
///
/// The project's retention settings. Reads hide what they have expired at once; the rows are removed from the
/// telemetry store within a day.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/retention",
    operation_id = "get_retention",
    tag = "lifecycle",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The settings; `ETag` is the project's version.", body = wire::Retention),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_retention(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
) -> Result<(HeaderMap, Json<wire::Retention>), ApiError> {
    let rid = scoped.request_id;
    let found = service(&state, rid)?
        .retention(scoped.scope)
        .await
        .map_err(failed(rid))?;
    Ok((etag(found.version), Json(retention(&found))))
}

/// Set retention
///
/// Replaces the three settings. Shortening applies to the data the project already holds, at once for every
/// read; lengthening never shows data that a shorter setting had hidden. Needs `If-Match` with the `ETag` of
/// `GET retention`. API keys cannot change settings.
#[utoipa::path(
    put,
    path = "/api/v1/projects/{project_id}/retention",
    operation_id = "put_retention",
    tag = "lifecycle",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    request_body = wire::RetentionUpdate,
    responses(
        (status = 200, description = "The settings.", body = wire::Retention),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: retention needs the project admin role.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`: a value is out of range, or above `retention_days`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn put_retention(
    State(state): State<ApiState>,
    scoped: Scoped<Manage>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::RetentionUpdate>,
) -> Result<(HeaderMap, Json<wire::Retention>), ApiError> {
    let rid = scoped.request_id;
    if matches!(scoped.principal, Principal::Key(_)) {
        return Err(ApiError::new(
            rid,
            ProblemCode::Forbidden,
            "API keys cannot change project settings; sign in to the UI as a project admin",
        ));
    }
    let expected = if_match(&headers, rid)?;
    let new = RetentionSettings {
        retention_days: body.retention_days,
        payload_retention_days: body.payload_retention_days,
        mcp_content_retention_days: body.mcp_content_retention_days,
    };
    let stored = service(&state, rid)?
        .set_retention(scoped.scope, expected, new)
        .await
        .map_err(failed(rid))?;
    Ok((etag(stored.version), Json(retention(&stored))))
}

fn place(name: &str, status: &str, deletes: (u32, u32, u32)) -> wire::DeletionPlace {
    wire::DeletionPlace {
        place: name.to_owned(),
        status: status.to_owned(),
        deletes: deletes.0,
        deletes_done: deletes.1,
        deletes_verified: deletes.2,
    }
}

fn deletion(request: &DeletionRequest) -> wire::DeletionRequest {
    let progress = Progress::parse(&request.places);
    let mut places: Vec<wire::DeletionPlace> = progress
        .tables
        .iter()
        .map(|(table, at)| {
            let status = if at.failed > 0 {
                "failed"
            } else if at.verified == at.actions {
                "verified"
            } else if at.done == at.actions {
                "deleted"
            } else {
                "pending"
            };
            place(table, status, (at.actions, at.done, at.verified))
        })
        .collect();
    places.extend(
        progress
            .other
            .iter()
            .map(|(name, status)| place(name, status, (0, 0, 0))),
    );
    let (trace_count, trace_ids, user_id, issuer, session_id) = match &request.selector {
        DeletionSelector::Traces(ids) => (
            Some(u32::try_from(ids.len()).unwrap_or(u32::MAX)),
            Some(
                ids.iter()
                    .take(SHOWN_TRACE_IDS)
                    .map(ToString::to_string)
                    .collect(),
            ),
            None,
            None,
            None,
        ),
        DeletionSelector::User { user_id, issuer } => {
            (None, None, Some(user_id.clone()), issuer.clone(), None)
        }
        DeletionSelector::Session(session_id) => (None, None, None, None, Some(session_id.clone())),
        DeletionSelector::Project => (None, None, None, None, None),
    };
    wire::DeletionRequest {
        id: request.id.to_string(),
        kind: match &request.selector {
            DeletionSelector::Traces(_) => wire::DeletionKind::Trace,
            DeletionSelector::User { .. } => wire::DeletionKind::User,
            DeletionSelector::Session(_) => wire::DeletionKind::Session,
            DeletionSelector::Project => wire::DeletionKind::Project,
        },
        trace_count,
        trace_ids,
        user_id,
        issuer,
        session_id,
        state: match request.state {
            DeletionState::Pending => wire::DeletionState::Pending,
            DeletionState::Logged => wire::DeletionState::Logged,
            DeletionState::Applied => wire::DeletionState::Applied,
            DeletionState::Verified => wire::DeletionState::Verified,
            DeletionState::Completed => wire::DeletionState::Completed,
            DeletionState::Failed => wire::DeletionState::Failed,
        },
        verified: matches!(
            request.state,
            DeletionState::Verified | DeletionState::Completed
        ),
        requested_at: request.requested_at.to_string(),
        requested_by: request.requested_by.clone(),
        updated_at: request.updated_at.to_string(),
        traces_resolved: u32::try_from(progress.traces).unwrap_or(u32::MAX),
        places,
        error: request.error.clone(),
    }
}

/// One member of the body that the chosen kind does not take.
fn unexpected(rid: RequestId, member: &str, kind: &str) -> ApiError {
    ApiError::validation(
        rid,
        vec![field_error(
            member,
            "unexpected_member",
            &format!("a `{kind}` deletion does not take `{member}`; leave it out"),
        )],
    )
}

fn missing(rid: RequestId, member: &str, kind: &str) -> ApiError {
    ApiError::validation(
        rid,
        vec![field_error(
            member,
            "required",
            &format!("a `{kind}` deletion needs `{member}`"),
        )],
    )
}

/// The selector of a request body: exactly the members of its kind.
fn selector(
    rid: RequestId,
    body: wire::DeletionRequestCreate,
) -> Result<DeletionSelector, ApiError> {
    let wire::DeletionRequestCreate {
        kind,
        trace_ids,
        user_id,
        issuer,
        session_id,
    } = body;
    let name = match kind {
        wire::DeletionKind::Trace => "trace",
        wire::DeletionKind::User => "user",
        wire::DeletionKind::Session => "session",
        wire::DeletionKind::Project => return Err(whole_project(rid)),
    };
    let given = [
        (
            "trace_ids",
            trace_ids.is_some(),
            kind == wire::DeletionKind::Trace,
        ),
        (
            "user_id",
            user_id.is_some(),
            kind == wire::DeletionKind::User,
        ),
        ("issuer", issuer.is_some(), kind == wire::DeletionKind::User),
        (
            "session_id",
            session_id.is_some(),
            kind == wire::DeletionKind::Session,
        ),
    ];
    if let Some((member, _, _)) = given.iter().find(|(_, set, fits)| *set && !*fits) {
        return Err(unexpected(rid, member, name));
    }
    match kind {
        wire::DeletionKind::Trace => {
            let ids = trace_ids.ok_or_else(|| missing(rid, "trace_ids", name))?;
            let mut parsed = Vec::with_capacity(ids.len());
            for (at, id) in ids.iter().enumerate() {
                parsed.push(TraceId::parse(id).map_err(|_| {
                    ApiError::validation(
                        rid,
                        vec![field_error(
                            &format!("trace_ids/{at}"),
                            "invalid_id",
                            "expected 32 hex characters",
                        )],
                    )
                })?);
            }
            Ok(DeletionSelector::Traces(parsed))
        }
        wire::DeletionKind::User => Ok(DeletionSelector::User {
            user_id: user_id.ok_or_else(|| missing(rid, "user_id", name))?,
            issuer,
        }),
        wire::DeletionKind::Session => Ok(DeletionSelector::Session(
            session_id.ok_or_else(|| missing(rid, "session_id", name))?,
        )),
        wire::DeletionKind::Project => Err(whole_project(rid)),
    }
}

/// The refusal of a request body of kind `project`: deleting the project makes that request.
fn whole_project(rid: RequestId) -> ApiError {
    ApiError::validation(
        rid,
        vec![field_error(
            "kind",
            "invalid_value",
            "a whole project is deleted with the project: an org owner or admin sends DELETE \
             /api/v1/projects/{project_id}, which also removes its data",
        )],
    )
}

fn requester(principal: &Principal) -> String {
    match principal {
        Principal::Key(grant) => format!("key:{}", grant.principal.api_key_id),
        Principal::Session(session) => format!("user:{}", session.user.id),
    }
}

/// Request a deletion
///
/// Deletes a list of traces, everything of one user, or everything of one session. The delete is point in
/// time: data received after the request is kept. The answer comes once the request is durable; from then on
/// the API and the MCP server no longer return the data, and the request's `state` says how far the removal
/// from the telemetry store has come. Send an `Idempotency-Key` to make a retry safe. Deleting traces needs
/// the `delete_traces` permission (project member, or a key with the `delete` scope); deleting by user or
/// session needs `delete_by_user` (project admin, or a key with the `delete` scope).
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/deletion-requests",
    operation_id = "create_deletion_request",
    tag = "lifecycle",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`. A repeat with the same body returns the same request."),
    ),
    request_body = wire::DeletionRequestCreate,
    responses(
        (status = 201, description = "The request.", body = wire::DeletionRequest),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: the role or the key's scopes are too low.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`, or `ingest_unavailable` when the write-ahead log cannot take the request yet.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn create_deletion_request(
    State(state): State<ApiState>,
    scoped: Scoped<DeleteTraces>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::DeletionRequestCreate>,
) -> Result<Response, ApiError> {
    let rid = scoped.request_id;
    let key = idempotency::key(&headers).map_err(|()| {
        let message = "`Idempotency-Key` must be 1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`";
        ApiError::validation(
            rid,
            vec![field_error("", "invalid_idempotency_key", message)],
        )
    })?;
    // The parsed body in its one serialized form, so that two spellings of one request hash the same.
    let canonical = serde_json::to_vec(&body).unwrap_or_default();
    let hash = idempotency::request_hash("create_deletion_request", &canonical);
    let by_trace = body.kind == wire::DeletionKind::Trace;
    let selector = selector(rid, body)?;
    if !by_trace {
        let project_id = scoped.scope.project_id();
        let permission = ProjectPermission::DeleteByUser;
        let allowed = match &scoped.principal {
            Principal::Key(grant) => grant.authorize(project_id, permission).is_ok(),
            Principal::Session(session) => session.authorize(project_id, permission).is_ok(),
        };
        if !allowed {
            return Err(ApiError::new(
                rid,
                ProblemCode::Forbidden,
                "deleting by user or session needs the `delete_by_user` permission on the project; ask a project admin",
            ));
        }
    }
    let new = NewDeletion {
        selector,
        requested_by: requester(&scoped.principal),
        idempotency: key.map(|key| (key, hex(&hash))),
    };
    let accepted = service(&state, rid)?
        .request_deletion(scoped.scope, new)
        .await
        .map_err(failed(rid))?;
    let mut response = (StatusCode::CREATED, Json(deletion(&accepted.request))).into_response();
    let location = format!(
        "{}/projects/{}/deletion-requests/{}",
        crate::BASE_PATH,
        scoped.scope.project_id(),
        accepted.request.id
    );
    if let Ok(value) = HeaderValue::from_str(&location) {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, value);
    }
    Ok(response)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// List deletion requests
///
/// The project's deletion requests, newest first. An org owner or admin also lists those of a deleted project,
/// among them the request of kind `project` that removes its data.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/deletion-requests",
    operation_id = "list_deletion_requests",
    tag = "lifecycle",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("limit" = Option<u32>, Query, description = "How many requests: 1 to 100, default 50.", minimum = 1, maximum = 100),
        ("cursor" = Option<String>, Query, description = "`next_cursor` of the page before."),
    ),
    responses(
        (status = 200, description = "The requests.", body = wire::DeletionRequestList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project, or it is deleted and the caller is not an org owner or admin.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed` or `cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_deletion_requests(
    State(state): State<ApiState>,
    scoped: DeletionReads,
    uri: Uri,
) -> Result<Json<wire::DeletionRequestList>, ApiError> {
    let rid = scoped.request_id;
    let mut params = Params::new(rid, &uri)?;
    let limit = params.limit(MAX_DELETION_PAGE).unwrap_or(DEFAULT_PAGE);
    let cursor = params.optional("cursor");
    params.finish()?;
    // The cursor is the sequence number of the last request of the page before, inside the caller's project.
    let before = match cursor {
        Some(text) => Some(text.parse::<u64>().map_err(|_| {
            ApiError::new(
                rid,
                ProblemCode::CursorInvalid,
                "the cursor is not one this list returned; start again without it",
            )
        })?),
        None => None,
    };
    let mut requests = service(&state, rid)?
        .deletions(scoped.scope, before, limit + 1)
        .await
        .map_err(failed(rid))?;
    let more = requests.len() > limit as usize;
    requests.truncate(limit as usize);
    let next_cursor = requests
        .last()
        .filter(|_| more)
        .map(|last| last.deletion_seq.to_string());
    Ok(Json(wire::DeletionRequestList {
        data: requests.iter().map(deletion).collect(),
        next_cursor,
    }))
}

/// Get a deletion request
///
/// One request with its state and the status of each place it reaches. An org owner or admin also reads those
/// of a deleted project.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/deletion-requests/{request_id}",
    operation_id = "get_deletion_request",
    tag = "lifecycle",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("request_id" = String, Path, description = "The request id (a UUID)."),
    ),
    responses(
        (status = 200, description = "The request.", body = wire::DeletionRequest),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_deletion_request(
    State(state): State<ApiState>,
    scoped: DeletionReads,
) -> Result<Json<wire::DeletionRequest>, ApiError> {
    let rid = scoped.request_id;
    let id = scoped
        .path
        .get("request_id")
        .and_then(|id| id.parse::<crucible_records::Uuid>().ok())
        .map(DeletionRequestId::from_uuid)
        .ok_or_else(|| ApiError::not_found(rid))?;
    let found = service(&state, rid)?
        .deletion(scoped.scope, id)
        .await
        .map_err(failed(rid))?
        .ok_or_else(|| ApiError::not_found(rid))?;
    Ok(Json(deletion(&found)))
}
