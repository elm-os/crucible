//! The API through tower's `oneshot`, over a query service whose store is crucible-query's in-memory store and
//! whose recent index is a real one fed by a model of stage B (contract 8.6 to 8.8, 11.6, 12.1 to 12.4).

// Test code throughout: lets the helpers use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

// The store and the node model of crucible-query's service tests, shared rather than repeated.
#[allow(dead_code)]
#[path = "../../../crucible-query/tests/services/store.rs"]
mod store;
#[allow(dead_code)]
#[path = "../../../crucible-query/tests/services/world.rs"]
mod world;

mod alerts;
mod document;
mod endpoints;
mod errors;
mod facts;
mod identity;
mod metrics;
mod payload;
mod search;
mod sql;

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use crucible_api::{ProjectSettingsSource, StaticProjectSettings};
use crucible_auth::{KeyVerifier, PublicKey, SecretKey, StaticKey, StaticKeys};
use crucible_cursor::{KeyRing, Kid, SigningKey};
use crucible_env::Env;
use crucible_records::{ApiKeyId, Timestamp};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

use crate::world::{World, project};

/// The secret key of project 1.
pub(crate) fn key() -> String {
    format!("csk_{}", "1".repeat(43))
}

/// The secret key of project 2.
pub(crate) fn other_key() -> String {
    format!("csk_{}", "2".repeat(43))
}

/// The path of a project-scoped route of project `n`.
pub(crate) fn path(n: u8, rest: &str) -> String {
    format!("/api/v1/projects/{}{rest}", project(n))
}

/// `from` and `to` as query parameters.
pub(crate) fn window(from: Timestamp, to: Timestamp) -> String {
    format!("from={from}&to={to}")
}

fn keys(env: &Env) -> Arc<dyn KeyVerifier> {
    let key = |n: u8, secret: String| StaticKey {
        id: ApiKeyId::from_bytes([n; 16]),
        project_id: project(n),
        public_key: PublicKey::parse(&secret.replace("csk_", "cpk_")).unwrap(),
        secret: SecretKey::parse(&secret).unwrap(),
    };
    let keys = vec![key(1, self::key()), key(2, other_key())];
    Arc::new(StaticKeys::new(env.rng.as_ref(), keys).unwrap())
}

fn settings() -> Arc<dyn ProjectSettingsSource> {
    Arc::new(StaticProjectSettings::default())
}

/// The node's cursor-signing key, `test`.
pub(crate) fn cursor_keys() -> Arc<KeyRing> {
    let key = SigningKey::new(Kid::new("test").unwrap(), [9; 32]);
    Arc::new(KeyRing::new(key))
}

/// The API over the world's query service, writing facts into the world's store.
pub(crate) fn api(env: &Env, world: &World) -> Router {
    api_writing(env, world, facts::FakeWriter::new(world))
}

/// The API over the world's query service with this fact writer.
pub(crate) fn api_writing(env: &Env, world: &World, writer: Arc<facts::FakeWriter>) -> Router {
    let service = world.service.clone();
    crucible_api::router(env, service, keys(env), settings(), cursor_keys(), writer)
}

/// The API before a telemetry store is configured.
pub(crate) fn api_without_store(env: &Env) -> Router {
    crucible_api::router_without_store(env, keys(env), settings(), cursor_keys())
}

/// A response, its body parsed as JSON (`Null` when it is not JSON).
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Value,
}

impl Reply {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    /// Asserts a problem document with this status and code whose request id is the one in `X-Request-Id`.
    pub(crate) fn assert_problem(&self, status: u16, code: &str) {
        assert_eq!(self.status.as_u16(), status, "{:#}", self.body);
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json")
        );
        assert_eq!(self.body["code"], code, "{:#}", self.body);
        assert_eq!(self.body["status"], status);
        assert_eq!(self.body["type"], format!("urn:crucible:problem:{code}"));
        let request_id = self.body["request_id"].as_str().unwrap();
        assert_eq!(self.header("x-request-id"), Some(request_id));
    }

    /// The pointer and code of each field error.
    pub(crate) fn field_errors(&self) -> Vec<(&str, &str)> {
        self.body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["pointer"].as_str().unwrap(), e["code"].as_str().unwrap()))
            .collect()
    }

    pub(crate) fn pretty(&self) -> String {
        serde_json::to_string_pretty(&self.body).unwrap()
    }
}

/// One request through the router, with `key` as the Bearer token when given.
pub(crate) async fn send(router: &Router, method: Method, uri: &str, key: Option<&str>) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        request = request.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// A GET with project 1's key.
pub(crate) async fn get(router: &Router, uri: &str) -> Reply {
    send(router, Method::GET, uri, Some(&key())).await
}

/// A POST of a JSON body with project 1's key and the extra headers.
pub(crate) async fn post(
    router: &Router,
    uri: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Reply {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {}", key()))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// Runs `body` with the `Env` of a simulated host: a paused virtual clock and seeded streams, so request ids
/// are deterministic and the request timeout fires as soon as a request is stuck.
pub(crate) fn run<F, Fut>(body: F)
where
    F: FnOnce(Env) -> Fut,
    Fut: Future<Output = ()>,
{
    crucible_sim::run(7, |sim| async move {
        let disk = sim.disk("disk");
        let host = sim.host("api/boot-1", &disk, 0);
        body(host.env.clone()).await;
        drop(host);
    });
}
