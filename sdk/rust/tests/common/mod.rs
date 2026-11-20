//! A fake OTLP/HTTP collector on a loopback port: a hyper server that decodes every export and keeps the raw
//! request, so tests can check spans, headers and that no secret reaches the wire. It also answers the feedback
//! route, `POST /api/v1/projects/{id}/feedback`: with the scripted answers while any are left, else with 201.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value as Proto;
use prost::Message as _;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The secret key every test exports with.
pub const SECRET_KEY: &str = "csk_test0000000000000000000000000000000000000";

/// Tests in one binary share the SDK's process-wide client, so they take turns.
pub static GLOBAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How the collector answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 200 with an empty response.
    Accept,
    /// Accepts the request and never answers.
    Stall,
}

/// One scripted answer to a feedback post. Status 0 closes the connection without answering.
#[derive(Clone, Debug, Default)]
pub struct FeedbackAnswer {
    pub status: u16,
    pub headers: Vec<(&'static str, &'static str)>,
    pub body: &'static str,
}

/// One request as received: an export or a feedback post.
#[derive(Clone, Debug)]
pub struct Export {
    pub path: String,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

/// A span as received, with its resource attributes.
#[derive(Clone, Debug)]
pub struct Span {
    pub name: String,
    pub kind: i32,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
    pub attributes: BTreeMap<String, Value>,
    pub resource: BTreeMap<String, Value>,
    pub status_code: i32,
    pub status_message: String,
}

impl Span {
    /// An attribute, or `Value::Null` when absent.
    pub fn attr(&self, key: &str) -> Value {
        self.attributes.get(key).cloned().unwrap_or(Value::Null)
    }
}

#[derive(Debug)]
pub struct Collector {
    pub endpoint: String,
    exports: Arc<Mutex<Vec<Export>>>,
    answers: Arc<Mutex<VecDeque<FeedbackAnswer>>>,
    task: JoinHandle<()>,
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Collector {
    pub async fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let exports = Arc::new(Mutex::new(Vec::new()));
        let answers = Arc::new(Mutex::new(VecDeque::new()));
        let (recorded, scripted) = (exports.clone(), answers.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (recorded, scripted) = (recorded.clone(), scripted.clone());
                let service = service_fn(move |request| {
                    answer(request, mode, recorded.clone(), scripted.clone())
                });
                tokio::spawn(async move {
                    let _served = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            endpoint: format!("http://{address}"),
            exports,
            answers,
            task,
        }
    }

    /// Scripts the answers to the next feedback posts, in order.
    pub fn answer_feedback(&self, answers: impl IntoIterator<Item = FeedbackAnswer>) {
        self.answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(answers);
    }

    /// The feedback posts, in arrival order.
    pub fn feedback(&self) -> Vec<Export> {
        self.exports()
            .into_iter()
            .filter(|e| is_feedback(&e.path))
            .collect()
    }

    pub fn exports(&self) -> Vec<Export> {
        self.exports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether any export body holds `needle`: protobuf carries strings as their UTF-8 bytes.
    pub fn wire_contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        self.exports()
            .iter()
            .any(|e| e.body.windows(needle.len()).any(|w| w == needle))
    }

    /// Every span of every export, in arrival order.
    pub fn spans(&self) -> Vec<Span> {
        let mut spans = Vec::new();
        for export in self
            .exports()
            .into_iter()
            .filter(|e| e.path == "/v1/traces")
        {
            let request = ExportTraceServiceRequest::decode(export.body.as_slice()).unwrap();
            for resource_spans in request.resource_spans {
                let resource = resource_spans
                    .resource
                    .map(|r| attributes(&r.attributes))
                    .unwrap_or_default();
                for scope in resource_spans.scope_spans {
                    for span in scope.spans {
                        let status = span.status.unwrap_or_default();
                        spans.push(Span {
                            name: span.name,
                            kind: span.kind,
                            trace_id: hex(&span.trace_id),
                            span_id: hex(&span.span_id),
                            parent_span_id: hex(&span.parent_span_id),
                            attributes: attributes(&span.attributes),
                            resource: resource.clone(),
                            status_code: status.code,
                            status_message: status.message,
                        });
                    }
                }
            }
        }
        spans
    }

    /// The span named `name`; panics when there is not exactly one.
    pub fn span(&self, name: &str) -> Span {
        let spans: Vec<Span> = self
            .spans()
            .into_iter()
            .filter(|s| s.name == name)
            .collect();
        assert_eq!(spans.len(), 1, "spans named {name:?}: {spans:#?}");
        spans.into_iter().next().unwrap()
    }
}

fn is_feedback(path: &str) -> bool {
    path.strip_prefix("/api/v1/projects/")
        .and_then(|rest| rest.strip_suffix("/feedback"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// An error that makes hyper close the connection without an answer.
#[derive(Debug)]
struct Hangup;

impl std::fmt::Display for Hangup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("hang up")
    }
}

impl std::error::Error for Hangup {}

async fn answer(
    request: Request<Incoming>,
    mode: Mode,
    exports: Arc<Mutex<Vec<Export>>>,
    answers: Arc<Mutex<VecDeque<FeedbackAnswer>>>,
) -> Result<Response<Full<Bytes>>, Hangup> {
    if mode == Mode::Stall {
        std::future::pending::<()>().await;
    }
    let path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let body = request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    exports
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Export {
            path: path.clone(),
            headers,
            body,
        });
    if is_feedback(&path) {
        let scripted = answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let answer = scripted.unwrap_or(FeedbackAnswer {
            status: 201,
            headers: vec![("content-type", "application/json")],
            body: r#"{"key":"k"}"#,
        });
        if answer.status == 0 {
            return Err(Hangup);
        }
        let mut response = Response::builder().status(answer.status);
        for (name, value) in answer.headers {
            response = response.header(name, value);
        }
        return Ok(response.body(Full::new(Bytes::from(answer.body))).unwrap());
    }
    Ok(Response::builder()
        .header("content-type", "application/x-protobuf")
        .body(Full::new(Bytes::new()))
        .unwrap())
}

fn attributes(
    list: &[opentelemetry_proto::tonic::common::v1::KeyValue],
) -> BTreeMap<String, Value> {
    list.iter()
        .map(|kv| (kv.key.clone(), kv.value.as_ref().map_or(Value::Null, value)))
        .collect()
}

fn value(v: &AnyValue) -> Value {
    match &v.value {
        Some(Proto::StringValue(s)) => json!(s),
        Some(Proto::BoolValue(b)) => json!(b),
        Some(Proto::IntValue(i)) => json!(i),
        Some(Proto::DoubleValue(d)) => json!(d),
        Some(Proto::ArrayValue(a)) => Value::Array(a.values.iter().map(value).collect()),
        other => json!(format!("{other:?}")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Starts the SDK against `collector` with the test key, after stopping any client a previous test left.
pub async fn init(collector: &Collector, configure: impl FnOnce(&mut crucible_sdk::Options)) {
    let _stopped = crucible_sdk::shutdown().await;
    let mut options = crucible_sdk::Options {
        endpoint: collector.endpoint.clone(),
        secret_key: SECRET_KEY.to_owned(),
        service_name: "sdk-tests".to_owned(),
        flush_interval: Duration::from_secs(60),
        ..crucible_sdk::Options::default()
    };
    configure(&mut options);
    crucible_sdk::init(options).unwrap();
}
