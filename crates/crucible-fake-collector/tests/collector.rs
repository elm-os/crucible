//! The collector over loopback HTTP, through the production `Env`: what it records for protobuf, JSON and gzip
//! bodies, and how it answers and records requests it refuses.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::Env;
use crucible_env_prod::ProdConfig;
use crucible_fake_collector::value::Value;
use crucible_fake_collector::{Collector, Event, Export};
use crucible_records::{SpanId, SpanKind, TraceId};
use flate2::Compression;
use flate2::write::GzEncoder;
use http::Request;
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value::Value as Proto;
use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use proptest::prelude::*;
use prost::Message as _;

fn env() -> Env {
    crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap()
        .env
}

/// Posts `body` and returns the status and the response body.
async fn post(
    env: &Env,
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, Bytes) {
    let deadline = env.clock.now().saturating_add(Duration::from_secs(5));
    let stream = env.net.connect(addr, deadline).await.unwrap();
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await.unwrap();
    let driver = env.spawn("test-connection", async move {
        let _closed = connection.await;
    });
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(request.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    driver.abort();
    (status, body)
}

fn kv(key: &str, value: Proto) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        ..KeyValue::default()
    }
}

fn request(spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", Proto::StringValue("svc".to_owned()))],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans,
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
}

fn exports(collector: &Collector) -> Vec<Export> {
    collector
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Export(x) => Some(x),
            Event::Checkpoint(_) => None,
        })
        .collect()
}

fn value() -> impl Strategy<Value = (Proto, Value)> {
    prop_oneof![
        "[a-z ]{0,12}".prop_map(|s| (Proto::StringValue(s.clone()), Value::String(s))),
        any::<bool>().prop_map(|b| (Proto::BoolValue(b), Value::Bool(b))),
        any::<i64>().prop_map(|i| (Proto::IntValue(i), Value::Int(i))),
        (-1.0e9f64..1.0e9).prop_map(|d| (Proto::DoubleValue(d), Value::Double(d))),
        prop::collection::vec("[a-z]{1,6}", 0..4).prop_map(|items| {
            let proto = Proto::ArrayValue(ArrayValue {
                values: items
                    .iter()
                    .map(|s| AnyValue {
                        value: Some(Proto::StringValue(s.clone())),
                    })
                    .collect(),
            });
            (
                proto,
                Value::Array(items.into_iter().map(Value::String).collect()),
            )
        }),
    ]
}

#[derive(Clone, Debug)]
struct Expected {
    trace: [u8; 16],
    span: [u8; 8],
    parent: Option<[u8; 8]>,
    name: String,
    kind: i32,
    attributes: Vec<(String, Value)>,
}

fn span() -> impl Strategy<Value = (Span, Expected)> {
    (
        any::<[u8; 16]>().prop_filter("non-zero", |t| t.iter().any(|b| *b != 0)),
        any::<[u8; 8]>().prop_filter("non-zero", |s| s.iter().any(|b| *b != 0)),
        prop::option::of(any::<[u8; 8]>().prop_filter("non-zero", |s| s.iter().any(|b| *b != 0))),
        "[a-z/ ]{1,16}",
        0..6i32,
        prop::collection::vec(("[a-z.]{1,10}", value()), 0..6),
    )
        .prop_map(|(trace, span, parent, name, kind, attributes)| {
            let proto = Span {
                trace_id: trace.to_vec(),
                span_id: span.to_vec(),
                parent_span_id: parent.map(|p| p.to_vec()).unwrap_or_default(),
                name: name.clone(),
                kind,
                start_time_unix_nano: 1_800_000_000_000_000_000,
                end_time_unix_nano: 1_800_000_000_250_000_000,
                attributes: attributes
                    .iter()
                    .map(|(k, (p, _))| kv(k, p.clone()))
                    .collect(),
                ..Span::default()
            };
            let expected = Expected {
                trace,
                span,
                parent,
                name,
                kind,
                attributes: attributes.into_iter().map(|(k, (_, v))| (k, v)).collect(),
            };
            (proto, expected)
        })
}

fn gzip(body: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(body).unwrap();
    encoder.finish().unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever the encoding, the collector records every span exactly as it was built.
    #[test]
    fn spans_are_recorded_as_sent(spans in prop::collection::vec(span(), 1..6), encoding in 0..3u8) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let env = env();
            let collector = Collector::start(&env).await.unwrap();
            let (protos, expected): (Vec<_>, Vec<_>) = spans.into_iter().unzip();
            let request = request(protos);
            let (body, headers): (Vec<u8>, Vec<(&str, &str)>) = match encoding {
                0 => (request.encode_to_vec(), vec![("content-type", "application/x-protobuf")]),
                1 => (serde_json::to_vec(&request).unwrap(), vec![("content-type", "application/json")]),
                _ => (
                    gzip(&request.encode_to_vec()),
                    vec![("content-type", "application/x-protobuf"), ("content-encoding", "gzip")],
                ),
            };
            let (status, _) = post(&env, collector.addr(), "POST", "/v1/traces", &headers, body).await;
            prop_assert_eq!(status, 200);
            let exports = exports(&collector);
            prop_assert_eq!(exports.len(), 1);
            let recorded = &exports[0].spans;
            prop_assert_eq!(recorded.len(), expected.len());
            for (got, want) in recorded.iter().zip(&expected) {
                prop_assert_eq!(got.trace_id, TraceId::from_bytes(want.trace));
                prop_assert_eq!(got.span_id, SpanId::from_bytes(want.span));
                prop_assert_eq!(got.parent_span_id, want.parent.map(SpanId::from_bytes));
                prop_assert_eq!(&got.name, &want.name);
                prop_assert_eq!(got.kind, SpanKind::from_value(u8::try_from(want.kind).unwrap()).unwrap());
                prop_assert_eq!(&got.attributes, &want.attributes);
                prop_assert_eq!(got.end.micros() - got.start.micros(), 250_000);
                prop_assert_eq!(&got.resource, &vec![("service.name".to_owned(), Value::String("svc".to_owned()))]);
            }
            Ok::<(), TestCaseError>(())
        })?;
    }
}

/// Method, path, headers, body and the expected status.
type Refused<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, Vec<u8>, u16);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_requests_are_answered_and_recorded() {
    let env = env();
    let collector = Collector::start(&env).await.unwrap();
    let addr = collector.addr();
    let proto = [("content-type", "application/x-protobuf")];
    let valid = request(vec![Span {
        trace_id: vec![1; 16],
        span_id: vec![2; 8],
        name: "ok".to_owned(),
        ..Span::default()
    }])
    .encode_to_vec();
    let cases: [Refused<'_>; 5] = [
        ("GET", "/v1/traces", proto.to_vec(), Vec::new(), 405),
        ("POST", "/v1/logs", proto.to_vec(), valid.clone(), 404),
        (
            "POST",
            "/v1/traces",
            vec![("content-type", "text/plain")],
            valid.clone(),
            415,
        ),
        (
            "POST",
            "/v1/traces",
            proto.to_vec(),
            vec![0xff, 0xff, 0xff],
            400,
        ),
        (
            "POST",
            "/v1/traces",
            vec![
                ("content-type", "application/x-protobuf"),
                ("content-encoding", "br"),
            ],
            valid.clone(),
            415,
        ),
    ];
    for (method, path, headers, body, want) in cases {
        let (status, _) = post(&env, addr, method, path, &headers, body).await;
        assert_eq!(status, want, "{method} {path} {headers:?}");
    }
    let recorded = exports(&collector);
    assert_eq!(recorded.len(), 5);
    assert!(
        recorded
            .iter()
            .all(|x| x.refusal.is_some() && x.spans.is_empty())
    );
    assert_eq!(
        recorded.iter().map(|x| x.status).collect::<Vec<_>>(),
        [405, 404, 415, 400, 415]
    );
    assert!(collector.take().len() == 5 && collector.is_empty());

    // A zero trace id is a partial success, recorded with the span's rejection.
    let zero = request(vec![Span {
        trace_id: vec![0; 16],
        span_id: vec![2; 8],
        ..Span::default()
    }]);
    let (status, body) = post(
        &env,
        addr,
        "POST",
        "/v1/traces",
        &proto,
        zero.encode_to_vec(),
    )
    .await;
    assert_eq!(status, 200);
    let response = ExportTraceServiceResponse::decode(body).unwrap();
    assert_eq!(response.partial_success.unwrap().rejected_spans, 1);
    assert!(exports(&collector)[0].spans[0].rejection.is_some());

    // A JSON request gets a JSON answer; the authorization header is kept.
    let json = [
        ("content-type", "application/json"),
        ("authorization", "Bearer csk_x"),
    ];
    let (status, body) = post(
        &env,
        addr,
        "POST",
        "/v1/traces",
        &json,
        serde_json::to_vec(&request(Vec::new())).unwrap(),
    )
    .await;
    assert_eq!((status, &body[..]), (200, &b"{}"[..]));
    assert_eq!(
        exports(&collector)[1].authorization.as_deref(),
        Some("Bearer csk_x")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoints_are_ordered_against_exports() {
    let env = env();
    let collector = Collector::start(&env).await.unwrap();
    let addr = collector.addr();
    let proto = [("content-type", "application/x-protobuf")];
    let json = [("content-type", "application/json")];
    let body = request(Vec::new()).encode_to_vec();
    post(&env, addr, "POST", "/v1/traces", &proto, body.clone()).await;
    let (status, _) = post(
        &env,
        addr,
        "POST",
        "/conformance/checkpoint",
        &json,
        br#"{"name":"flushed"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 204);
    post(&env, addr, "POST", "/v1/traces", &proto, body).await;
    let (status, _) = post(
        &env,
        addr,
        "POST",
        "/conformance/checkpoint",
        &json,
        br#"{"name":""}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 400);
    let events = collector.events();
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0], Event::Export(_)));
    assert!(matches!(&events[1], Event::Checkpoint(name) if name == "flushed"));
    assert!(matches!(&events[2], Event::Export(_)));
    assert_eq!(collector.traces_url(), format!("http://{addr}/v1/traces"));
}
