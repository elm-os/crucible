//! MCP cases: the runner plays the steps against the reference server, over stdio or streamable HTTP, and
//! prints one `response` event per completed send.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HOST};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{RunSpec, server};

/// What came back for one message.
#[derive(Debug, Default)]
pub(crate) struct Response {
    pub(crate) http_status: Option<u16>,
    pub(crate) session_id: Option<String>,
    pub(crate) message: Option<Value>,
    pub(crate) notifications: u32,
}

/// One message sent to the reference server; for a request, its response once it arrives.
struct Exchange {
    done: oneshot::Receiver<Result<Response, String>>,
    /// Aborts an HTTP exchange, which closes its connection.
    abort: Option<AbortHandle>,
}

enum Peer {
    Stdio(StdioPeer),
    Http(HttpPeer),
}

pub(crate) async fn run(spec: &RunSpec, raw: Value) -> Result<(), String> {
    let mut peer = match spec.transport.as_deref() {
        Some("stdio") => Peer::Stdio(StdioPeer::start(raw)?),
        Some("streamable_http") => Peer::Http(HttpPeer::start(spec).await?),
        other => return Err(format!("unknown transport {other:?}")),
    };
    let mut responses: BTreeMap<String, Value> = BTreeMap::new();
    let mut pending: BTreeMap<String, Exchange> = BTreeMap::new();
    for step in &spec.steps {
        let fail = |e: String| format!("step {}: {e}", step.step);
        if let Some(send) = &step.send {
            let message = resolve(send, &responses).map_err(fail)?;
            let id = message.get("id").map(Value::to_string);
            let body = message.to_string().into_bytes();
            let exchange = match &mut peer {
                Peer::Stdio(p) => p.send(id, body).await.map_err(fail)?,
                Peer::Http(p) => p.send(id, body, &step.headers),
            };
            if step.wait == Some(false) {
                pending.insert(step.step.clone(), exchange);
                continue;
            }
            finish(&step.step, exchange, &mut responses).await?;
        } else if let Some(target) = &step.await_step {
            let exchange = pending
                .remove(target)
                .ok_or_else(|| fail(format!("{target} is not in flight")))?;
            finish(target, exchange, &mut responses).await?;
        } else if let Some(ms) = step.sleep_ms {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        } else if let Some(target) = &step.close {
            let abort = pending
                .remove(target)
                .and_then(|e| e.abort)
                .ok_or_else(|| fail(format!("{target} is not an HTTP exchange in flight")))?;
            abort.abort();
        } else {
            return Err(fail("not an MCP step".to_owned()));
        }
    }
    // A request a later step cancelled is never awaited; an HTTP exchange still open is abandoned.
    for exchange in pending.into_values() {
        if let Some(abort) = exchange.abort {
            abort.abort();
        }
    }
    match peer {
        Peer::Stdio(p) => p.stop().await,
        Peer::Http(p) => p.stop().await,
    }
}

/// Waits for an exchange and prints its `response` event.
async fn finish(
    step: &str,
    exchange: Exchange,
    responses: &mut BTreeMap<String, Value>,
) -> Result<(), String> {
    let response = exchange
        .done
        .await
        .map_err(|_| format!("step {step}: the server answered nothing"))?
        .map_err(|e| format!("step {step}: {e}"))?;
    if let Some(message) = &response.message {
        responses.insert(step.to_owned(), message.clone());
    }
    let event = json!({
        "event": "response",
        "step": step,
        "http_status": response.http_status,
        "session_id": response.session_id,
        "message": response.message,
        "notifications": response.notifications,
    });
    println!("{event}");
    Ok(())
}

/// Replaces every `{"$from": "<step>/<pointer>"}` with the value the RFC 6901 pointer selects in that step's
/// response.
fn resolve(value: &Value, responses: &BTreeMap<String, Value>) -> Result<Value, String> {
    Ok(match value {
        Value::Object(map) => {
            if let (1, Some(Value::String(from))) = (map.len(), map.get("$from")) {
                let (step, pointer) = from.split_once('/').unwrap_or((from, ""));
                return responses
                    .get(step)
                    .and_then(|r| r.pointer(&format!("/{pointer}")))
                    .cloned()
                    .ok_or_else(|| format!("$from {from:?} selects nothing"));
            }
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                out.insert(key.clone(), resolve(item, responses)?);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve(item, responses))
                .collect::<Result<_, _>>()?,
        ),
        other => other.clone(),
    })
}

/// Whether `message` is the response to the request whose id has the compact JSON `id`.
fn answers(message: &Value, id: &str) -> bool {
    message.get("id").map(Value::to_string).as_deref() == Some(id)
        && (message.get("result").is_some() || message.get("error").is_some())
}

struct Inflight {
    id: String,
    notifications: u32,
    done: oneshot::Sender<Result<Response, String>>,
}

type Inflights = Arc<Mutex<Vec<Inflight>>>;

/// The reference server as a child process, speaking JSON lines over its stdin and stdout.
struct StdioPeer {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: JoinHandle<()>,
    inflight: Inflights,
}

impl StdioPeer {
    fn start(mut raw: Value) -> Result<Self, String> {
        if let Some(spec) = raw.as_object_mut() {
            spec.remove("steps");
        }
        let program = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut child = tokio::process::Command::new(program)
            .arg("serve-stdio")
            .arg(raw.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("starting the reference server: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("the reference server has no stdout")?;
        let inflight = Inflights::default();
        let reader = tokio::spawn(read_lines(stdout, inflight.clone()));
        Ok(Self {
            child,
            stdin,
            reader,
            inflight,
        })
    }

    async fn send(&mut self, id: Option<String>, body: Vec<u8>) -> Result<Exchange, String> {
        let (tx, done) = oneshot::channel();
        match id {
            Some(id) => self
                .inflight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Inflight {
                    id,
                    notifications: 0,
                    done: tx,
                }),
            None => {
                let _sent = tx.send(Ok(Response::default()));
            }
        }
        let stdin = self.stdin.as_mut().ok_or("stdin is closed")?;
        let mut line = body;
        line.push(b'\n');
        stdin.write_all(&line).await.map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        Ok(Exchange { done, abort: None })
    }

    /// Closes the server's stdin and waits for it to flush and exit.
    async fn stop(mut self) -> Result<(), String> {
        drop(self.stdin.take());
        let _read = self.reader.await;
        let status = self.child.wait().await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("the reference server exited with {status}"))
        }
    }
}

/// Matches each response line to its request; every other line is a server message for the request most
/// recently sent.
async fn read_lines(stdout: ChildStdout, inflight: Inflights) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let mut inflight = inflight.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = inflight.iter().position(|i| answers(&message, &i.id)) {
            let done = inflight.remove(index);
            let _sent = done.done.send(Ok(Response {
                message: Some(message),
                notifications: done.notifications,
                ..Response::default()
            }));
        } else if let Some(last) = inflight.last_mut() {
            last.notifications += 1;
        }
    }
}

/// The reference server in process on 127.0.0.1, each message sent as its own POST.
struct HttpPeer {
    url: String,
    session: Arc<Mutex<Option<String>>>,
    server: JoinHandle<()>,
    cancel: CancellationToken,
}

impl HttpPeer {
    async fn start(spec: &RunSpec) -> Result<Self, String> {
        crate::init_sdk(spec)?;
        let instrumentation = server::instrumentation(spec)?;
        let reference = server::Reference::new(spec);
        // Dual era: an initialize request opens a legacy session, and a request at the modern version is served
        // statelessly, one wrapped handler per request.
        let config = StreamableHttpServerConfig::default();
        let cancel = config.cancellation_token.clone();
        let service = StreamableHttpService::new(
            move || Ok(instrumentation.wrap(reference.clone())),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let service = TowerToHyperService::new(service.clone());
                tokio::spawn(async move {
                    let _served = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Ok(Self {
            url: format!("http://{address}/mcp"),
            session: Arc::default(),
            server,
            cancel,
        })
    }

    fn send(&self, id: Option<String>, body: Vec<u8>, step_headers: &BTreeMap<String, String>) -> Exchange {
        let mut headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "application/json, text/event-stream".to_owned()),
        ];
        headers.extend(step_headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        let session = self.session.clone();
        if let Some(id) = session.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            headers.push(("mcp-session-id".to_owned(), id));
        }
        let url = self.url.clone();
        let (tx, done) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = post(&url, &headers, body, id.as_deref()).await;
            if let Ok(Response {
                session_id: Some(id),
                ..
            }) = &result
            {
                session
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_or_insert_with(|| id.clone());
            }
            let _sent = tx.send(result);
        });
        Exchange {
            done,
            abort: Some(task.abort_handle()),
        }
    }

    /// Stops the server, which ends its sessions, then flushes the SDK.
    async fn stop(self) -> Result<(), String> {
        self.cancel.cancel();
        self.server.abort();
        crucible_sdk::shutdown().await.map_err(|e| e.to_string())
    }
}

/// POSTs `body` on a connection of its own. For a request (`id` is the compact JSON of its id) it reads the
/// response, one JSON object or an event stream, until the message with that id; the messages before it count
/// as server messages for the request.
pub(crate) async fn post(
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    id: Option<&str>,
) -> Result<Response, String> {
    let uri: http::Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| format!("{url} has no host"))?
        .to_string();
    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let stream = tokio::net::TcpStream::connect(&authority)
        .await
        .map_err(|e| format!("connect {authority}: {e}"))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    let mut request = http::Request::post(path).header(HOST, authority.as_str());
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| e.to_string())?;
    let exchange = async move {
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        read_response(response, id).await
    };
    let mut exchange = std::pin::pin!(exchange);
    let mut connection = std::pin::pin!(connection);
    // The connection is driven in this task, so aborting the task closes it.
    tokio::select! {
        biased;
        result = &mut exchange => result,
        _ = &mut connection => exchange.await,
    }
}

async fn read_response(response: http::Response<Incoming>, id: Option<&str>) -> Result<Response, String> {
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let stream = header(CONTENT_TYPE).is_some_and(|t| t.starts_with("text/event-stream"));
    let mut out = Response {
        http_status: Some(response.status().as_u16()),
        session_id: header(http::HeaderName::from_static("mcp-session-id")),
        ..Response::default()
    };
    let mut body = response.into_body();
    let Some(id) = id else {
        while let Some(frame) = body.frame().await {
            frame.map_err(|e| e.to_string())?;
        }
        return Ok(out);
    };
    if !stream {
        let bytes = body.collect().await.map_err(|e| e.to_string())?.to_bytes();
        out.message = Some(serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "status {:?}, body {:?}: {e}",
                out.http_status,
                String::from_utf8_lossy(&bytes)
            )
        })?);
        return Ok(out);
    }
    let mut buffer = Vec::new();
    let mut data = String::new();
    while let Some(frame) = body.frame().await {
        let Ok(chunk) = frame.map_err(|e| e.to_string())?.into_data() else {
            continue;
        };
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            } else if line.is_empty() && !data.is_empty() && dispatch(&mut out, &mut data, id) {
                return Ok(out);
            }
        }
    }
    if !data.is_empty() && dispatch(&mut out, &mut data, id) {
        return Ok(out);
    }
    Err("the event stream ended before the response".to_owned())
}

/// Takes one buffered event; reports whether it was the response.
fn dispatch(out: &mut Response, data: &mut String, id: &str) -> bool {
    let message = serde_json::from_str::<Value>(data).ok();
    data.clear();
    match message {
        Some(message) if answers(&message, id) => {
            out.message = Some(message);
            true
        }
        _ => {
            out.notifications += 1;
            false
        }
    }
}
