//! Crucible's own MCP server (contract 12.1): read-only tools for coding agents over the query services, at
//! `/mcp` on the main listener.
//!
//! - **Transport.** rmcp's streamable HTTP service with legacy session mode on, so both protocol eras are
//!   served: a client of a version before 2026-07-28 opens a session with `initialize` and sends its
//!   `Mcp-Session-Id`, and a 2026-07-28 client sends self-contained requests that are served statelessly. A
//!   request-response tool call of the stateless era is answered with plain JSON. The `Host` check of rmcp is
//!   off: every request carries a key, and the defaults must work on an internal network (contract 1.7).
//! - **Auth.** Every request carries a project's secret key as `Authorization: Bearer`, or the key pair as
//!   Basic (contract 12.2). It is verified by the [`KeyVerifier`] before rmcp reads the request; a refusal is a
//!   401 problem that says how to send the key. The key's project is the only project a tool reads: no tool
//!   takes a project argument, and the scope comes from each request's own credential, never from a session.
//! - **Reads** run as a `ReadContext` of caller class `agent` (contract 8.4), with the project's retention and
//!   maximum window from the [`ProjectSettingsSource`].
//! - **Results** carry previews only (the first 1,024 bytes of an input or output, never a payload), stay under
//!   a hard cap of [`MAX_RESULT_BYTES`] for the whole tool result, and continue with a signed cursor when they
//!   are cut; each tool also caps its rows.
//! - **Errors** a caller can act on are tool results with `isError` whose text is a problem document with the
//!   stable code of contract 12.3 and what to change. An unknown tool is a JSON-RPC error.
//! - **Rate limit.** Tool calls are limited per key on the `Env` clock (GCRA), by default 50 calls per second
//!   with bursts of 200 (contract 11.9); [`McpConfig`] changes or disables it.
//!
//! Each tool is one module of `tools`, registered there with its `mod` line and one entry in the tool list
//! (docs/architecture/adding-an-endpoint.md).

mod auth;
mod cursor;
mod error;
mod mcp;
mod rate;
mod server;
mod shape;
mod tools;

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::any;
use crucible_api::ProjectSettingsSource;
use crucible_auth::KeyVerifier;
use crucible_cursor::KeyRing;
use crucible_env::Env;
use crucible_export::Exports;
use crucible_query::QueryService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::auth::AuthState;
use crate::server::McpServer;

pub use shape::MAX_RESULT_BYTES;

/// The path of the MCP server on the main listener (contract 12.1).
pub const PATH: &str = "/mcp";

/// The settings of the MCP server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct McpConfig {
    /// Tool calls one key may make per second, sustained. 0 turns the limit off.
    pub calls_per_second: u32,
    /// Tool calls one key may make at once after a quiet spell.
    pub burst: u32,
}

impl Default for McpConfig {
    /// The API's read limit per key of contract 11.9: 50 calls per second, bursts of 200.
    fn default() -> Self {
        Self {
            calls_per_second: 50,
            burst: 200,
        }
    }
}

/// The MCP server over a query service at [`PATH`], with cursors signed by `cursors`. Merge it into the main
/// listener's router, inside the request-id layer. Its sessions and streams end when the `Env`'s shutdown
/// signal resolves, so they do not hold up a graceful shutdown.
pub fn router(
    env: &Env,
    service: QueryService,
    keys: Arc<dyn KeyVerifier>,
    settings: Arc<dyn ProjectSettingsSource>,
    cursors: Arc<KeyRing>,
    config: McpConfig,
    exports: Option<Exports>,
) -> Router {
    let server = McpServer::new(env, service, settings, cursors, config, exports);
    let transport = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(true)
        .with_json_response(true)
        .with_max_request_body_bytes(crucible_api::BODY_LIMIT)
        .disable_allowed_hosts();
    let stop = transport.cancellation_token.clone();
    let shutdown = env.shutdown();
    // Detached: the task ends with the shutdown signal, and its handle is not needed to wait for that.
    let _ = env.spawn("mcp-shutdown", async move {
        shutdown.await;
        stop.cancel();
    });
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        transport,
    );
    authenticated(env, keys, Router::new().route_service(PATH, service))
}

/// The MCP server before a telemetry store is up: requests are authenticated as usual, then answered with a
/// 503 `telemetry_store_unavailable` problem that says which setting provides the store.
pub fn router_without_store(env: &Env, keys: Arc<dyn KeyVerifier>) -> Router {
    let state = env.clone();
    let unavailable = any(move |request: axum::extract::Request| {
        let env = state.clone();
        async move { auth::store_unavailable(&env, &request) }
    });
    authenticated(env, keys, Router::new().route(PATH, unavailable))
}

fn authenticated(env: &Env, keys: Arc<dyn KeyVerifier>, router: Router) -> Router {
    let state = AuthState {
        env: env.clone(),
        keys,
    };
    router.layer(middleware::from_fn_with_state(state, auth::authenticate))
}
