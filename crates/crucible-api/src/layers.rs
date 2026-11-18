//! The middleware of contract 12.1 that milestone 1 needs, outermost first: request id, security headers (with
//! the API's CSP), body limit and the request timeout on the `Env` clock. The request id and the security
//! headers other than the CSP also wrap the UI and ingest routes of the main listener ([`common_layers`]).

use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::{
    CONTENT_LENGTH, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use crucible_api_types::ProblemCode;
use crucible_env::Env;
use crucible_records::RequestId;

use crate::problem::{ApiError, request_id};

/// The header that echoes the request id.
pub const X_REQUEST_ID: &str = "x-request-id";
/// Bodies above this many bytes are refused with `payload_too_large` (default; revisit).
pub const BODY_LIMIT: usize = 1 << 20;
/// A request that has not answered after this long is answered with `request_timeout`. It sits above every
/// query class deadline (contract 8.4), so a slow query ends with its own problem first (default; revisit).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The deadline of the SQL workspace's run and export routes. A statement may run for the profile's 30 s after
/// the server validated it, and must then end with its own problem (`query_timeout`), so the request deadline
/// sits above the store's (default; revisit).
pub const SQL_REQUEST_TIMEOUT: Duration = Duration::from_secs(75);
/// The Content Security Policy of API responses.
const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

/// Wraps `router` in the request-id and security-header layers, request id outermost. The main listener applies
/// this to all of its routes; applying it twice is harmless, because both layers keep what an outer instance
/// already set.
pub fn common_layers(router: Router, env: &Env) -> Router {
    router
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn_with_state(
            env.clone(),
            assign_request_id,
        ))
}

/// Mints the request id (contract 12.1), a UUIDv7 from `env.new_id()`, stores it as a [`RequestId`] request
/// extension and echoes it as `X-Request-Id`. An inbound `X-Request-Id` is ignored: no proxy is trusted yet.
async fn assign_request_id(State(env): State<Env>, mut request: Request, next: Next) -> Response {
    if request.extensions().get::<RequestId>().is_some() {
        return next.run(request).await;
    }
    let id = RequestId::from_uuid(env.new_id());
    request.extensions_mut().insert(id);
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert(X_REQUEST_ID, value);
    }
    response
}

/// `nosniff`, a same-origin referrer policy (cross-origin requests get no referrer, and same-origin requests
/// keep a real `Origin`), and the cross-origin opener and resource policies. The CSP is not among them: UI
/// pages carry their own, and a browser may merge the headers of a 304 into its cached page.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let defaults = [
        (X_CONTENT_TYPE_OPTIONS.as_str(), "nosniff"),
        (REFERRER_POLICY.as_str(), "same-origin"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
    ];
    for (name, value) in defaults {
        if !headers.contains_key(name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
    response
}

/// The policy of every API response: it loads nothing and is never framed.
pub(crate) async fn api_policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(API_CSP));
    response
}

/// Refuses a request whose declared body is above [`BODY_LIMIT`]; extractors that read a body stop at the
/// same limit.
pub(crate) async fn body_limit(State(env): State<Env>, request: Request, next: Next) -> Response {
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|len| len > BODY_LIMIT as u64) {
        let id = request_id(request.extensions(), &env);
        let detail = format!("the request body is above {BODY_LIMIT} bytes");
        return ApiError::new(id, ProblemCode::PayloadTooLarge, detail).into_response();
    }
    next.run(request).await
}

/// The deadline of a request by its path: [`SQL_REQUEST_TIMEOUT`] for the routes that run user SQL,
/// [`REQUEST_TIMEOUT`] for every other.
fn request_deadline(path: &str) -> Duration {
    let runs_sql = path.split_once("/sql/").is_some_and(|(_, rest)| {
        let rest = rest.trim_end_matches('/');
        rest == "run"
            || rest == "export"
            || (rest.starts_with("saved-queries/") && rest.ends_with("/run"))
    });
    if runs_sql {
        SQL_REQUEST_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    }
}

/// Answers `request_timeout` when the handler has not answered within its deadline on the `Env` clock.
pub(crate) async fn request_timeout(
    State(env): State<Env>,
    request: Request,
    next: Next,
) -> Response {
    let id = request_id(request.extensions(), &env);
    let deadline = request_deadline(request.uri().path());
    match env.timeout(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            id,
            ProblemCode::RequestTimeout,
            "the request did not finish within the server's deadline",
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_routes_that_run_sql_get_the_longer_deadline() {
        let base = "/api/v1/projects/p/sql";
        for path in ["run", "export", "saved-queries/q/run", "run/"] {
            assert_eq!(
                request_deadline(&format!("{base}/{path}")),
                SQL_REQUEST_TIMEOUT,
                "{path}"
            );
        }
        for path in [
            "schema",
            "runs",
            "runs/r",
            "saved-queries",
            "saved-queries/q",
            "saved-queries/run",
        ] {
            assert_eq!(
                request_deadline(&format!("{base}/{path}")),
                REQUEST_TIMEOUT,
                "{path}"
            );
        }
        assert_eq!(
            request_deadline("/api/v1/projects/p/traces"),
            REQUEST_TIMEOUT
        );
        // The store's own limits end first: two validations of 15 s and a run of 35 s.
        assert!(SQL_REQUEST_TIMEOUT > Duration::from_secs(15 + 15 + 35));
    }
}
