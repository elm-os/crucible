//! Authentication before rmcp reads a request (contract 12.2): a project's secret key as Bearer, or its key pair
//! as Basic, verified by the [`KeyVerifier`]. The verified principal travels in the request's extensions, which
//! rmcp hands to the tool call with the rest of the request's parts, so each call reads the project of the
//! credential it was sent with.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::{AUTHORIZATION, RETRY_AFTER};
use axum::middleware::Next;
use axum::response::Response;
use crucible_api_types::ProblemCode;
use crucible_auth::{KeyPrincipal, KeyVerifier, Refusal};
use crucible_env::Env;

/// The verified principal of a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Authorized(pub(crate) KeyPrincipal);

/// What the authentication layer needs.
#[derive(Clone, Debug)]
pub(crate) struct AuthState {
    pub(crate) env: Env,
    pub(crate) keys: Arc<dyn KeyVerifier>,
}

/// Verifies the request's credential and records its principal, or answers 401 with what to fix. A key the server
/// cannot check during a Postgres outage is 503 with `Retry-After` instead (contract 11.8). The refusal never
/// echoes the credential.
pub(crate) async fn authenticate(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let verified = match request.headers().get(AUTHORIZATION) {
        None => Err(Refusal::Missing),
        Some(value) => value
            .to_str()
            .map_err(|_| Refusal::Malformed)
            .and_then(|value| crucible_auth::authenticate(state.keys.as_ref(), value)),
    };
    match verified {
        Ok(principal) => {
            request.extensions_mut().insert(Authorized(principal));
            next.run(request).await
        }
        Err(Refusal::Unavailable) => {
            let id = crucible_api::request_id(request.extensions(), &state.env);
            let mut response = crucible_api::problem(
                id,
                ProblemCode::ControlPlaneUnavailable,
                &Refusal::Unavailable.to_string(),
            );
            response.headers_mut().insert(
                RETRY_AFTER,
                HeaderValue::from(crucible_auth::RETRY_AFTER.as_secs()),
            );
            response
        }
        Err(refusal) => {
            let id = crucible_api::request_id(request.extensions(), &state.env);
            crucible_api::problem(id, ProblemCode::Unauthenticated, &refusal.to_string())
        }
    }
}

/// The answer of the MCP path while no telemetry store is up.
pub(crate) fn store_unavailable(env: &Env, request: &Request) -> Response {
    let id = crucible_api::request_id(request.extensions(), env);
    crucible_api::problem(
        id,
        ProblemCode::TelemetryStoreUnavailable,
        "the MCP server reads the telemetry store, and none is up: start the server with --clickhouse-url \
         (clickhouse.url), or retry while the store starts",
    )
}
