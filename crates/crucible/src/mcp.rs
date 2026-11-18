//! The MCP server on the main listener (contract 12.1). `/mcp` serves `crucible-mcp-server` over the query service
//! once the telemetry store is up; until then, and always in the ingest-only mode, it authenticates each request
//! and answers 503 `telemetry_store_unavailable` with the setting that provides a store. Paths below `/mcp/` stay
//! the UI's, whose pages there still resolve.

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use crucible_api::StaticProjectSettings;
use crucible_auth::KeyVerifier;
use crucible_cursor::KeyRing;
use crucible_env::Env;
use crucible_mcp_server::McpConfig;
use crucible_query::QueryService;
use tower::ServiceExt as _;

/// Which MCP server the main listener serves.
pub(crate) struct McpGate {
    /// The server over the store, set once the store is up.
    over_store: OnceLock<Router>,
    /// The server until then.
    without_store: Router,
}

impl McpGate {
    pub(crate) fn new(env: &Env, keys: Arc<dyn KeyVerifier>) -> Self {
        Self {
            over_store: OnceLock::new(),
            without_store: crucible_mcp_server::router_without_store(env, keys),
        }
    }

    /// Mounts the MCP server over the store's query service, with the keys and cursor keys of the API.
    pub(crate) fn open(
        &self,
        env: &Env,
        service: QueryService,
        keys: Arc<dyn KeyVerifier>,
        cursors: Arc<KeyRing>,
    ) {
        let settings = Arc::new(StaticProjectSettings::default());
        let router = crucible_mcp_server::router(
            env,
            service,
            keys,
            settings,
            cursors,
            McpConfig::default(),
        );
        // The store task is the only caller, once.
        let _ = self.over_store.set(router);
    }
}

/// Hands a request for `/mcp` to the MCP server of the moment.
pub(crate) async fn serve(State(gate): State<Arc<McpGate>>, request: Request) -> Response {
    let router = gate.over_store.get().unwrap_or(&gate.without_store).clone();
    match router.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// An rmcp client transport that sends its HTTP requests through a router in process, so tests drive the real
/// mount with the real client of both protocol eras.
#[cfg(test)]
#[allow(clippy::disallowed_types)] // rmcp's client trait passes its headers in a std `HashMap`.
pub(crate) mod client {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
    use futures::StreamExt as _;
    use futures::stream::BoxStream;
    use http_body_util::BodyExt as _;
    use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
    use rmcp::transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    };
    use sse_stream::{Sse, SseStream};
    use tower::ServiceExt as _;

    /// The client of one router.
    #[derive(Clone, Debug)]
    pub(crate) struct RouterClient(pub(crate) Router);

    /// A request that could not be built or read.
    #[derive(Debug)]
    pub(crate) struct RouterError(String);

    impl std::fmt::Display for RouterError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::error::Error for RouterError {}

    type Failure = StreamableHttpError<RouterError>;

    fn failure(message: impl Into<String>) -> Failure {
        StreamableHttpError::Client(RouterError(message.into()))
    }

    impl RouterClient {
        async fn send(
            &self,
            method: Method,
            uri: &str,
            session_id: Option<&str>,
            auth_header: Option<String>,
            headers: HashMap<HeaderName, HeaderValue>,
            body: Body,
        ) -> Result<axum::http::Response<Body>, Failure> {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json");
            if let Some(session_id) = session_id {
                request = request.header("mcp-session-id", session_id);
            }
            if let Some(token) = auth_header {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let request = request.body(body).map_err(|e| failure(e.to_string()))?;
            let response = self.0.clone().oneshot(request).await;
            response.map_err(|never| match never {})
        }
    }

    fn events(body: Body) -> BoxStream<'static, Result<Sse, SseError>> {
        SseStream::new(body).boxed()
    }

    fn header(response: &axum::http::Response<Body>, name: &str) -> Option<String> {
        let value = response.headers().get(name)?;
        value.to_str().ok().map(str::to_owned)
    }

    impl StreamableHttpClient for RouterClient {
        type Error = RouterError;

        async fn post_message(
            &self,
            uri: Arc<str>,
            message: ClientJsonRpcMessage,
            session_id: Option<Arc<str>>,
            auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<StreamableHttpPostResponse, Failure> {
            let body = serde_json::to_vec(&message).map_err(|e| failure(e.to_string()))?;
            let response = self
                .send(
                    Method::POST,
                    &uri,
                    session_id.as_deref(),
                    auth_header,
                    custom_headers,
                    Body::from(body),
                )
                .await?;
            let session = header(&response, "mcp-session-id");
            match response.status() {
                StatusCode::ACCEPTED => return Ok(StreamableHttpPostResponse::Accepted),
                StatusCode::OK => {}
                status => {
                    let body = response.into_body().collect().await;
                    let text = body.map(|b| b.to_bytes()).unwrap_or_default();
                    let text = String::from_utf8_lossy(&text).into_owned();
                    return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                        format!("{status}: {text}"),
                    )));
                }
            }
            let content_type = header(&response, "content-type").unwrap_or_default();
            if content_type.starts_with("text/event-stream") {
                return Ok(StreamableHttpPostResponse::Sse(
                    events(response.into_body()),
                    session,
                ));
            }
            let bytes = response.into_body().collect().await;
            let bytes = bytes.map_err(|e| failure(e.to_string()))?.to_bytes();
            let message: ServerJsonRpcMessage = serde_json::from_slice(&bytes)?;
            Ok(StreamableHttpPostResponse::Json(message, session))
        }

        async fn delete_session(
            &self,
            uri: Arc<str>,
            session_id: Arc<str>,
            auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<(), Failure> {
            let response = self
                .send(
                    Method::DELETE,
                    &uri,
                    Some(&session_id),
                    auth_header,
                    custom_headers,
                    Body::empty(),
                )
                .await?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(StreamableHttpError::ServerDoesNotSupportDeleteSession)
            }
        }

        async fn get_stream(
            &self,
            uri: Arc<str>,
            session_id: Option<Arc<str>>,
            last_event_id: Option<String>,
            auth_header: Option<String>,
            mut custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<BoxStream<'static, Result<Sse, SseError>>, Failure> {
            if let Some(id) = last_event_id.and_then(|id| HeaderValue::from_str(&id).ok()) {
                custom_headers.insert(HeaderName::from_static("last-event-id"), id);
            }
            let response = self
                .send(
                    Method::GET,
                    &uri,
                    session_id.as_deref(),
                    auth_header,
                    custom_headers,
                    Body::empty(),
                )
                .await?;
            if response.status() == StatusCode::OK {
                Ok(events(response.into_body()))
            } else {
                Err(StreamableHttpError::ServerDoesNotSupportSse)
            }
        }
    }
}
