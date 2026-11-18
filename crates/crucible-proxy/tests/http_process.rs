//! The HTTP proxy against real sockets (invariant I11, process tests): a scripted Streamable HTTP server, the
//! production `Env`, and a collector in each of its states.
//!
//! The same requests are sent to the server directly and through the proxy. The client must see the same
//! status, headers and body bytes, and the server the same method, path, headers and body bytes, with the
//! `Authorization` header untouched. SSE events arrive one by one, a client that goes away aborts the upstream
//! request, and the bearer token appears in nothing that is exported.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use common::{
    SINKS, Sink, Sinked, by_id, check_wire_contract, int, loopback, prod_env, start_sink, text,
};
use crucible_env::{Env, JoinHandle};
use crucible_proxy::http::{HostHeader, HttpProxyConfig, serve};
use crucible_proxy::identity::{IdentityResolver, VerifyConfig};
use crucible_proxy::net::Target;
use crucible_records::{CaptureSource, IdentitySource};
use futures::StreamExt as _;
use futures::channel::mpsc;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value as Json, json};
use tokio::sync::Notify;

const RAW_SESSION: &str = "raw-session-5f1c";
const SESSION_HASH_LEN: usize = 32;
const PLANTED_ARGUMENT: &str = "s3cret-argument-value";

/// One request as the upstream received it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// A request that was never answered was dropped: its connection closed.
    aborted: Arc<AtomicBool>,
    /// Lets a gated event stream send its second event.
    gate: Arc<Notify>,
    _accept: JoinHandle<()>,
}

struct OnDrop(Arc<AtomicBool>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

type ServerBody = BoxBody<Bytes, Infallible>;

fn full(bytes: impl Into<Bytes>) -> ServerBody {
    Full::new(bytes.into()).boxed()
}

fn sorted_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn event(message: &Json) -> Bytes {
    Bytes::from(format!("event: message\ndata: {message}\n\n"))
}

async fn answer(
    env: Env,
    seen: Arc<Mutex<Vec<Seen>>>,
    aborted: Arc<AtomicBool>,
    gate: Arc<Notify>,
    request: Request<Incoming>,
) -> Result<Response<ServerBody>, Infallible> {
    let (parts, body) = request.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.to_string(),
        headers: sorted_headers(&parts.headers),
        body: body.to_vec(),
    });
    let json = |status: StatusCode, value: &Json| {
        Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json")
            .header("x-upstream", "scripted")
            .body(full(value.to_string()))
            .unwrap()
    };
    if parts.method == Method::GET {
        return Ok(match parts.uri.path() {
            "/.well-known/oauth-protected-resource" => json(
                StatusCode::OK,
                &json!({"resource": "https://mcp.example.test", "authorization_servers": ["https://issuer.test"]}),
            ),
            "/jwks.json" => json(
                StatusCode::OK,
                &json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "kid": "k1",
                    "x": "Gb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE"}]}),
            ),
            _ => json(StatusCode::NOT_FOUND, &json!({"error": "not found"})),
        });
    }
    let message: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
    let id = message.get("id").cloned();
    let rpc = message.get("method").and_then(Json::as_str).unwrap_or("");
    let name = message
        .pointer("/params/name")
        .and_then(Json::as_str)
        .unwrap_or("");
    let Some(id) = id else {
        // A notification.
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(full(Bytes::new()))
            .unwrap());
    };
    let result = |result: Json| json!({"jsonrpc": "2.0", "id": id, "result": result});
    Ok(match (rpc, name) {
        ("initialize", _) => {
            let mut response = json(
                StatusCode::OK,
                &result(json!({"protocolVersion": "2025-06-18", "capabilities": {},
                    "serverInfo": {"name": "scripted-http", "version": "3.1"}})),
            );
            response
                .headers_mut()
                .insert("mcp-session-id", HeaderValue::from_static(RAW_SESSION));
            response
        }
        (_, "auth") => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                "www-authenticate",
                "Bearer resource_metadata=\"https://mcp.example.test/.well-known/oauth-protected-resource\"",
            )
            .header(CONTENT_TYPE, "application/json")
            .body(full(r#"{"error":"invalid_token"}"#))
            .unwrap(),
        (_, "slow") => {
            let _dropped = OnDrop(aborted);
            futures::future::pending::<Response<ServerBody>>().await
        }
        (_, "stream") => {
            let gated = parts.headers.contains_key("x-gate");
            let (events, receiver) = mpsc::unbounded::<Bytes>();
            let progress = |n: u64| {
                event(&json!({"jsonrpc": "2.0", "method": "notifications/progress",
                    "params": {"progressToken": "tok-1", "progress": n}}))
            };
            let last = event(&result(json!({"content": [{"type": "text", "text": "done"}]})));
            drop(env.spawn("upstream-events", {
                let env = env.clone();
                async move {
                    let _sent = events.unbounded_send(Bytes::from_static(b": keep-alive\n\n"));
                    let _sent = events.unbounded_send(progress(1));
                    if gated {
                        gate.notified().await;
                    } else {
                        env.clock.sleep(Duration::from_millis(20)).await;
                    }
                    let _sent = events.unbounded_send(progress(2));
                    env.clock.sleep(Duration::from_millis(5)).await;
                    let _sent = events.unbounded_send(last);
                    events.close_channel();
                }
            }));
            let stream = receiver.map(|bytes| Ok::<_, Infallible>(Frame::data(bytes)));
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/event-stream")
                .header("cache-control", "no-cache")
                .body(http_body_util::BodyExt::boxed(StreamBody::new(stream)))
                .unwrap()
        }
        ("tools/call", "fail") => json(
            StatusCode::OK,
            &result(json!({"content": [], "isError": true})),
        ),
        ("nope", _) => json(
            StatusCode::OK,
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}}),
        ),
        _ => json(
            StatusCode::OK,
            &result(json!({"content": [{"type": "text", "text": format!("{} bytes", body.len())}],
                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "scripted-http", "version": "3.1"}}})),
        ),
    })
}

async fn upstream(env: &Env) -> Upstream {
    let mut listener = env.net.listen(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let aborted = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(Notify::new());
    let accept = env.spawn("upstream", {
        let (env, seen, aborted, gate) = (env.clone(), seen.clone(), aborted.clone(), gate.clone());
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (env2, seen, aborted, gate) =
                    (env.clone(), seen.clone(), aborted.clone(), gate.clone());
                drop(env.spawn("upstream-connection", async move {
                    let service = service_fn(move |request| {
                        answer(
                            env2.clone(),
                            seen.clone(),
                            aborted.clone(),
                            gate.clone(),
                            request,
                        )
                    });
                    let _closed = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                }));
            }
        }
    });
    Upstream {
        addr,
        seen,
        aborted,
        gate,
        _accept: accept,
    }
}

struct Client {
    sender: SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// What the client received: status, headers (without `date`, which is the time of the answer) and body.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Client {
    async fn connect(env: &Env, addr: SocketAddr) -> Self {
        let deadline = env.clock.now().saturating_add(Duration::from_secs(5));
        let stream = env.net.connect(addr, deadline).await.unwrap();
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let driver = env.spawn("test-client", async move {
            let _closed = connection.await;
        });
        Self { sender, driver }
    }

    async fn send(&mut self, request: Request<Full<Bytes>>) -> Response<Incoming> {
        self.sender.ready().await.unwrap();
        self.sender.send_request(request).await.unwrap()
    }

    async fn exchange(&mut self, request: Request<Full<Bytes>>) -> Answer {
        let response = self.send(request).await;
        let (parts, body) = response.into_parts();
        let mut headers = sorted_headers(&parts.headers);
        headers.retain(|(name, _)| name != "date");
        Answer {
            status: parts.status.as_u16(),
            headers,
            body: body.collect().await.unwrap().to_bytes().to_vec(),
        }
    }
}

/// A token shaped like a JWT. Its signature is not valid for anything.
fn unsigned_token() -> String {
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(
            r#"{"iss":"https://issuer.test","sub":"user-42","client_id":"client-7","tid":"acme","exp":4102444800}"#
        ),
        URL_SAFE_NO_PAD.encode("not-a-signature")
    )
}

fn post(token: &str, extra: &[(&str, &str)], body: &Json) -> Request<Full<Bytes>> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/mcp?x=1")
        .header(HOST, "mcp.example.test")
        .header(USER_AGENT, "mcp-test-client/1.0")
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "application/json, text/event-stream")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("cookie", "session=chocolate-chip")
        .header("mcp-param-secret", "param-header-value");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    request
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn modern_meta() -> Json {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "http-test", "version": "0.2"},
        "io.modelcontextprotocol/clientCapabilities": {},
        "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        "progressToken": "tok-1",
    })
}

fn call(id: u64, name: &str) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": name, "arguments": {"text": PLANTED_ARGUMENT}, "_meta": modern_meta()}})
}

/// A modern-era session: a JSON answer, an event stream, a tool error, a JSON-RPC error, a 401 challenge, a
/// notification and the well-known metadata.
fn requests(token: &str) -> Vec<Request<Full<Bytes>>> {
    let modern = [("mcp-protocol-version", "2026-07-28")];
    vec![
        post(token, &modern, &call(1, "echo")),
        post(token, &modern, &call(2, "stream")),
        post(token, &modern, &call(3, "auth")),
        post(token, &modern, &call(4, "fail")),
        post(
            token,
            &modern,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}),
        ),
        post(
            token,
            &modern,
            &json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
        ),
        Request::builder()
            .method(Method::GET)
            .uri("/.well-known/oauth-protected-resource")
            .header(HOST, "mcp.example.test")
            .body(Full::new(Bytes::new()))
            .unwrap(),
    ]
}

struct Proxy {
    addr: SocketAddr,
    sink: Sinked,
    task: JoinHandle<()>,
}

async fn proxy(
    env: &Env,
    upstream: SocketAddr,
    sink: Sink,
    identity: Arc<IdentityResolver>,
) -> Proxy {
    let sink = start_sink(env, sink, CaptureSource::ProxyHttp).await;
    let listener = env.net.listen(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = HttpProxyConfig {
        upstream: Target::parse(&format!("http://{upstream}")).unwrap(),
        host_header: HostHeader::Preserve,
        connect_timeout: Duration::from_secs(5),
        identity,
    };
    let task = env.spawn(
        "proxy-under-test",
        serve(env.clone(), listener, config, sink.telemetry.tap()),
    );
    Proxy { addr, sink, task }
}

async fn run_all(env: &Env, addr: SocketAddr, token: &str) -> Vec<Answer> {
    let mut client = Client::connect(env, addr).await;
    let mut answers = Vec::new();
    for request in requests(token) {
        answers.push(client.exchange(request).await);
    }
    answers
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_and_responses_pass_unchanged_in_every_collector_state() {
    let env = prod_env();
    let token = unsigned_token();
    let server = upstream(&env).await;
    let baseline = run_all(&env, server.addr, &token).await;
    let direct_seen = std::mem::take(&mut *server.seen.lock().unwrap());
    assert_eq!(baseline.len(), 7);
    assert_eq!(
        baseline
            .iter()
            .map(|answer| answer.status)
            .collect::<Vec<_>>(),
        [200, 200, 401, 200, 200, 202, 200]
    );
    assert!(
        String::from_utf8_lossy(&baseline[1].body).starts_with(": keep-alive\n\nevent: message\n")
    );

    for sink in SINKS {
        let started = env.clock.now();
        let proxy = proxy(&env, server.addr, sink, IdentityResolver::off(&env)).await;
        let answers = run_all(&env, proxy.addr, &token).await;
        // The client sees what the server sent: status, headers (the 401 challenge too) and body bytes.
        assert_eq!(answers, baseline, "{sink:?}");
        // The server sees what the client sent: `Authorization`, `Host` and every other header, and the body.
        let seen = std::mem::take(&mut *server.seen.lock().unwrap());
        assert_eq!(seen, direct_seen, "{sink:?}");
        assert!(
            seen[0]
                .headers
                .contains(&("authorization".to_owned(), format!("Bearer {token}")))
        );
        assert!(
            seen[0]
                .headers
                .contains(&("host".to_owned(), "mcp.example.test".to_owned()))
        );
        let elapsed = env.clock.now().saturating_duration_since(started);
        assert!(
            elapsed < Duration::from_secs(10),
            "{sink:?} took {elapsed:?}"
        );

        proxy.task.abort();
        let received = proxy.sink.finish().await;
        if sink != Sink::Healthy {
            assert!(received.spans.is_empty());
            continue;
        }
        let spans = &received.spans;
        check_wire_contract(spans, "proxy_http");
        // Five requests, the server's two progress notifications and the client's notification; the metadata
        // GET is no MCP operation.
        assert_eq!(
            spans.len(),
            8,
            "{:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let echo = by_id(spans, "1");
        assert_eq!(echo.name, "tools/call echo");
        assert_eq!(int(echo, "http.response.status_code"), Some(200));
        assert_eq!(
            text(echo, "crucible.mcp.transport"),
            Some("streamable_http")
        );
        assert_eq!(text(echo, "crucible.mcp.era"), Some("modern"));
        assert_eq!(text(echo, "mcp.protocol.version"), Some("2026-07-28"));
        assert_eq!(text(echo, "crucible.mcp.client.name"), Some("http-test"));
        assert_eq!(
            text(echo, "crucible.mcp.client.ip_prefix"),
            Some("127.0.0.0/24")
        );
        assert_eq!(
            text(echo, "user_agent.original"),
            Some("mcp-test-client/1.0")
        );
        assert_eq!(
            text(echo, "crucible.mcp.server.name"),
            Some("scripted-http")
        );
        assert_eq!(text(echo, "mcp.session.id"), None);
        assert_eq!(
            echo.trace_id.to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(
            echo.parent_span_id.map(|id| id.to_string()).as_deref(),
            Some("b7ad6b7169203331")
        );
        // Identity is off by default: no identity key at all.
        assert_eq!(text(echo, "crucible.mcp.identity.source"), None);
        assert_eq!(text(echo, "crucible.mcp.user.subject"), None);
        let stream = by_id(spans, "2");
        assert_eq!(int(stream, "crucible.mcp.progress.count"), Some(2));
        assert_eq!(text(stream, "crucible.mcp.result_type"), Some("complete"));
        assert_eq!(int(stream, "crucible.mcp.content.blocks.text"), Some(1));
        assert!(stream.end.micros() - stream.start.micros() >= 20_000);
        let refused = by_id(spans, "3");
        assert_eq!(int(refused, "http.response.status_code"), Some(401));
        assert_eq!(text(refused, "crucible.mcp.error.class"), Some("auth"));
        assert_eq!(text(refused, "error.type"), Some("401"));
        assert_eq!(
            text(by_id(spans, "4"), "crucible.mcp.error.class"),
            Some("tool_execution")
        );
        assert_eq!(
            text(by_id(spans, "5"), "rpc.response.status_code"),
            Some("-32601")
        );
        let notification = spans
            .iter()
            .find(|span| text(span, "mcp.method.name") == Some("notifications/roots/list_changed"))
            .unwrap();
        assert_eq!(int(notification, "http.response.status_code"), None);

        // Taint: the token, the cookie, the parameter header and the tool argument reach no export.
        for planted in [
            token.as_str(),
            "Bearer",
            "chocolate-chip",
            "param-header-value",
            PLANTED_ARGUMENT,
        ] {
            assert!(!received.contains(planted), "{planted} was exported");
        }
        assert!(
            received.contains("mcp-test-client/1.0"),
            "the search itself works"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_reach_the_client_as_they_are_sent() {
    let env = prod_env();
    let server = upstream(&env).await;
    let proxy = proxy(
        &env,
        server.addr,
        Sink::Stalled,
        IdentityResolver::off(&env),
    )
    .await;
    let mut client = Client::connect(&env, proxy.addr).await;
    let request = post("opaque", &[("x-gate", "1")], &call(9, "stream"));
    let mut body = client.send(request).await.into_body();
    // The first events arrive while the server still holds the rest back.
    let mut early = Vec::new();
    while !String::from_utf8_lossy(&early).contains("\"progress\":1") {
        let frame = env
            .timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("the first event arrives before the stream ends")
            .unwrap()
            .unwrap();
        early.extend_from_slice(frame.data_ref().unwrap());
    }
    assert!(early.starts_with(b": keep-alive\n\n"));
    assert!(!String::from_utf8_lossy(&early).contains("\"progress\":2"));
    server.gate.notify_one();
    let rest = body.collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&rest).contains("\"result\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_session_is_keyed_by_its_hashed_id() {
    let env = prod_env();
    let server = upstream(&env).await;
    let proxy = proxy(
        &env,
        server.addr,
        Sink::Healthy,
        IdentityResolver::off(&env),
    )
    .await;
    let mut client = Client::connect(&env, proxy.addr).await;
    let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "legacy-client", "version": "9"}}});
    let answer = client.exchange(post("opaque", &[], &initialize)).await;
    assert!(
        answer
            .headers
            .contains(&("mcp-session-id".to_owned(), RAW_SESSION.to_owned()))
    );
    let legacy = [
        ("mcp-session-id", RAW_SESSION),
        ("mcp-protocol-version", "2025-06-18"),
    ];
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    assert_eq!(
        client.exchange(post("opaque", &legacy, &list)).await.status,
        200
    );
    drop(client);
    proxy.task.abort();
    let received = proxy.sink.finish().await;
    check_wire_contract(&received.spans, "proxy_http");
    let (first, second) = (by_id(&received.spans, "1"), by_id(&received.spans, "2"));
    let key = text(second, "mcp.session.id").unwrap();
    assert_eq!(key.len(), SESSION_HASH_LEN);
    assert_eq!(
        key,
        crucible_proxy::http::session_key(RAW_SESSION.as_bytes())
    );
    // The initialize exchange gets the id its response assigned.
    assert_eq!(text(first, "mcp.session.id"), Some(key));
    assert_eq!(text(first, "crucible.mcp.era"), Some("legacy"));
    // What the client said in initialize is remembered for the session.
    assert_eq!(
        text(second, "crucible.mcp.client.name"),
        Some("legacy-client")
    );
    assert_eq!(text(second, "mcp.protocol.version"), Some("2025-06-18"));
    assert_eq!(
        text(second, "crucible.mcp.server.name"),
        Some("scripted-http")
    );
    assert!(
        !received.contains(RAW_SESSION),
        "the raw session id was exported"
    );
}

async fn until(env: &Env, what: &str, condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        env.clock.sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_goes_away_aborts_the_upstream_request() {
    let env = prod_env();
    let server = upstream(&env).await;
    let proxy = proxy(
        &env,
        server.addr,
        Sink::Healthy,
        IdentityResolver::off(&env),
    )
    .await;
    let mut client = Client::connect(&env, proxy.addr).await;
    client.sender.ready().await.unwrap();
    let pending = client
        .sender
        .send_request(post("opaque", &[], &call(7, "slow")));
    let waiting = env.spawn("waiting-client", async move {
        let _never = pending.await;
    });
    until(&env, "the upstream has the request", || {
        server.seen.lock().unwrap().len() == 1
    })
    .await;
    assert!(!server.aborted.load(Ordering::SeqCst));
    waiting.abort();
    drop(client);
    until(&env, "the upstream request is aborted", || {
        server.aborted.load(Ordering::SeqCst)
    })
    .await;
    proxy.task.abort();
    let received = proxy.sink.finish().await;
    check_wire_contract(&received.spans, "proxy_http");
    let cancelled = by_id(&received.spans, "7");
    assert_eq!(
        text(cancelled, "crucible.mcp.cancel.source"),
        Some("stream_close")
    );
    assert_eq!(
        text(cancelled, "crucible.mcp.error.class"),
        Some("cancelled")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_upstream_is_answered_with_502() {
    let env = prod_env();
    let closed = {
        let listener = env.net.listen(loopback()).await.unwrap();
        listener.local_addr().unwrap()
    };
    let proxy = proxy(&env, closed, Sink::Absent, IdentityResolver::off(&env)).await;
    let mut client = Client::connect(&env, proxy.addr).await;
    let answer = client.exchange(post("opaque", &[], &call(1, "echo"))).await;
    assert_eq!(answer.status, 502);
    assert!(String::from_utf8_lossy(&answer.body).contains("could not be reached"));
    // The connection is still usable.
    assert_eq!(
        client
            .exchange(post("opaque", &[], &call(2, "echo")))
            .await
            .status,
        502
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claims_mode_labels_the_identity_as_unverified() {
    let env = prod_env();
    let token = unsigned_token();
    let server = upstream(&env).await;
    let proxy = proxy(
        &env,
        server.addr,
        Sink::Healthy,
        IdentityResolver::claims(&env),
    )
    .await;
    let mut client = Client::connect(&env, proxy.addr).await;
    assert_eq!(
        client
            .exchange(post(&token, &[], &call(1, "echo")))
            .await
            .status,
        200
    );
    assert_eq!(
        client
            .exchange(post("opaque-token", &[], &call(2, "echo")))
            .await
            .status,
        200
    );
    drop(client);
    proxy.task.abort();
    let received = proxy.sink.finish().await;
    check_wire_contract(&received.spans, "proxy_http");
    let with_claims = by_id(&received.spans, "1");
    assert_eq!(
        text(with_claims, "crucible.mcp.identity.source"),
        Some("token_claims")
    );
    assert_eq!(
        text(with_claims, "crucible.mcp.user.issuer"),
        Some("https://issuer.test")
    );
    assert_eq!(
        text(with_claims, "crucible.mcp.user.subject"),
        Some("user-42")
    );
    assert_eq!(
        text(with_claims, "crucible.mcp.oauth.client_id"),
        Some("client-7")
    );
    assert_eq!(text(with_claims, "crucible.mcp.tenant"), Some("acme"));
    // An opaque token yields no identity.
    assert_eq!(
        text(by_id(&received.spans, "2"), "crucible.mcp.identity.source"),
        None
    );
    assert!(!received.contains(&token));
}

/// The Ed25519 private key of RFC 8410, section 10.3, as PKCS#8 DER. Its public key is the one the scripted
/// server publishes at `/jwks.json`.
const RFC8410_PRIVATE_KEY: &str =
    "MC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC";

fn signed_token(claims: &Json) -> String {
    let key =
        jsonwebtoken::EncodingKey::from_ed_der(&STANDARD.decode(RFC8410_PRIVATE_KEY).unwrap());
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
    header.kid = Some("k1".to_owned());
    jsonwebtoken::encode(&header, claims, &key).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_mode_takes_identity_only_from_tokens_that_verify() {
    let env = prod_env();
    let server = upstream(&env).await;
    let now = env.clock.wall_now().micros() / 1_000_000;
    let claims = |subject: &str, expires: i64| {
        json!({"iss": "https://issuer.test", "sub": subject, "aud": "https://mcp.example.test",
            "client_id": "client-7", "exp": expires})
    };
    let good = signed_token(&claims("user-good", now + 3_600));
    let expired = signed_token(&claims("user-late", now - 3_600));
    let other_audience = signed_token(
        &json!({"iss": "https://issuer.test", "sub": "user-aud", "aud": "https://other.test", "exp": now + 3_600}),
    );
    // The same header and claims as the good token, with a signature that does not belong to them.
    let forged = {
        let mut parts: Vec<&str> = good.split('.').collect();
        let other = signed_token(&claims("user-other", now + 3_600));
        let signature = other.split('.').nth(2).unwrap().to_owned();
        parts[2] = &signature;
        parts.join(".")
    };
    let resolver = IdentityResolver::verified(
        &env,
        VerifyConfig {
            jwks: Target::parse(&format!("http://{}/jwks.json", server.addr)).unwrap(),
            audience: Some("https://mcp.example.test".to_owned()),
            issuer: Some("https://issuer.test".to_owned()),
            refresh: Duration::from_secs(600),
        },
    );
    let bearer = |token: &str| HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    let probe = bearer(&good);
    until(&env, "the key set is fetched", || {
        resolver.resolve(Some(&probe)).source.is_some()
    })
    .await;
    server.seen.lock().unwrap().clear();

    let verified = resolver.resolve(Some(&bearer(&good)));
    assert_eq!(verified.source, Some(IdentitySource::TokenVerified));
    assert_eq!(verified.subject, "user-good");
    for (label, token) in [
        ("expired", &expired),
        ("wrong audience", &other_audience),
        ("forged", &forged),
        ("unsigned", &unsigned_token()),
    ] {
        assert_eq!(
            resolver.resolve(Some(&bearer(token))).source,
            None,
            "{label}"
        );
    }
    assert_eq!(resolver.resolve(None).source, None);

    let proxy = proxy(&env, server.addr, Sink::Healthy, resolver).await;
    let mut client = Client::connect(&env, proxy.addr).await;
    assert_eq!(
        client
            .exchange(post(&good, &[], &call(1, "echo")))
            .await
            .status,
        200
    );
    assert_eq!(
        client
            .exchange(post(&forged, &[], &call(2, "echo")))
            .await
            .status,
        200
    );
    // The forged token is still forwarded untouched: the proxy observes, the server decides.
    let seen = server.seen.lock().unwrap().clone();
    assert!(
        seen[1]
            .headers
            .contains(&("authorization".to_owned(), format!("Bearer {forged}")))
    );
    drop(client);
    proxy.task.abort();
    let received = proxy.sink.finish().await;
    check_wire_contract(&received.spans, "proxy_http");
    let span = by_id(&received.spans, "1");
    assert_eq!(
        text(span, "crucible.mcp.identity.source"),
        Some("token_verified")
    );
    assert_eq!(text(span, "crucible.mcp.user.subject"), Some("user-good"));
    assert_eq!(
        text(by_id(&received.spans, "2"), "crucible.mcp.identity.source"),
        None
    );
    assert!(!received.contains(&good) && !received.contains(&forged));
}
