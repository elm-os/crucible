//! The REST API under `/api/v1` (contract section 12): the routes of milestone 1, principal resolution and the
//! permission check, problem documents, the middleware of contract 12.1 and the OpenAPI document.
//!
//! - **Routes.** `GET projects/{project_id}/traces`, `.../traces/{trace_id}`, `.../traces/{trace_id}/spans`,
//!   `.../spans`, `.../sessions`, `.../sessions/{session_id}`, `.../users`, `.../users/{user_id}`,
//!   `.../users/{user_id}/sessions` and `.../data-bounds` over a [`QueryService`], one module per route
//!   group; DTOs come from `crucible-api-types`.
//! - **Auth.** A project's secret key as `Authorization: Bearer` (contract 12.2), verified by a
//!   [`KeyVerifier`]. The key's project must be the path's; any other project, or a malformed id, is
//!   `not_found` (contract 11.6). Reads run in the `agent` class with the project's retention, which a
//!   [`ProjectSettingsSource`] supplies.
//! - **Errors.** Every error is `application/problem+json` with the request id (contract 12.3), unknown paths
//!   and methods included.
//! - **Middleware**, outermost first: request id, security headers with the API's CSP, body limit, request
//!   timeout on the `Env` clock. The request id and the other security headers also wrap the main listener's
//!   other routes through [`common_layers`].
//! - **Cursors** are signed with the node's [`KeyRing`] (contract 12.4), bound to the request's project, list,
//!   filters, sort and window, and expire 24 h after issue. A cursor that does not open is `cursor_invalid`,
//!   checked with the other parameters before the store is asked.

mod access;
mod authz;
mod body;
mod control;
mod convert;
mod cursor;
mod filters;
mod idempotency;
mod layers;
mod openapi;
mod params;
mod problem;
mod routes;
mod settings;
mod writer;

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware;
use axum::response::Response;
use crucible_api_types::ProblemCode;
use crucible_auth::KeyVerifier;
use crucible_auth::service::Identity;
use crucible_cursor::KeyRing;
use crucible_env::Env;
use crucible_query::QueryService;

pub use authz::{Access, ROUTES, RouteAccess, authz_matrix};
pub use layers::{BODY_LIMIT, REQUEST_TIMEOUT, X_REQUEST_ID, common_layers};
pub use openapi::{openapi, openapi_json};
pub use problem::{PROBLEM_JSON, problem, request_id};
pub use settings::{
    DEFAULT_MAX_WINDOW_DAYS, ProjectSettings, ProjectSettingsSource, StaticProjectSettings,
};
pub use writer::{AcceptedFact, FactAppend, FactAuthor, FactRefusal, FactWriter, NewFact};

use crate::access::ApiState;

/// The base path of the API (contract 12.1).
pub const BASE_PATH: &str = "/api/v1";

/// The API over a query service, with its routes under [`BASE_PATH`], signing cursors with `cursors` and
/// writing facts through `facts`. Merge it into the main listener's router, then wrap that router in
/// [`common_layers`].
pub fn router(
    env: &Env,
    service: QueryService,
    keys: Arc<dyn KeyVerifier>,
    settings: Arc<dyn ProjectSettingsSource>,
    cursors: Arc<KeyRing>,
    facts: Arc<dyn FactWriter>,
) -> Router {
    build(env, Some((service, facts)), keys, settings, cursors, None)
}

/// The API before a telemetry store is configured: requests are authenticated and validated as usual, cursors
/// included, then answered with `telemetry_store_unavailable`.
pub fn router_without_store(
    env: &Env,
    keys: Arc<dyn KeyVerifier>,
    settings: Arc<dyn ProjectSettingsSource>,
    cursors: Arc<KeyRing>,
) -> Router {
    build(env, None, keys, settings, cursors, None)
}

/// The API of a node with a control plane: sessions, members and keys from Postgres through `identity`, whose
/// key snapshot verifies API keys with their scopes. `store`, the query service and where facts are written, is
/// `None` until a telemetry store is up.
pub fn platform_router(
    env: &Env,
    store: Option<(QueryService, Arc<dyn FactWriter>)>,
    identity: Arc<Identity>,
    settings: Arc<dyn ProjectSettingsSource>,
    cursors: Arc<KeyRing>,
) -> Router {
    let keys: Arc<dyn KeyVerifier> = Arc::clone(identity.keys()) as Arc<dyn KeyVerifier>;
    build(env, store, keys, settings, cursors, Some(identity))
}

fn build(
    env: &Env,
    store: Option<(QueryService, Arc<dyn FactWriter>)>,
    keys: Arc<dyn KeyVerifier>,
    settings: Arc<dyn ProjectSettingsSource>,
    cursors: Arc<KeyRing>,
    identity: Option<Arc<Identity>>,
) -> Router {
    let (service, facts) = store.unzip();
    let state = ApiState {
        env: env.clone(),
        service,
        keys,
        settings,
        cursors,
        identity,
        facts,
        idempotency: Arc::default(),
    };
    let api = routes::router()
        .fallback(unknown_path)
        .method_not_allowed_fallback(unknown_method)
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            env.clone(),
            layers::request_timeout,
        ))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(
            env.clone(),
            layers::body_limit,
        ))
        .layer(middleware::from_fn(layers::api_policy));
    common_layers(Router::new().nest(BASE_PATH, api), env)
}

/// Paths under `/api/v1` that name no route never fall back to the UI (contract 12.1).
async fn unknown_path(State(state): State<ApiState>, request: Request) -> Response {
    let id = request_id(request.extensions(), &state.env);
    problem(id, ProblemCode::NotFound, "no resource exists at this path")
}

/// A method the path does not support. axum lists the supported ones in `Allow`.
async fn unknown_method(State(state): State<ApiState>, request: Request) -> Response {
    let id = request_id(request.extensions(), &state.env);
    problem(
        id,
        ProblemCode::MethodNotAllowed,
        "the resource does not support this method; see the Allow header",
    )
}
