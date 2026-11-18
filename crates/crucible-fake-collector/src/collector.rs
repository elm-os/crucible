//! The loopback OTLP/HTTP collector.
//!
//! It accepts `POST /v1/traces` in protobuf (`application/x-protobuf`) or JSON (`application/json`), optionally
//! gzip-compressed, runs every body through the same scanner as ingest (`crucible-otlp`, after transcoding JSON),
//! and records each request with its spans in one ordered log. `POST /conformance/checkpoint` with
//! `{"name": "<name>"}` adds a checkpoint to the same log, so a reader can tell which exports arrived before it.
//! Requests the collector refuses (wrong path, method, media type or encoding, malformed body) are recorded too,
//! with the status they were answered and the reason.

use std::convert::Infallible;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use crucible_env::{Env, JoinHandle, Listener, NetError};
use crucible_otlp::{MAX_BODY_BYTES, Rejection, TraceRequest, transcode_json};
use crucible_records::{SpanId, SpanKind, StatusCode, Timestamp, TraceId};
use flate2::read::GzDecoder;
use http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode as HttpStatus};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceResponse,
};
use prost::Message as _;

use crate::value::{Attributes, owned};

/// The OTLP traces path.
pub const TRACES_PATH: &str = "/v1/traces";
/// The checkpoint path.
pub const CHECKPOINT_PATH: &str = "/conformance/checkpoint";

/// One entry of the collector's log, in arrival order.
#[derive(Clone, Debug)]
pub enum Event {
    /// A request to any path other than the checkpoint path.
    Export(Export),
    /// A checkpoint posted by a runner.
    Checkpoint(String),
}

/// One recorded request.
#[derive(Clone, Debug)]
pub struct Export {
    /// The HTTP method.
    pub method: String,
    /// The request path.
    pub path: String,
    /// The `Content-Type` header.
    pub content_type: Option<String>,
    /// The `Content-Encoding` header.
    pub content_encoding: Option<String>,
    /// The `Authorization` header.
    pub authorization: Option<String>,
    /// The status the collector answered.
    pub status: u16,
    /// Why the request was refused; `None` when it was accepted.
    pub refusal: Option<String>,
    /// The decompressed body: canonical protobuf when the request was accepted.
    pub body: Vec<u8>,
    /// The spans of an accepted request, in wire order, rejected ones included.
    pub spans: Vec<RecordedSpan>,
}

/// A span as received, with its resource and instrumentation scope.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedSpan {
    /// Trace id.
    pub trace_id: TraceId,
    /// Span id.
    pub span_id: SpanId,
    /// Parent span id; `None` for a root.
    pub parent_span_id: Option<SpanId>,
    /// Span name.
    pub name: String,
    /// Span kind.
    pub kind: SpanKind,
    /// Start time.
    pub start: Timestamp,
    /// End time.
    pub end: Timestamp,
    /// Status code.
    pub status: StatusCode,
    /// Status message.
    pub status_message: String,
    /// Span attributes.
    pub attributes: Attributes,
    /// Span events.
    pub events: Vec<RecordedEvent>,
    /// Span links.
    pub links: Vec<RecordedLink>,
    /// Resource attributes.
    pub resource: Attributes,
    /// Instrumentation scope name.
    pub scope_name: String,
    /// Instrumentation scope version.
    pub scope_version: String,
    /// Instrumentation scope attributes.
    pub scope_attributes: Attributes,
    /// Why ingest would reject the span through partial success.
    pub rejection: Option<Rejection>,
}

/// A span event.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedEvent {
    /// Event name.
    pub name: String,
    /// Event time.
    pub time: Timestamp,
    /// Event attributes.
    pub attributes: Attributes,
}

/// A span link.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedLink {
    /// Linked trace id.
    pub trace_id: TraceId,
    /// Linked span id.
    pub span_id: SpanId,
    /// Link attributes.
    pub attributes: Attributes,
}

type Log = Arc<Mutex<Vec<Event>>>;

/// The running collector. Dropping it stops accepting connections.
#[derive(Debug)]
pub struct Collector {
    addr: SocketAddr,
    log: Log,
    accept: JoinHandle<()>,
}

impl Collector {
    /// Binds a loopback port and starts serving.
    pub async fn start(env: &Env) -> Result<Self, NetError> {
        let listener = env
            .net
            .listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await?;
        let addr = listener.local_addr()?;
        let log = Log::default();
        let accept = env.spawn("fake-collector", serve(env.clone(), listener, log.clone()));
        Ok(Self { addr, log, accept })
    }

    /// The bound address.
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL an OTLP exporter is configured with, `http://127.0.0.1:<port>`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The full traces URL.
    pub fn traces_url(&self) -> String {
        format!("{}{TRACES_PATH}", self.endpoint())
    }

    /// The checkpoint URL.
    pub fn checkpoint_url(&self) -> String {
        format!("{}{CHECKPOINT_PATH}", self.endpoint())
    }

    /// A copy of the log.
    pub fn events(&self) -> Vec<Event> {
        self.lock().clone()
    }

    /// Empties the log and returns what it held.
    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.lock())
    }

    /// How many entries the log holds.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Event>> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve(env: Env, mut listener: Box<dyn Listener>, log: Log) {
    while let Ok((stream, _peer)) = listener.accept().await {
        let log = log.clone();
        let service = service_fn(move |request| respond(log.clone(), request));
        // Dropping the handle detaches the connection task.
        drop(env.spawn("fake-collector-connection", async move {
            let _closed = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        }));
    }
}

fn header(headers: &HeaderMap, name: http::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// The media type without parameters, lowercased.
fn media_type(content_type: Option<&str>) -> String {
    content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

async fn respond(
    log: Log,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = request.into_parts();
    let raw = match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };
    let path = parts.uri.path().to_owned();
    if parts.method == Method::POST && path == CHECKPOINT_PATH {
        return Ok(checkpoint(&log, &raw));
    }
    let mut export = Export {
        method: parts.method.to_string(),
        path,
        content_type: header(&parts.headers, CONTENT_TYPE),
        content_encoding: header(&parts.headers, CONTENT_ENCODING),
        authorization: header(&parts.headers, AUTHORIZATION),
        status: 200,
        refusal: None,
        body: Vec::new(),
        spans: Vec::new(),
    };
    let json = media_type(export.content_type.as_deref()) == "application/json";
    let response = match accept(&mut export, &parts.method, &raw) {
        Ok(rejected) => ok(json, rejected),
        Err((status, why)) => {
            export.status = status.as_u16();
            export.refusal = Some(why.clone());
            plain(status, why)
        }
    };
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Event::Export(export));
    Ok(response)
}

/// Decodes and scans an export. Returns the number of spans rejected through partial success, or the status and
/// reason of a refusal.
fn accept(export: &mut Export, method: &Method, raw: &[u8]) -> Result<u32, (HttpStatus, String)> {
    if *method != Method::POST {
        return Err((
            HttpStatus::METHOD_NOT_ALLOWED,
            format!("{method} is not POST"),
        ));
    }
    if export.path != TRACES_PATH {
        return Err((
            HttpStatus::NOT_FOUND,
            format!("{} is not {TRACES_PATH}", export.path),
        ));
    }
    let body = match export
        .content_encoding
        .as_deref()
        .map(|e| e.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("" | "identity") => raw.to_vec(),
        Some("gzip") => gunzip(raw)?,
        Some(other) => {
            return Err((
                HttpStatus::UNSUPPORTED_MEDIA_TYPE,
                format!("content encoding {other} is not gzip"),
            ));
        }
    };
    let canonical = match media_type(export.content_type.as_deref()).as_str() {
        "application/x-protobuf" => body,
        "application/json" => {
            export.body = body;
            transcode_json(&export.body).map_err(|e| (HttpStatus::BAD_REQUEST, e.to_string()))?
        }
        other => {
            export.body = body;
            return Err((
                HttpStatus::UNSUPPORTED_MEDIA_TYPE,
                format!("content type {other:?} is not application/x-protobuf or application/json"),
            ));
        }
    };
    export.body = canonical;
    let request = TraceRequest::scan(&export.body, None)
        .map_err(|e| (HttpStatus::BAD_REQUEST, e.to_string()))?;
    let rejected = request.partial_success().map_or(0, |p| p.rejected_spans());
    export.spans = request.spans().map(|span| record(&span)).collect();
    Ok(rejected)
}

fn gunzip(raw: &[u8]) -> Result<Vec<u8>, (HttpStatus, String)> {
    let mut out = Vec::new();
    let cap = u64::try_from(MAX_BODY_BYTES).unwrap_or(u64::MAX);
    GzDecoder::new(raw)
        .take(cap.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| (HttpStatus::BAD_REQUEST, format!("gzip: {e}")))?;
    if out.len() > MAX_BODY_BYTES {
        return Err((
            HttpStatus::PAYLOAD_TOO_LARGE,
            "decompressed body is above the cap".to_owned(),
        ));
    }
    Ok(out)
}

fn record(span: &crucible_otlp::SpanView<'_>) -> RecordedSpan {
    let status = span.status();
    let scope = span.scope();
    RecordedSpan {
        trace_id: span.trace_id(),
        span_id: span.span_id(),
        parent_span_id: span.parent_span_id(),
        name: span.name().to_owned(),
        kind: span.kind(),
        start: span.start_time(),
        end: span.end_time(),
        status: status.code(),
        status_message: status.message().to_owned(),
        attributes: owned(span.attributes()),
        events: span
            .events()
            .map(|e| RecordedEvent {
                name: e.name().to_owned(),
                time: e.time(),
                attributes: owned(e.attributes()),
            })
            .collect(),
        links: span
            .links()
            .map(|l| RecordedLink {
                trace_id: l.trace_id(),
                span_id: l.span_id(),
                attributes: owned(l.attributes()),
            })
            .collect(),
        resource: owned(span.resource().attributes()),
        scope_name: scope.name().to_owned(),
        scope_version: scope.version().to_owned(),
        scope_attributes: owned(scope.attributes()),
        rejection: span.rejection(),
    }
}

fn checkpoint(log: &Log, raw: &[u8]) -> Response<Full<Bytes>> {
    #[derive(serde::Deserialize)]
    struct Body {
        name: String,
    }
    match serde_json::from_slice::<Body>(raw) {
        Ok(body) if !body.name.is_empty() => {
            log.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Event::Checkpoint(body.name));
            plain(HttpStatus::NO_CONTENT, String::new())
        }
        _ => plain(
            HttpStatus::BAD_REQUEST,
            "a checkpoint body is {\"name\": \"<non-empty name>\"}".to_owned(),
        ),
    }
}

fn ok(json: bool, rejected: u32) -> Response<Full<Bytes>> {
    let partial = (rejected > 0).then(|| ExportTracePartialSuccess {
        rejected_spans: i64::from(rejected),
        error_message: format!("{rejected} spans have an all-zero trace or span id"),
    });
    let (body, media) = if json {
        let body = match &partial {
            Some(p) => serde_json::json!({
                "partialSuccess": { "rejectedSpans": p.rejected_spans.to_string(), "errorMessage": p.error_message }
            })
            .to_string(),
            None => "{}".to_owned(),
        };
        (Bytes::from(body), "application/json")
    } else {
        let response = ExportTraceServiceResponse {
            partial_success: partial,
        };
        (
            Bytes::from(response.encode_to_vec()),
            "application/x-protobuf",
        )
    };
    let mut response = Response::new(Full::new(body));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(media));
    response
}

fn plain(status: HttpStatus, text: String) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(text)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
