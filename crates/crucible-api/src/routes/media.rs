//! Media (contracts 12.6 and 14): `POST projects/{project_id}/media` asks for an upload, `PATCH
//! .../media/{media_id}` confirms it, `GET .../media/{media_id}` answers the object with a presigned download,
//! and `GET .../media` lists where the media of a trace or a span appear.
//!
//! Media need an object store and the control plane, which holds the objects and their uses. Without both,
//! every route checks its request and answers 501 `media_not_supported` with the settings that turn media on.
//! Uploads and confirms need the `ingest` scope; reads need `read`.

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types::{
    Media, MediaCreate, MediaDisposition, MediaList, MediaListParams, MediaState, MediaUpload,
    MediaUploadUrl, MediaUse, Problem, ProblemCode, Saturation, SignedHeader,
};
use crucible_control::{MediaObject, TxError};
use crucible_env::{Disposition, ObjectError, Presigned};
use crucible_media::{Created, MediaError, NewUpload, rules};
use crucible_records::{AuthorKind, MediaObjectId, RequestId, SpanId, TraceId};
use utoipa::OpenApi;

use crate::access::{ApiState, IngestMedia, Principal, Read, Scoped};
use crate::idempotency::{self, Begin};
use crate::params::Params;
use crate::problem::{ApiError, field_error};
use crate::writer::FactAuthor;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/projects/{project_id}/media",
            get(list_media).post(create_media),
        )
        .route(
            "/projects/{project_id}/media/{media_id}",
            get(get_media).patch(confirm_media),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(create_media, confirm_media, get_media, list_media))]
pub(super) struct Paths;

/// The payload fields a use may name: the payload route's selectors (contract 12.6).
pub const FIELDS: [&str; 12] = [
    "input",
    "output",
    "system_instructions",
    "metadata",
    "attributes",
    "resource_attributes",
    "events",
    "links",
    "tool_definitions",
    "tool_calls",
    "mcp_args",
    "mcp_result",
];

/// `media_not_supported`, with what turns media on (contract 1.7).
pub(crate) fn not_supported(rid: RequestId) -> ApiError {
    ApiError::new(
        rid,
        ProblemCode::MediaNotSupported,
        "media need an object store and the control plane: set object_store.endpoint, object_store.bucket, \
         object_store.access_key and object_store.secret_key (or their CRUCIBLE_OBJECT_STORE_* variables) and \
         postgres.url; without them, keep media inline in the payload under the 8 MiB field cap",
    )
}

fn service(state: &ApiState, rid: RequestId) -> Result<&crucible_media::Media, ApiError> {
    state.media.as_ref().ok_or_else(|| not_supported(rid))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The DTO of an object, with the download when there is one.
pub(crate) fn media_dto(object: &MediaObject, link: Option<&Presigned>) -> Media {
    let mut saturation = Saturation::default();
    Media {
        id: object.id.to_string(),
        content_type: object.content_type.clone(),
        len: saturation.u64(object.len),
        sha256: hex(&object.sha256),
        state: match object.state {
            crucible_control::MediaState::Pending => MediaState::Pending,
            crucible_control::MediaState::Uploaded => MediaState::Uploaded,
        },
        disposition: match rules::disposition(&object.content_type) {
            Disposition::Inline => MediaDisposition::Inline,
            Disposition::Attachment => MediaDisposition::Attachment,
        },
        created_at: object.created_at.to_string(),
        uploaded_at: object.uploaded_at.map(|t| t.to_string()),
        url: link.map(|link| link.url.clone()),
        url_expires_at: link.map(|link| link.expires_at.to_string()),
        saturated: saturation.saturated(),
    }
}

/// The problem of a failed media call.
pub(crate) fn media_problem(rid: RequestId, error: &MediaError) -> ApiError {
    match error {
        MediaError::TooLarge { max } => ApiError::new(
            rid,
            ProblemCode::MediaTooLarge,
            format!(
                "the object is larger than the cap of {max} bytes; send a smaller one, or raise media.max_bytes"
            ),
        )
        .with_limit("media.max_bytes", u32::try_from(*max).unwrap_or(u32::MAX)),
        MediaError::ContentType => ApiError::validation(
            rid,
            vec![field_error(
                "content_type",
                "not_allowed",
                "the content type is not on the allowlist; send a common image, audio, video, text or \
                 document type, or application/octet-stream",
            )],
        ),
        MediaError::NotFound => no_media(rid),
        MediaError::NotUploaded => ApiError::new(
            rid,
            ProblemCode::Conflict,
            "the object store holds nothing for this media yet; PUT the bytes to the upload URL, then confirm",
        ),
        MediaError::Mismatch => ApiError::new(
            rid,
            ProblemCode::MediaHashMismatch,
            "the uploaded object does not match the declared length or SHA-256, and was deleted; upload the \
             declared bytes again, then confirm",
        ),
        MediaError::Control(
            TxError::Unavailable(_) | TxError::CommitUnknown | TxError::SerializationFailure,
        ) => ApiError::new(
            rid,
            ProblemCode::ControlPlaneUnavailable,
            "the control plane (Postgres) is unavailable; try again shortly",
        )
        .with_retry_after(5),
        MediaError::Objects(ObjectError::Unavailable(_)) => ApiError::new(
            rid,
            ProblemCode::InternalError,
            "the object store did not answer; try again shortly",
        )
        .with_retry_after(5),
        other => {
            tracing::error!(request_id = %rid, error = %other, "a media call failed");
            ApiError::new(rid, ProblemCode::InternalError, "internal error")
        }
    }
}

/// The answer of a media route that cannot run here: `media_not_supported`, for the compat media endpoint.
pub fn media_not_supported(rid: RequestId) -> Response {
    not_supported(rid).into_response()
}

/// The answer to a failed media call, for the compat media endpoint.
pub fn media_refusal(rid: RequestId, error: &MediaError) -> Response {
    media_problem(rid, error).into_response()
}

/// The answer to a request with wrong fields, each a JSON pointer and what to send instead, for the compat
/// media endpoint.
pub fn media_invalid(rid: RequestId, errors: &[(String, String)]) -> Response {
    let errors = errors
        .iter()
        .map(|(pointer, message)| crucible_api_types::FieldError {
            pointer: pointer.clone(),
            code: "invalid".to_owned(),
            message: message.clone(),
        })
        .collect();
    ApiError::validation(rid, errors).into_response()
}

/// `not_found` for a media id that names no media of the project.
fn no_media(rid: RequestId) -> ApiError {
    ApiError::new(
        rid,
        ProblemCode::NotFound,
        "no media of this project has this id; POST media answers the ids",
    )
}

fn path_media(project: &Scoped<impl Send + Sync>) -> Result<MediaObjectId, ApiError> {
    project
        .path
        .get("media_id")
        .and_then(|id| MediaObjectId::parse(id).ok())
        .ok_or_else(|| no_media(project.request_id))
}

/// The use a create records, or the problems of its fields.
fn upload_of(rid: RequestId, request: &MediaCreate) -> Result<NewUpload, ApiError> {
    let mut errors = Vec::new();
    let sha256 = parse_hex::<32>(&request.sha256);
    if sha256.is_none() {
        errors.push(field_error(
            "sha256",
            "invalid_hash",
            "send the SHA-256 of the bytes as 64 lowercase hexadecimal characters",
        ));
    }
    let trace_id = TraceId::parse(&request.trace_id)
        .ok()
        .filter(|id| !id.is_zero());
    if trace_id.is_none() {
        errors.push(field_error(
            "trace_id",
            "invalid_id",
            "expected 32 hex characters, not all zero",
        ));
    }
    let span_id = match request.span_id.as_deref() {
        None => None,
        Some(text) => {
            let id = SpanId::parse(text).ok().filter(|id| !id.is_zero());
            if id.is_none() {
                errors.push(field_error(
                    "span_id",
                    "invalid_id",
                    "expected 16 hex characters, not all zero",
                ));
            }
            id
        }
    };
    let field = request.field.clone().unwrap_or_default();
    if !field.is_empty() && !FIELDS.contains(&field.as_str()) {
        errors.push(field_error(
            "field",
            "invalid",
            &format!("expected one of {}", FIELDS.join(", ")),
        ));
    }
    match (sha256, trace_id) {
        (Some(sha256), Some(trace_id)) if errors.is_empty() => Ok(NewUpload {
            content_type: request.content_type.clone(),
            len: request.len.get(),
            sha256,
            trace_id,
            span_id,
            field,
        }),
        _ => Err(ApiError::validation(rid, errors)),
    }
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if pairs.len() != N || !rest.is_empty() {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (byte, [high, low]) in out.iter_mut().zip(pairs) {
        *byte = digit(*high)? << 4 | digit(*low)?;
    }
    Some(out)
}

/// The DTO of a create's answer.
fn upload_dto(created: &Created) -> MediaUpload {
    MediaUpload {
        media_id: created.object().id.to_string(),
        upload: match created {
            Created::Existing(_) => None,
            Created::Upload(_, link) => Some(MediaUploadUrl {
                url: link.url.clone(),
                headers: link
                    .headers
                    .iter()
                    .map(|(name, value)| SignedHeader {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
                expires_at: link.expires_at.to_string(),
            }),
        },
    }
}

/// Ask for an upload
///
/// Declares bytes a span uses and answers the media id with a presigned `PUT`, valid for an hour, whose
/// content type, length and SHA-256 are signed headers: send them exactly as listed. When the project already
/// holds these bytes, the answer has no upload, and the use is recorded at once. Then upload the bytes to the
/// URL and confirm with `PATCH media/{media_id}`.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/media",
    operation_id = "create_media",
    tag = "media",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`; a repeat with the same body answers as the first request did."),
    ),
    request_body = MediaCreate,
    responses(
        (status = 201, description = "The media id, and the upload when the bytes are not stored yet.", body = MediaUpload),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: uploading needs an API key with the `ingest` scope.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused` or `conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`media_too_large`: above `media.max_bytes`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 501, description = "`media_not_supported`: no object store is configured.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn create_media(
    State(state): State<ApiState>,
    project: Scoped<IngestMedia>,
    headers: HeaderMap,
    request: Result<Json<MediaCreate>, JsonRejection>,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let key = idempotency::key(&headers).map_err(|()| {
        let message = "`Idempotency-Key` must be 1 to 128 characters of A-Z, a-z, 0-9, `_` and `-`";
        ApiError::validation(
            rid,
            vec![field_error("", "invalid_idempotency_key", message)],
        )
    })?;
    let Json(request) = request.map_err(|rejection| {
        ApiError::validation(
            rid,
            vec![field_error("", "invalid_body", &rejection.body_text())],
        )
    })?;
    let upload = upload_of(rid, &request)?;
    let media = service(&state, rid)?;
    media
        .check(&upload.content_type, upload.len)
        .map_err(|e| media_problem(rid, &e))?;
    let reservation = match &key {
        Some(key) => {
            let author = match &project.principal {
                Principal::Key(grant) => FactAuthor {
                    kind: AuthorKind::ApiKey,
                    id: grant.principal.api_key_id.uuid(),
                },
                Principal::Session(session) => FactAuthor {
                    kind: AuthorKind::User,
                    id: session.user.id.uuid(),
                },
            };
            let raw = serde_json::to_vec(&request).unwrap_or_default();
            let hash = idempotency::request_hash("media", &raw);
            match state.idempotency.begin(
                (author, "media", key.as_str()),
                hash,
                state.env.clock.wall_now(),
            ) {
                Begin::Run(reservation) => Some(reservation),
                Begin::Replay(status, body) => {
                    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::CREATED);
                    return Ok((
                        status,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                        .into_response());
                }
                Begin::InFlight => {
                    return Err(ApiError::new(
                        rid,
                        ProblemCode::Conflict,
                        "a request with this `Idempotency-Key` is still running; retry once it answers",
                    ));
                }
                Begin::Reused => {
                    return Err(ApiError::new(
                        rid,
                        ProblemCode::IdempotencyKeyReused,
                        "this `Idempotency-Key` was used for another request; send a new key",
                    ));
                }
            }
        }
        None => None,
    };
    let created = media
        .create(project.scope, upload)
        .await
        .map_err(|e| media_problem(rid, &e))?;
    let dto = upload_dto(&created);
    if let Some(reservation) = reservation {
        reservation.complete(
            StatusCode::CREATED.as_u16(),
            serde_json::to_vec(&dto).unwrap_or_default(),
        );
    }
    Ok((StatusCode::CREATED, Json(dto)).into_response())
}

/// Confirm an upload
///
/// Checks with the object store that the bytes were uploaded with the declared length (and SHA-256, where the
/// store reports it) and marks the media uploaded. Confirming uploaded media answers it again.
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/media/{media_id}",
    operation_id = "confirm_media",
    tag = "media",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("media_id" = String, Path, description = "The media id (a UUID)."),
    ),
    responses(
        (status = 200, description = "The media, uploaded, with a presigned download.", body = Media),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`: confirming needs an API key with the `ingest` scope.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no media of the project has this id, or its upload expired after 24 hours.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: nothing was uploaded yet.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`media_hash_mismatch`: the upload differs from what was declared; upload again.", body = Problem, content_type = "application/problem+json"),
        (status = 501, description = "`media_not_supported`: no object store is configured.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn confirm_media(
    State(state): State<ApiState>,
    project: Scoped<IngestMedia>,
) -> Result<Json<Media>, ApiError> {
    let rid = project.request_id;
    let id = path_media(&project)?;
    let media = service(&state, rid)?;
    let object = media
        .confirm(project.scope, id)
        .await
        .map_err(|e| media_problem(rid, &e))?;
    let (object, link) = media
        .with_link(object)
        .map_err(|e| media_problem(rid, &e))?;
    Ok(Json(media_dto(&object, link.as_ref())))
}

/// Get media
///
/// The media's metadata and, once it is uploaded, a presigned download on the object store that lives 5
/// minutes. Images, audio and video of common types are served inline; every other type downloads as a file.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/media/{media_id}",
    operation_id = "get_media",
    tag = "media",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("media_id" = String, Path, description = "The media id (a UUID)."),
    ),
    responses(
        (status = 200, description = "The media.", body = Media),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no media of the project has this id, or it was deleted.", body = Problem, content_type = "application/problem+json"),
        (status = 501, description = "`media_not_supported`: no object store is configured.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_media(
    State(state): State<ApiState>,
    project: Scoped<Read>,
) -> Result<Json<Media>, ApiError> {
    let rid = project.request_id;
    let id = path_media(&project)?;
    let media = service(&state, rid)?;
    let (object, link) = media
        .get(project.scope, id)
        .await
        .map_err(|e| media_problem(rid, &e))?;
    Ok(Json(media_dto(&object, link.as_ref())))
}

/// List media uses
///
/// Where media appear in one trace, or in one span of it: one entry per use, by payload field, each with its
/// media's metadata. Read `GET media/{media_id}` for a download.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/media",
    operation_id = "list_media",
    tag = "media",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), MediaListParams),
    responses(
        (status = 200, description = "The uses.", body = MediaList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 501, description = "`media_not_supported`: no object store is configured.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_media(
    State(state): State<ApiState>,
    project: Scoped<Read>,
    uri: Uri,
) -> Result<Json<MediaList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let trace_id = params.required("trace_id").and_then(|text| {
        let id = TraceId::parse(&text).ok();
        if id.is_none() {
            params.error("trace_id", "invalid_id", "expected 32 hex characters");
        }
        id
    });
    let span_id = params.optional("span_id").and_then(|text| {
        let id = SpanId::parse(&text).ok();
        if id.is_none() {
            params.error("span_id", "invalid_id", "expected 16 hex characters");
        }
        id
    });
    params.finish()?;
    let Some(trace_id) = trace_id else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let media = service(&state, rid)?;
    let uses = media
        .uses(project.scope, trace_id, span_id)
        .await
        .map_err(|e| media_problem(rid, &e))?;
    Ok(Json(MediaList {
        data: uses
            .iter()
            .map(|usage| MediaUse {
                span_id: usage.reference.span_id.map(|id| id.to_string()),
                field: Some(usage.reference.field.clone()).filter(|field| !field.is_empty()),
                media: media_dto(&usage.object, None),
            })
            .collect(),
    }))
}
