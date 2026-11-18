//! Helpers shared by the handler tests: keys, requests, responses, gRPC clients and the WAL frames they leave.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::{Buf as _, BufMut as _, Bytes};
use crucible_auth::{PublicKey, SecretKey, StaticKey, StaticKeys};
use crucible_env::{ChaChaRng, Env};
use crucible_ingest::{CompatRoute, Ingest, IngestConfig, RoutingHeader};
use crucible_records::{ApiKeyId, Lsn, ProjectId};
use crucible_wal::{Wal, WalConfig};
use crucible_wal_codec::{IngestRequest, Record, RecordType};
use flate2::Compression;
use flate2::write::GzEncoder;
use http_body_util::BodyExt as _;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message as _;
use tonic::codec::{Codec, CompressionEncoding, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::metadata::MetadataValue;
use tower::ServiceExt as _;

/// The project of the test key.
pub const PROJECT: ProjectId = ProjectId::from_bytes([
    0x01, 0x93, 0xa1, 0xb2, 0x7c, 0x3d, 0x7e, 0x4f, 0x8a, 0x5b, 0x6c, 0x7d, 0x8e, 0x9f, 0x0a, 0x1b,
]);
/// The id of the test key.
pub const KEY_ID: ApiKeyId = ApiKeyId::from_bytes([
    0x01, 0x93, 0xa1, 0xb2, 0x7c, 0x3d, 0x7e, 0x4f, 0x8a, 0x5b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
]);
/// The project of the other key, whose pair the mismatch cases mix with the test key's.
pub const OTHER_PROJECT: ProjectId = ProjectId::from_bytes([0x02; 16]);
/// `User-Agent` of the test requests, as an OTLP exporter sends it.
pub const USER_AGENT: &str = "OTel-OTLP-Exporter-Rust/0.31.0 extra/1";
pub const PROTOBUF: &str = "application/x-protobuf";
pub const JSON: &str = "application/json";
/// A day in nanoseconds.
pub const DAY_NANOS: u64 = 86_400_000_000_000;

/// An invented compat route with SDK identity headers, and its hint.
pub const COMPAT_PATH: &str = "/compat-a/v1/traces";
pub const COMPAT_DIALECT: u8 = 5;
pub const SDK_NAME_HEADER: &str = "x-sdk-name";
pub const SDK_VERSION_HEADER: &str = "x-sdk-version";
/// An invented compat route without SDK identity headers, and its hint.
pub const PLAIN_COMPAT_PATH: &str = "/compat-b/traces";
pub const PLAIN_COMPAT_DIALECT: u8 = 6;
/// An invented routing header, and the hint it sets on `/v1/traces`.
pub const ROUTING_HEADER: &str = "x-route-to";
pub const ROUTING_DIALECT: u8 = 4;

/// The secret of the test key.
pub fn secret() -> String {
    format!("csk_{}", "1".repeat(43))
}

/// The public key of the test key.
pub fn public() -> String {
    format!("cpk_{}", "1".repeat(43))
}

/// The secret and the public key of the other key.
pub fn other_secret() -> String {
    format!("csk_{}", "3".repeat(43))
}

pub fn other_public() -> String {
    format!("cpk_{}", "3".repeat(43))
}

/// A well-formed secret that no key has.
pub fn unknown_secret() -> String {
    format!("csk_{}", "2".repeat(43))
}

/// `Authorization: Basic` with `user:password`.
pub fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", BASE64.encode(format!("{user}:{password}")))
}

pub fn keys() -> Arc<StaticKeys> {
    let key = |id, project_id, public: String, secret: String| StaticKey {
        id,
        project_id,
        public_key: PublicKey::parse(&public).unwrap(),
        secret: SecretKey::parse(&secret).unwrap(),
    };
    let keys = vec![
        key(KEY_ID, PROJECT, public(), secret()),
        key(
            ApiKeyId::from_bytes([0x02; 16]),
            OTHER_PROJECT,
            other_public(),
            other_secret(),
        ),
    ];
    Arc::new(StaticKeys::new(&ChaChaRng::from_u64(7), keys).unwrap())
}

/// Small segments, no space pressure.
pub fn wal_config() -> WalConfig {
    WalConfig {
        segment_bytes: 1 << 20,
        max_bytes: 1 << 30,
        min_free_bytes: 0,
        ..WalConfig::default()
    }
}

/// The ingest settings with the invented compat routes and routing header.
pub fn compat_config() -> IngestConfig {
    IngestConfig {
        compat_routes: vec![
            CompatRoute {
                path: COMPAT_PATH,
                dialect: COMPAT_DIALECT,
                sdk_headers: Some((SDK_NAME_HEADER, SDK_VERSION_HEADER)),
            },
            CompatRoute {
                path: PLAIN_COMPAT_PATH,
                dialect: PLAIN_COMPAT_DIALECT,
                sdk_headers: None,
            },
        ],
        routing_headers: vec![RoutingHeader {
            name: ROUTING_HEADER,
            dialect: ROUTING_DIALECT,
        }],
        ..IngestConfig::default()
    }
}

pub fn ingest(env: &Env, wal: &Wal, wal_config: &WalConfig, config: IngestConfig) -> Ingest {
    Ingest::new(env, wal.clone(), wal_config, keys(), config)
}

pub fn router(env: &Env, wal: &Wal, wal_config: &WalConfig, config: IngestConfig) -> Router {
    ingest(env, wal, wal_config, config).router()
}

/// A request to `POST /v1/traces`, or to another path.
#[derive(Clone, Debug)]
pub struct Req {
    pub path: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Req {
    /// An authenticated request with a body in `content_type`.
    pub fn new(body: Vec<u8>, content_type: &str) -> Self {
        Self {
            path: "/v1/traces".to_owned(),
            body,
            headers: vec![
                ("authorization".to_owned(), format!("Bearer {}", secret())),
                ("content-type".to_owned(), content_type.to_owned()),
                ("user-agent".to_owned(), USER_AGENT.to_owned()),
            ],
        }
    }

    pub fn protobuf(request: &ExportTraceServiceRequest) -> Self {
        Self::new(request.encode_to_vec(), PROTOBUF)
    }

    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self::new(body.into(), JSON)
    }

    /// The request to another path.
    pub fn to(mut self, path: &str) -> Self {
        path.clone_into(&mut self.path);
        self
    }

    /// Sets a header, replacing any value it had.
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self = self.without(name);
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn without(mut self, name: &str) -> Self {
        self.headers.retain(|(n, _)| n != name);
        self
    }

    /// Compresses the body with gzip.
    pub fn gzip(mut self) -> Self {
        self.body = gzip(&self.body);
        self.with("content-encoding", "gzip")
    }

    /// The request with `Content-Length`, as a client that knows the body's length sends it.
    pub fn build(self) -> Request<Body> {
        let len = self.body.len().to_string();
        self.with("content-length", &len).build_streamed()
    }

    /// The request without `Content-Length`, as a chunked upload sends it.
    pub fn build_streamed(self) -> Request<Body> {
        let mut builder = Request::post(&self.path);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder.body(Body::from(self.body)).unwrap()
    }
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// `google.rpc.Status`, the body of every OTLP/HTTP refusal and the details of a gRPC status.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RpcStatus {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(message, repeated, tag = "3")]
    pub details: Vec<Any>,
}

/// `google.protobuf.Any`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Any {
    #[prost(string, tag = "1")]
    pub type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    pub value: Vec<u8>,
}

/// `google.rpc.RetryInfo`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RetryInfo {
    #[prost(message, optional, tag = "1")]
    pub retry_delay: Option<ProtoDuration>,
}

/// `google.protobuf.Duration`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ProtoDuration {
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    #[prost(int32, tag = "2")]
    pub nanos: i32,
}

/// A response.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap()
    }

    pub fn retry_after(&self) -> Option<u32> {
        self.header("retry-after").map(|v| v.parse().unwrap())
    }

    /// The `ExportTraceServiceResponse` of a protobuf 200.
    pub fn export_response(&self) -> ExportTraceServiceResponse {
        assert_eq!(self.status, StatusCode::OK, "{self:?}");
        assert_eq!(self.content_type(), PROTOBUF);
        ExportTraceServiceResponse::decode(self.body.as_ref()).unwrap()
    }

    /// The `rejected_spans` of a protobuf 200; zero when the response has no partial success.
    pub fn rejected_spans(&self) -> i64 {
        self.export_response()
            .partial_success
            .map_or(0, |p| p.rejected_spans)
    }

    /// The `google.rpc.Status` of a protobuf refusal.
    pub fn rpc_status(&self) -> RpcStatus {
        assert_eq!(self.content_type(), PROTOBUF);
        RpcStatus::decode(self.body.as_ref()).unwrap()
    }

    pub fn json(&self) -> serde_json::Value {
        assert_eq!(self.content_type(), JSON);
        serde_json::from_slice(&self.body).unwrap()
    }
}

pub async fn send_request(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

pub async fn send(router: &Router, req: Req) -> Reply {
    send_request(router, req.build()).await
}

/// The generated OTLP client over the gRPC router itself, without a transport.
pub fn grpc_client(router: &Router) -> TraceServiceClient<Router> {
    TraceServiceClient::new(router.clone())
}

/// A gRPC request with the metadata of an OTLP exporter: `authorization` (when given) and `user-agent`.
pub fn grpc_request<T>(message: T, authorization: Option<&str>) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    let metadata = request.metadata_mut();
    if let Some(value) = authorization {
        metadata.insert("authorization", MetadataValue::try_from(value).unwrap());
    }
    metadata.insert("user-agent", MetadataValue::from_static(USER_AGENT));
    request
}

/// Exports `message` with the test key and returns the response or the status.
pub async fn grpc_export(
    router: &Router,
    message: ExportTraceServiceRequest,
) -> Result<ExportTraceServiceResponse, tonic::Status> {
    let bearer = format!("Bearer {}", secret());
    grpc_client(router)
        .export(grpc_request(message, Some(&bearer)))
        .await
        .map(tonic::Response::into_inner)
}

/// Message bytes as they are, for requests the generated client cannot build.
#[derive(Debug, Clone, Copy)]
pub struct RawCodec;

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = Self;
    type Decoder = Self;

    fn encoder(&mut self) -> Self {
        Self
    }

    fn decoder(&mut self) -> Self {
        Self
    }
}

impl Encoder for RawCodec {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), tonic::Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawCodec {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, tonic::Status> {
        Ok(Some(src.copy_to_bytes(src.remaining()).to_vec()))
    }
}

/// Exports raw message bytes to `path` with the test key, gzip-compressed when `compress`.
pub async fn grpc_raw(
    router: &Router,
    path: &'static str,
    message: Vec<u8>,
    compress: bool,
) -> Result<Vec<u8>, tonic::Status> {
    let mut client = tonic::client::Grpc::new(router.clone());
    if compress {
        client = client.send_compressed(CompressionEncoding::Gzip);
    }
    client.ready().await.unwrap();
    let bearer = format!("Bearer {}", secret());
    let path = http::uri::PathAndQuery::from_static(path);
    client
        .unary(grpc_request(message, Some(&bearer)), path, RawCodec)
        .await
        .map(tonic::Response::into_inner)
}

/// The path of the export method.
pub const EXPORT: &str = "/opentelemetry.proto.collector.trace.v1.TraceService/Export";

/// The seconds of the `RetryInfo` in a status's details, after checking that the details repeat the status.
pub fn retry_after(status: &tonic::Status) -> Option<i64> {
    if status.details().is_empty() {
        return None;
    }
    let details = RpcStatus::decode(status.details()).unwrap();
    assert_eq!(details.code, status.code() as i32);
    assert_eq!(details.message, status.message());
    let [any] = details.details.as_slice() else {
        panic!("one detail expected: {details:?}");
    };
    assert_eq!(any.type_url, "type.googleapis.com/google.rpc.RetryInfo");
    let delay = RetryInfo::decode(any.value.as_slice())
        .unwrap()
        .retry_delay
        .unwrap();
    assert_eq!(delay.nanos, 0);
    Some(delay.seconds)
}

/// The uncompressed payload of every durable frame, in LSN order. Every frame must be an `IngestRequest`.
pub async fn payloads(wal: &Wal) -> Vec<Vec<u8>> {
    let mut replay = wal.replay(Lsn::new(1));
    let mut out = Vec::new();
    while let Some(frame) = replay.next().await.unwrap() {
        assert_eq!(frame.record_type(), RecordType::IngestRequest);
        out.push(frame.payload().unwrap().into_owned());
    }
    out
}

/// Decodes an `IngestRequest` payload, checking that it encodes back to the same bytes.
pub fn envelope(payload: &[u8]) -> IngestRequest<'_> {
    let record = Record::decode(RecordType::IngestRequest, payload).unwrap();
    assert_eq!(record.encode().unwrap(), payload, "codec round trip");
    match record {
        Record::IngestRequest(request) => request,
        other => panic!("not an ingest request: {other:?}"),
    }
}

/// The bits set in a `rejected` bitmap, least-significant bit first.
pub fn rejected_ordinals(bitmap: &[u8]) -> Vec<u32> {
    (0..bitmap.len() as u32 * 8)
        .filter(|i| bitmap[*i as usize / 8] & (1 << (i % 8)) != 0)
        .collect()
}

pub fn string(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_owned())),
        }),
        ..KeyValue::default()
    }
}

/// A span with the given ids that starts at `start_nanos`.
pub fn span(trace_id: Vec<u8>, span_id: Vec<u8>, start_nanos: u64, name: &str) -> Span {
    Span {
        trace_id,
        span_id,
        name: name.to_owned(),
        kind: 3,
        start_time_unix_nano: start_nanos,
        end_time_unix_nano: start_nanos + 1_500_000,
        attributes: vec![string("gen_ai.request.model", "model-a")],
        ..Span::default()
    }
}

/// A span with valid ids derived from `n`.
pub fn good_span(n: u8, start_nanos: u64) -> Span {
    span(vec![n | 1; 16], vec![n | 1; 8], start_nanos, "call")
}

/// One resource and one scope holding `spans`.
pub fn export(spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string("service.name", "checkout")],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "agent".to_owned(),
                    version: "1.0".to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans,
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
}

/// The envelope fields every test request should produce, apart from the body-specific ones.
pub fn assert_common_envelope(request: &IngestRequest<'_>) {
    assert_eq!(request.project_id, PROJECT);
    assert_eq!(request.api_key_id, KEY_ID);
    assert_eq!(request.compat_dialect, 0);
    assert_eq!(request.transport, crucible_wal_codec::IngestTransport::Http);
    assert!(!request.admin_import);
    assert_eq!(request.sdk_name, "OTel-OTLP-Exporter-Rust");
    assert_eq!(request.sdk_version, "0.31.0");
    assert_eq!(request.idempotency_key, b"");
}
