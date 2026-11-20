//! Operations to OTLP: one span per [`Op`], with the OTel MCP semantic-convention keys and the
//! `crucible.mcp.*` keys of contract 9.1. Pure.

use crucible_records::{
    CancelSource, CaptureSource, Era, ErrorClass, McpStatus, ResultType, Transport,
};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message as _;

use crate::core::{Op, error_type};

/// What every span of one proxy process shares.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResourceInfo {
    /// `service.name`: the name the operator gave the proxied server.
    pub service_name: String,
    /// `service.instance.id`: random per proxy process.
    pub instance_id: String,
    /// `crucible.deploy.id`, when the operator set one.
    pub deploy_id: Option<String>,
    /// `proxy_stdio` or `proxy_http`.
    pub capture_source: CaptureSource,
}

fn value(value: Value) -> Option<AnyValue> {
    Some(AnyValue { value: Some(value) })
}

fn text(key: &str, text: impl Into<String>) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: value(Value::StringValue(text.into())),
        ..KeyValue::default()
    }
}

fn int(key: &str, number: i64) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: value(Value::IntValue(number)),
        ..KeyValue::default()
    }
}

fn flag(key: &str, flag: bool) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: value(Value::BoolValue(flag)),
        ..KeyValue::default()
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 0x0f)]])
        .map(char::from)
        .collect()
}

fn nanos(micros: i64) -> u64 {
    u64::try_from(micros).unwrap_or(0).saturating_mul(1_000)
}

/// The attributes of one operation.
fn attributes(op: &Op, capture_source: CaptureSource) -> Vec<KeyValue> {
    let http = op.transport == Transport::StreamableHttp;
    let mut out = vec![
        text("mcp.method.name", op.method.clone()),
        text("crucible.mcp.capture_source", capture_source.as_str()),
        text("crucible.mcp.transport", op.transport.as_str()),
        text("network.transport", if http { "tcp" } else { "pipe" }),
        text("crucible.mcp.direction", op.direction.as_str()),
        flag("crucible.mcp.is_notification", op.is_notification),
        flag("crucible.mcp.is_batch", op.is_batch),
        flag("crucible.mcp.content.captured", false),
    ];
    let mut push_text = |key: &str, text_value: &str| {
        if !text_value.is_empty() {
            out.push(text(key, text_value));
        }
    };
    if let Some(id) = &op.jsonrpc_id {
        push_text("jsonrpc.request.id", id);
    }
    match op.method.as_str() {
        "tools/call" => push_text("gen_ai.tool.name", &op.target),
        "prompts/get" => push_text("gen_ai.prompt.name", &op.target),
        _ => {}
    }
    push_text("mcp.protocol.version", &op.protocol_version);
    if let Some(era) = op.era {
        push_text("crucible.mcp.era", era.as_str());
    }
    // A session id exists only in the legacy era (contract 9.4).
    if op.era != Some(Era::Modern)
        && let Some(key) = &op.session_key
    {
        push_text("mcp.session.id", key);
    }
    push_text("crucible.mcp.client.name", &op.client_name);
    push_text("crucible.mcp.client.version", &op.client_version);
    push_text("crucible.mcp.server.name", &op.server_name);
    push_text("crucible.mcp.server.version", &op.server_version);
    push_text("user_agent.original", &op.user_agent);
    push_text("crucible.mcp.client.ip_prefix", &op.ip_prefix);
    if let Some(error) = error_type(op) {
        push_text("error.type", &error);
    }
    if let Some(source) = op.identity.source {
        push_text("crucible.mcp.identity.source", source.as_str());
        push_text("crucible.mcp.user.issuer", &op.identity.issuer);
        push_text("crucible.mcp.user.subject", &op.identity.subject);
        push_text("crucible.mcp.oauth.client_id", &op.identity.client_id);
        push_text("crucible.mcp.tenant", &op.identity.tenant);
    }
    if op.classification.error_class != ErrorClass::None {
        out.push(text(
            "crucible.mcp.error.class",
            op.classification.error_class.as_str(),
        ));
    }
    if let Some(code) = op.jsonrpc_code {
        out.push(text("rpc.response.status_code", code.to_string()));
    }
    if op.cancel_source != CancelSource::None {
        out.push(text(
            "crucible.mcp.cancel.source",
            op.cancel_source.as_str(),
        ));
    }
    if op.result_type != ResultType::None {
        out.push(text("crucible.mcp.result_type", op.result_type.as_str()));
    }
    // The keyed hashes of contract 9.6, so that readers can chain the rounds of one exchange.
    for (key, hash) in [
        ("crucible.mcp.mrtr.state_in", op.mrtr_state_in),
        ("crucible.mcp.mrtr.state_out", op.mrtr_state_out),
    ] {
        if let Some(hash) = hash {
            out.push(text(key, hex(&hash)));
        }
    }
    if let Some(status) = op.http_status {
        out.push(int("http.response.status_code", i64::from(status)));
    }
    if op.request_bytes > 0 {
        out.push(int(
            "crucible.mcp.request.bytes",
            i64::try_from(op.request_bytes).unwrap_or(i64::MAX),
        ));
    }
    if op.response_bytes > 0 {
        out.push(int(
            "crucible.mcp.response.bytes",
            i64::try_from(op.response_bytes).unwrap_or(i64::MAX),
        ));
    }
    if op.jsonrpc_id.is_some() {
        // The proxy sees every progress notification, so the count is always observed.
        out.push(int(
            "crucible.mcp.progress.count",
            i64::from(op.progress_count),
        ));
    }
    for (kind, count) in &op.content_blocks {
        out.push(int(
            &format!("crucible.mcp.content.blocks.{kind}"),
            i64::from(*count),
        ));
    }
    out
}

fn span(op: &Op, capture_source: CaptureSource) -> Span {
    let start = nanos(op.start.micros());
    let duration = u64::try_from(op.duration.as_nanos()).unwrap_or(u64::MAX);
    let failed = op.classification.status != McpStatus::Ok;
    Span {
        trace_id: op.trace_id.to_bytes().to_vec(),
        span_id: op.span_id.to_bytes().to_vec(),
        parent_span_id: op
            .parent_span_id
            .map(|id| id.to_bytes().to_vec())
            .unwrap_or_default(),
        name: if op.target.is_empty() {
            op.method.clone()
        } else {
            format!("{} {}", op.method, op.target)
        },
        // The proxy stands in front of the server, so every row is a server-side row (contract 9.5).
        kind: SpanKind::Server as i32,
        start_time_unix_nano: start,
        end_time_unix_nano: start.saturating_add(duration),
        attributes: attributes(op, capture_source),
        status: failed.then(|| Status {
            message: op.error_message.clone(),
            code: StatusCode::Error as i32,
        }),
        ..Span::default()
    }
}

/// The body of one `POST /v1/traces`: an `ExportTraceServiceRequest` in protobuf.
pub fn encode(resource: &ResourceInfo, ops: &[Op]) -> Vec<u8> {
    let mut attributes = vec![
        text("service.name", resource.service_name.clone()),
        text("service.instance.id", resource.instance_id.clone()),
        text("telemetry.sdk.name", "crucible-proxy"),
        text("telemetry.sdk.language", "rust"),
        text("telemetry.sdk.version", env!("CARGO_PKG_VERSION")),
    ];
    if let Some(deploy) = &resource.deploy_id {
        attributes.push(text("crucible.deploy.id", deploy.clone()));
    }
    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes,
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "crucible-proxy".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans: ops
                    .iter()
                    .map(|op| span(op, resource.capture_source))
                    .collect(),
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    };
    request.encode_to_vec()
}
