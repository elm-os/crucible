//! Media on this node (contract 14): the object store and the media service over the control plane, the
//! collector job, and the compat media endpoint, which both HTTP ingest listeners serve.
//!
//! Without `[object_store]` there is no media: the API's media routes and the compat media endpoint check the
//! request's key and answer 501 `media_not_supported` with the settings that turn media on, and SDKs keep media
//! inline in their payloads under the field cap.

use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use crucible_api_types::ProblemCode;
use crucible_auth::{KeyPrincipal, KeyVerifier, Refusal, authenticate};
use crucible_compat::media::{
    MEDIA_ROUTES, MediaAnswer, hash_range, media_answer, media_id, parse_report, parse_upload,
    upload_answer,
};
use crucible_config::Config;
use crucible_control::{ControlStore, MediaState};
use crucible_env::{Env, ObjectStore, TlsRoots};
use crucible_jobs::Job;
use crucible_media::{Created, Media, MediaConfig, MediaError, NewUpload, S3Config, S3Store};
use crucible_records::RequestId;

use crate::control::Control;

/// The most bytes of a compat media request body that are read: its JSON fields only.
const BODY_LIMIT: usize = 64 << 10;

/// Media on this node.
#[derive(Clone, Debug)]
pub(crate) struct NodeMedia {
    /// What the API's media routes and the compat media endpoint call.
    pub(crate) service: Media,
    store: Arc<S3Store>,
    prefix: String,
    /// The origin of the object store's public endpoint, which the UI's pages may load media from.
    pub(crate) origin: String,
}

/// Media over the control plane, when `[object_store]` is set.
pub(crate) fn build(
    env: &Env,
    config: &Config,
    control: &Control,
) -> anyhow::Result<Option<NodeMedia>> {
    let Some(settings) = &config.object_store else {
        return Ok(None);
    };
    let s3 = S3Config {
        endpoint: settings.endpoint.clone(),
        public_endpoint: settings.public_endpoint.clone(),
        region: settings.region.clone(),
        bucket: settings.bucket.clone(),
        access_key: settings.access_key.clone(),
        secret_key: settings.secret_key.expose().to_owned(),
        virtual_host: settings.virtual_host,
    };
    let store =
        Arc::new(S3Store::new(env, &s3, TlsRoots::WebPki).context("the [object_store] settings")?);
    let service = Media::new(
        env,
        Arc::new(control.store.clone()) as Arc<dyn ControlStore>,
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        MediaConfig {
            prefix: settings.prefix.clone(),
            max_bytes: config.media_max_bytes,
        },
    );
    Ok(Some(NodeMedia {
        service,
        origin: store.public_origin(),
        store,
        prefix: settings.prefix.clone(),
    }))
}

impl NodeMedia {
    /// The `media-gc` job.
    pub(crate) fn job(&self) -> Job {
        crucible_media::gc::job(
            Arc::clone(&self.store) as Arc<dyn ObjectStore>,
            self.prefix.clone(),
            crucible_media::gc::GcConfig::default(),
        )
    }

    /// Checks once, in the background, that the bucket answers, and says in the log what to fix when it does
    /// not. Media requests fail until it does; nothing else waits for it.
    pub(crate) fn check(&self, env: &Env) {
        let store = Arc::clone(&self.store);
        let _detached = env.spawn("media-check", async move {
            match store.check().await {
                Ok(()) => tracing::info!("the media bucket answers"),
                Err(error) => tracing::error!(
                    %error,
                    "the media bucket does not answer: check object_store.endpoint, object_store.bucket (the \
                     bucket must exist), object_store.access_key and object_store.secret_key"
                ),
            }
        });
    }
}

/// What the compat media endpoint works with.
#[derive(Clone)]
struct Compat {
    env: Env,
    /// Keys with the `ingest` scope: uploads and confirms.
    ingest: Arc<dyn KeyVerifier>,
    /// Keys with the `read` scope: reads.
    read: Arc<dyn KeyVerifier>,
    media: Option<Media>,
}

/// The compat media endpoint's routes (contracts 12.1 and 14.3), for both HTTP ingest listeners.
pub(crate) fn compat_routes(
    env: &Env,
    ingest: Arc<dyn KeyVerifier>,
    read: Arc<dyn KeyVerifier>,
    media: Option<Media>,
) -> Router {
    let state = Compat {
        env: env.clone(),
        ingest,
        read,
        media,
    };
    let mut router = Router::new();
    for route in MEDIA_ROUTES {
        router = router.route(route.collection, post(upload)).route(
            &format!("{}/{{media_id}}", route.collection),
            get(read_media).patch(report),
        );
    }
    router.with_state(state)
}

fn json(status: StatusCode, body: Vec<u8>) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body).into_response()
}

/// The key of a request, or the answer that refuses it: 401 with what to fix, or 503 while the control plane
/// cannot check the key.
fn principal(
    keys: &dyn KeyVerifier,
    request: &Request,
    rid: RequestId,
) -> Result<KeyPrincipal, Response> {
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let refusal = match header {
        None => Refusal::Missing,
        Some(value) => match authenticate(keys, value) {
            Ok(principal) => return Ok(principal),
            Err(refusal) => refusal,
        },
    };
    let detail = refusal.to_string();
    Err(match refusal {
        Refusal::Unavailable => {
            let mut response =
                crucible_api::problem(rid, ProblemCode::ControlPlaneUnavailable, &detail);
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("5"));
            response
        }
        _ => crucible_api::problem(rid, ProblemCode::Unauthenticated, &detail),
    })
}

/// The request id, the key's principal, the media service and the body of a request, or the answer that
/// refuses it.
async fn begin(
    state: &Compat,
    keys: &dyn KeyVerifier,
    request: Request,
) -> Result<(RequestId, KeyPrincipal, Media, axum::body::Bytes), Response> {
    let rid = crucible_api::request_id(request.extensions(), &state.env);
    let principal = principal(keys, &request, rid)?;
    let Some(media) = state.media.clone() else {
        return Err(crucible_api::media_not_supported(rid));
    };
    let body = to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| {
            crucible_api::problem(
                rid,
                ProblemCode::PayloadTooLarge,
                "the request body is above 64 KiB; send only the media's fields, never its bytes",
            )
        })?;
    Ok((rid, principal, media, body))
}

fn invalid(rid: RequestId, errors: Vec<crucible_compat::media::FieldError>) -> Response {
    let pairs: Vec<(String, String)> = errors
        .into_iter()
        .map(|error| (error.pointer, error.message))
        .collect();
    crucible_api::media_invalid(rid, &pairs)
}

fn no_media(rid: RequestId) -> Response {
    crucible_api::problem(
        rid,
        ProblemCode::NotFound,
        "no media of this project has this id; ask for an upload first",
    )
}

/// `POST <collection>`: asks for an upload.
async fn upload(State(state): State<Compat>, request: Request) -> Response {
    let keys = Arc::clone(&state.ingest);
    let (rid, principal, media, body) = match begin(&state, keys.as_ref(), request).await {
        Ok(begun) => begun,
        Err(response) => return response,
    };
    let request = match parse_upload(&body) {
        Ok(request) => request,
        Err(errors) => return invalid(rid, errors),
    };
    let id = media_id(&request.sha256);
    let created = media
        .create(
            principal.scope,
            NewUpload {
                content_type: request.content_type,
                len: request.len,
                sha256: request.sha256,
                trace_id: request.trace_id,
                span_id: request.span_id,
                field: request.field,
            },
        )
        .await;
    match created {
        Ok(Created::Existing(_)) => json(StatusCode::CREATED, upload_answer(&id, None)),
        Ok(Created::Upload(_, link)) => {
            json(StatusCode::CREATED, upload_answer(&id, Some(&link.url)))
        }
        Err(error) => crucible_api::media_refusal(rid, &error),
    }
}

/// The object a compat media id names, if the project holds one.
async fn find(
    media: &Media,
    principal: &KeyPrincipal,
    id: &str,
) -> Result<Option<crucible_control::MediaObject>, MediaError> {
    let Some((low, high)) = hash_range(id) else {
        return Ok(None);
    };
    let found = media
        .get_by_hash_prefix(principal.scope, &low, &high)
        .await?;
    Ok(found.into_iter().next())
}

/// `GET <collection>/<mediaId>`: the object and a presigned download.
async fn read_media(
    State(state): State<Compat>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let keys = Arc::clone(&state.read);
    let (rid, principal, media, _) = match begin(&state, keys.as_ref(), request).await {
        Ok(begun) => begun,
        Err(response) => return response,
    };
    let object = match find(&media, &principal, &id).await {
        Ok(Some(object)) if object.state == MediaState::Uploaded => object,
        Ok(_) => return no_media(rid),
        Err(error) => return crucible_api::media_refusal(rid, &error),
    };
    let (object, link) = match media.with_link(object) {
        Ok((object, Some(link))) => (object, link),
        Ok(_) => return no_media(rid),
        Err(error) => return crucible_api::media_refusal(rid, &error),
    };
    let body = media_answer(&MediaAnswer {
        media_id: &id,
        content_type: &object.content_type,
        len: object.len,
        uploaded_at: object.uploaded_at.map(|at| at.to_string()),
        url: &link.url,
        url_expiry: link.expires_at.to_string(),
    });
    json(StatusCode::OK, body)
}

/// `PATCH <collection>/<mediaId>`: the SDK reports its upload; a successful one is confirmed.
async fn report(State(state): State<Compat>, Path(id): Path<String>, request: Request) -> Response {
    let keys = Arc::clone(&state.ingest);
    let (rid, principal, media, body) = match begin(&state, keys.as_ref(), request).await {
        Ok(begun) => begun,
        Err(response) => return response,
    };
    let succeeded = match parse_report(&body) {
        Ok(succeeded) => succeeded,
        Err(errors) => return invalid(rid, errors),
    };
    let object = match find(&media, &principal, &id).await {
        Ok(Some(object)) => object,
        Ok(None) => return no_media(rid),
        Err(error) => return crucible_api::media_refusal(rid, &error),
    };
    if !succeeded {
        // The upload failed on the SDK's side; the pending object is collected after 24 h.
        return (StatusCode::OK, Body::empty()).into_response();
    }
    match media.confirm(principal.scope, object.id).await {
        Ok(_) => (StatusCode::OK, Body::empty()).into_response(),
        Err(error) => crucible_api::media_refusal(rid, &error),
    }
}
