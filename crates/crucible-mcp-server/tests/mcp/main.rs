//! The MCP server through tower's `oneshot`, speaking JSON-RPC at the wire level in both protocol eras, over a
//! query service whose store is crucible-query's in-memory store and whose recent index is a real one fed by a
//! model of stage B.

// Test code throughout: lets the helpers use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

// The store and the node model of crucible-query's service tests, shared rather than repeated.
#[allow(dead_code)]
#[path = "../../../crucible-query/tests/services/store.rs"]
mod store;
#[allow(dead_code)]
#[path = "../../../crucible-query/tests/services/world.rs"]
mod world;

mod auth;
mod caps;
mod eras;
mod exports;
mod text;
mod tools;

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use crucible_api::StaticProjectSettings;
use crucible_auth::{KeyVerifier, PublicKey, SecretKey, StaticKey, StaticKeys};
use crucible_cursor::{KeyRing, Kid, SigningKey};
use crucible_env::Env;
use crucible_mcp_server::{McpConfig, PATH};
use crucible_records::{ApiKeyId, Timestamp};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::world::{World, project};

/// The protocol version of the stateless era.
pub(crate) const MODERN: &str = "2026-07-28";
/// A protocol version of the session era.
pub(crate) const LEGACY: &str = "2025-06-18";

/// The secret key of project `n`.
pub(crate) fn key(n: u8) -> String {
    format!("csk_{}", n.to_string().repeat(43))
}

/// The public key of project `n`.
pub(crate) fn public_key(n: u8) -> String {
    format!("cpk_{}", n.to_string().repeat(43))
}

fn keys(env: &Env) -> Arc<dyn KeyVerifier> {
    let static_key = |n: u8| StaticKey {
        id: ApiKeyId::from_bytes([n; 16]),
        project_id: project(n),
        public_key: PublicKey::parse(&public_key(n)).unwrap(),
        secret: SecretKey::parse(&key(n)).unwrap(),
    };
    let keys = vec![static_key(1), static_key(2)];
    Arc::new(StaticKeys::new(env.rng.as_ref(), keys).unwrap())
}

fn cursor_keys() -> Arc<KeyRing> {
    Arc::new(KeyRing::new(SigningKey::new(
        Kid::new("test").unwrap(),
        [9; 32],
    )))
}

/// The MCP server over the world's query service.
pub(crate) fn mcp(env: &Env, world: &World) -> Router {
    mcp_with(env, world, McpConfig::default())
}

pub(crate) fn mcp_with(env: &Env, world: &World, config: McpConfig) -> Router {
    let settings = Arc::new(StaticProjectSettings::default());
    let service = world.service.clone();
    crucible_mcp_server::router(env, service, keys(env), settings, cursor_keys(), config, None)
}

/// The MCP server over the world's query service, with the export tools over `exports`.
pub(crate) fn mcp_exporting(env: &Env, world: &World, exports: crucible_export::Exports) -> Router {
    let settings = Arc::new(StaticProjectSettings::default());
    let service = world.service.clone();
    let config = McpConfig::default();
    crucible_mcp_server::router(env, service, keys(env), settings, cursor_keys(), config, Some(exports))
}

/// The MCP server before a telemetry store is up.
pub(crate) fn mcp_without_store(env: &Env) -> Router {
    crucible_mcp_server::router_without_store(env, keys(env))
}

/// `from` and `to` as tool arguments.
pub(crate) fn window(from: Timestamp, to: Timestamp) -> Value {
    json!({ "from": from.to_string(), "to": to.to_string() })
}

/// An answer of the MCP path.
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    /// The JSON body, or the first JSON-RPC response of an event stream, or `Null`.
    pub(crate) body: Value,
}

impl Reply {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    /// Asserts a problem document with this status and code.
    pub(crate) fn assert_problem(&self, status: u16, code: &str) {
        assert_eq!(self.status.as_u16(), status, "{:#}", self.body);
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json")
        );
        assert_eq!(self.body["code"], code, "{:#}", self.body);
        assert_eq!(self.body["status"], status);
    }
}

/// The JSON-RPC messages of an event stream body, in order.
fn events(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.split("\n\n").filter_map(|event| {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        serde_json::from_str(&data.join("\n")).ok()
    })
}

/// Reads a JSON body whole, and an event stream up to its first JSON-RPC response: a session's stream stays
/// open after it.
async fn read(body: Body, stream: bool) -> Value {
    let mut body = body;
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            bytes.extend_from_slice(&data);
        }
        if stream {
            let text = String::from_utf8_lossy(&bytes);
            let response = events(&text)
                .find(|message| message.get("result").is_some() || message.get("error").is_some());
            if let Some(response) = response {
                return response;
            }
        }
    }
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// One request to the MCP path with these headers, sending `body` as JSON when there is one.
pub(crate) async fn send(
    router: &Router,
    method: Method,
    headers: &[(&str, String)],
    body: Option<&Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(PATH)
        .header("accept", "application/json, text/event-stream")
        .header("host", "crucible.internal:8080");
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let stream = parts
        .headers
        .get("content-type")
        .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"));
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: read(body, stream).await,
    }
}

/// A client of one protocol era with one project's key, speaking JSON-RPC to the MCP path.
pub(crate) struct Client {
    router: Router,
    pub(crate) authorization: String,
    /// The session of a session-era client.
    pub(crate) session: Option<String>,
    modern: bool,
    next_id: u64,
}

impl Client {
    /// A client of the session era: `initialize` opens a session whose id every later request carries.
    pub(crate) async fn legacy(router: &Router, key: &str) -> Self {
        let mut client = Self {
            router: router.clone(),
            authorization: format!("Bearer {key}"),
            session: None,
            modern: false,
            next_id: 1,
        };
        let params = json!({
            "protocolVersion": LEGACY,
            "capabilities": {},
            "clientInfo": { "name": "crucible-test", "version": "1.0.0" },
        });
        let reply = client.request("initialize", params).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        assert_eq!(reply.body["result"]["protocolVersion"], LEGACY);
        client.session = Some(reply.header("mcp-session-id").unwrap().to_owned());
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let reply = send(
            &client.router,
            Method::POST,
            &client.headers("notifications/initialized", None),
            Some(&initialized),
        )
        .await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        client
    }

    /// A client of the stateless era: every request carries its own protocol metadata.
    pub(crate) fn modern(router: &Router, key: &str) -> Self {
        Self {
            router: router.clone(),
            authorization: format!("Bearer {key}"),
            session: None,
            modern: true,
            next_id: 1,
        }
    }

    fn headers(&self, method: &str, name: Option<&str>) -> Vec<(&'static str, String)> {
        let mut headers = vec![("authorization", self.authorization.clone())];
        if let Some(session) = &self.session {
            headers.push(("mcp-session-id", session.clone()));
            headers.push(("mcp-protocol-version", LEGACY.to_owned()));
        }
        if self.modern {
            headers.push(("mcp-protocol-version", MODERN.to_owned()));
            headers.push(("mcp-method", method.to_owned()));
            if let Some(name) = name {
                headers.push(("mcp-name", name.to_owned()));
            }
        }
        headers
    }

    /// One JSON-RPC request.
    pub(crate) async fn request(&mut self, method: &str, mut params: Value) -> Reply {
        if self.modern {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientInfo": { "name": "crucible-test", "version": "1.0.0" },
                "io.modelcontextprotocol/clientCapabilities": {},
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let name = params["name"].as_str();
        let headers = self.headers(method, name);
        let reply = send(&self.router, Method::POST, &headers, Some(&body)).await;
        if reply.status == StatusCode::OK {
            assert_eq!(reply.body["id"], id, "{:#}", reply.body);
        }
        reply
    }

    /// The result of a `tools/call`.
    pub(crate) async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let params = json!({ "name": tool, "arguments": arguments });
        let reply = self.request("tools/call", params).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        reply.body["result"].clone()
    }

    /// The structured output of a successful tool call, checked against its text copy and the size cap.
    pub(crate) async fn output(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments).await;
        assert_eq!(result["isError"], false, "{result:#}");
        let bytes = serde_json::to_vec(&result).unwrap().len();
        assert!(
            bytes <= crucible_mcp_server::MAX_RESULT_BYTES,
            "{bytes} bytes"
        );
        let output = result["structuredContent"].clone();
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, output);
        output
    }

    /// The problem document of a failed tool call.
    pub(crate) async fn error(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments).await;
        assert_eq!(result["isError"], true, "{result:#}");
        assert!(result.get("structuredContent").is_none(), "{result:#}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}

/// Runs `body` with the `Env` of a simulated host: a paused virtual clock and seeded streams.
pub(crate) fn run<F, Fut>(body: F)
where
    F: FnOnce(Env) -> Fut,
    Fut: Future<Output = ()>,
{
    crucible_sim::run(7, |sim| async move {
        let disk = sim.disk("disk");
        let host = sim.host("mcp/boot-1", &disk, 0);
        body(host.env.clone()).await;
        drop(host);
    });
}
