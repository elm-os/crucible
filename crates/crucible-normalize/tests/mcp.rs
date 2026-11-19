//! The MCP projector and stage B's part of an MCP operation (contracts 3.5, 3.6 and 9.1 to 9.8): which spans
//! yield an operation, how each key of the wire contract fills its field, the error taxonomy as spans carry it,
//! the group key, multi-round state, deploy and handle ids, capture sources, opt-in content, the tool inventory,
//! what is never stored, and the link between an agent's tool call and the server's operation.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{frame, int, kv, kvlist, prices, request_with, s, span_with};
use crucible_normalize::{
    IdentityCtx, NormCtx, NormalizedFrame, ProjectSalt, StageB, normalize, row_digest,
};
use crucible_records::{
    CancelSource, CaptureSource, Direction, Era, ErrorClass, ErrorSubclass, GroupKeyKind,
    IdentitySource, Lsn, McpClassification, McpErrorAnnotations, McpErrorInput, McpOpRecord,
    McpSide, McpStatus, McpTargetKind, PayloadCaps, ResultType, SpanId, TraceId, Transport,
    Truncation, caps, classify_mcp_error, mcp_keys as keys,
};
use hmac::{Hmac, KeyInit, Mac};
use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::Span;
use opentelemetry_proto::tonic::trace::v1::span::Link;
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message;
use sha2::Sha256;

const SERVER: i32 = 2;
const CLIENT: i32 = 3;

fn flag(key: &str, value: bool) -> KeyValue {
    kv(key, any_value::Value::BoolValue(value))
}

fn double(key: &str, value: f64) -> KeyValue {
    kv(key, any_value::Value::DoubleValue(value))
}

fn ctx() -> NormCtx<'static> {
    NormCtx::new(prices(), PayloadCaps::DEFAULT)
}

/// A span of the given kind in trace `t`.
fn span(kind: i32, t: u8, id: u8, parent: Option<u8>, attributes: Vec<KeyValue>) -> Span {
    Span {
        kind,
        ..span_with(t, id, parent, attributes)
    }
}

/// A server-side `tools/call` of `echo` with these further attributes.
fn call(id: u8, mut attributes: Vec<KeyValue>) -> Span {
    attributes.insert(0, s(keys::METHOD, "tools/call"));
    attributes.insert(1, s(keys::TOOL_NAME, "echo"));
    span(SERVER, 1, id, None, attributes)
}

fn stage_a(resource: Vec<KeyValue>, spans: Vec<Span>) -> NormalizedFrame {
    let body = request_with(resource, spans).encode_to_vec();
    normalize(&frame(&body, &[]), &ctx()).unwrap()
}

/// Stages A and B of one request, with what stage B knows of the project's salt.
fn stages(
    resource: Vec<KeyValue>,
    spans: Vec<Span>,
    identity: &IdentityCtx<'_>,
) -> NormalizedFrame {
    let mut out = stage_a(resource, spans);
    StageB::default().admit(&mut out, identity).unwrap();
    out
}

/// The operation of the only span of a request with the resource `service.name = svc`.
fn op_of(span: Span) -> McpOpRecord {
    let out = stages(
        vec![s("service.name", "svc")],
        vec![span],
        &IdentityCtx::Salted(None),
    );
    assert_eq!(out.mcp_ops.len(), 1, "one operation");
    out.mcp_ops[0].record.clone()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_span_is_an_operation_if_and_only_if_it_carries_the_method() {
    let out = stage_a(
        vec![s("service.name", "svc"), s("service.version", "1.4.0")],
        vec![
            // Every other key of the contract, and no method.
            span_with(
                1,
                1,
                None,
                vec![
                    s(keys::TOOL_NAME, "echo"),
                    s(keys::CAPTURE_SOURCE, "sdk"),
                    s(keys::ERROR_TYPE, "tool_error"),
                    s(keys::JSONRPC_ID, "1"),
                ],
            ),
            // The method alone, on a span that is neither a server nor a client span.
            span_with(1, 2, None, vec![s(keys::METHOD, "tools/list")]),
        ],
    );
    assert_eq!(out.spans.len(), 2);
    assert_eq!(out.spans[0].core.mcp_side, McpSide::None);
    assert_eq!(out.spans[1].core.mcp_side, McpSide::Server);
    assert_eq!(out.mcp_ops.len(), 1);
    assert_eq!(out.mcp_ops[0].span, 1);
    // A span without an operation keeps its keys and the digest of its row.
    let residual = core::str::from_utf8(&out.spans[0].payload.attributes).unwrap();
    assert!(residual.contains(keys::ERROR_TYPE) && residual.contains(keys::JSONRPC_ID));
    assert_eq!(out.spans[0].core.row_digest, row_digest(&out.spans[0]));
    assert_ne!(out.spans[1].core.row_digest, row_digest(&out.spans[1]));

    // What an absent key means (contract 9.1): a plain semantic-convention span.
    let r = &out.mcp_ops[0].record;
    let c = &out.spans[1].core;
    assert_eq!(
        (r.project_id, r.trace_id, r.span_id),
        (c.project_id, c.trace_id, c.span_id)
    );
    assert_eq!((r.start_time, r.duration_us), (c.start_time, 250_000));
    assert_eq!(r.ingestion, c.ingestion);
    assert_eq!(r.normalizer_version, c.normalizer_version);
    assert_eq!(r.side, McpSide::Server);
    assert_eq!(r.capture_source, CaptureSource::Otlp);
    assert_eq!(&*r.method, "tools/list");
    assert_eq!((r.target_kind, &*r.target_name), (McpTargetKind::None, ""));
    assert_eq!((r.era, &*r.protocol_version), (Era::Legacy, ""));
    assert_eq!(r.transport, Transport::Unknown);
    assert_eq!(&*r.jsonrpc_id, "");
    assert_eq!(r.direction, Direction::ClientToServer);
    assert!(!r.is_notification && !r.is_batch && !r.timed_out && !r.caller_fault);
    assert_eq!((r.status, r.error_class), (McpStatus::Ok, ErrorClass::None));
    assert_eq!(r.error_subclass, ErrorSubclass::None);
    assert_eq!(
        (r.jsonrpc_code, r.http_status, r.progress_count),
        (None, None, None)
    );
    assert_eq!(r.result_type, ResultType::Complete);
    assert_eq!(
        (r.mrtr_round, r.mrtr_state_in, r.mrtr_state_out),
        (0, None, None)
    );
    assert_eq!(r.cancel_source, CancelSource::None);
    assert_eq!(
        (
            &*r.client_name,
            &*r.client_version,
            r.client_capabilities_hash
        ),
        ("", "", 0)
    );
    assert_eq!(r.identity_source, IdentitySource::None);
    assert_eq!((r.user_subject_hash, r.session_key), (None, None));
    assert_eq!((r.request_bytes, r.response_bytes), (0, 0));
    assert!(r.content_blocks.is_empty() && !r.content_captured);
    assert_eq!(r.content_sample_rate, 0.0);
    assert!(r.args.is_empty() && r.result.is_empty());
    assert_eq!((&*r.server_name, &*r.server_version), ("svc", "1.4.0"));
    // Contract 9.7: the deploy id defaults to the server version.
    assert_eq!((&*r.deploy_id, &*r.instance_id), ("1.4.0", ""));
    assert!(r.transport_link_trace_id.is_zero() && r.transport_link_span_id.is_zero());
    assert_eq!(
        (r.group_key_kind, &*r.group_key),
        (GroupKeyKind::Process, "")
    );
}

#[test]
fn every_key_of_the_wire_contract_fills_its_field() {
    let state_in = "59c50f1c773bc1354d420d974c06c5af";
    let state_out = "bec039d76aab792bd526d369321fb0c0";
    let mut call = span(
        SERVER,
        1,
        2,
        Some(1),
        vec![
            s(keys::METHOD, "tools/call"),
            s(keys::TOOL_NAME, "lookup_order"),
            s(keys::CAPTURE_SOURCE, "proxy_http"),
            s(keys::PROTOCOL_VERSION, "2026-07-28"),
            s(keys::TRANSPORT, "sse"),
            s(keys::NETWORK_TRANSPORT, "pipe"),
            s(keys::JSONRPC_ID, "req-7"),
            s(keys::DIRECTION, "server_to_client"),
            flag(keys::IS_BATCH, true),
            s(keys::RESULT_TYPE, "input_required"),
            int(keys::MRTR_ROUND, 2),
            s(keys::MRTR_STATE_IN, state_in),
            s(keys::MRTR_STATE_OUT, state_out),
            s(keys::TASK_ID, "task-1"),
            s(keys::SUBSCRIPTION_ID, "sub-1"),
            int(keys::PROGRESS_COUNT, 3),
            s(keys::CLIENT_NAME, "inspector"),
            s(keys::CLIENT_VERSION, "0.9.1"),
            int(keys::CLIENT_CAPABILITIES_HASH, -2),
            s(keys::OAUTH_CLIENT_ID, "client-9"),
            s(keys::TENANT, "acme"),
            s(keys::USER_ID, "user-7"),
            s(keys::HANDLE_ID, "ord_1042"),
            s(keys::USER_ISSUER, "https://issuer.example"),
            s(keys::USER_SUBJECT, "subject-1"),
            s(keys::IDENTITY_SOURCE, "token_verified"),
            int(keys::REQUEST_BYTES, 120),
            int(keys::RESPONSE_BYTES, 4_096),
            int("crucible.mcp.content.blocks.text", 2),
            int("crucible.mcp.content.blocks.image", 1),
            s(keys::TOOL_ARGUMENTS, r#"{"order":"ord_1042"}"#),
            s(keys::TOOL_RESULT, r#"{"status":"shipped"}"#),
            flag(keys::CONTENT_CAPTURED, true),
            double(keys::CONTENT_SAMPLE_RATE, 0.25),
            s(keys::CONTENT_TRUNCATION, "cap_exceeded"),
            s(keys::SERVER_NAME, "orders"),
            s(keys::SERVER_VERSION, "2.1.0"),
            s(keys::CLIENT_IP_PREFIX, "203.0.113.0/24"),
            s(keys::USER_AGENT, "inspector/0.9"),
            int(keys::HTTP_STATUS, 200),
            s("unrelated", "kept"),
        ],
    );
    call.links = vec![Link {
        trace_id: vec![9; 16],
        span_id: vec![8; 8],
        ..Link::default()
    }];
    let salt = ProjectSalt::new([3; 32]);
    let out = stages(
        vec![
            s("service.name", "svc"),
            s("service.version", "1.4.0"),
            s(keys::RESOURCE_DEPLOY_ID, "deploy-42"),
            s(keys::RESOURCE_INSTANCE_ID, "pod-3"),
        ],
        vec![call],
        &IdentityCtx::Salted(Some(&salt)),
    );
    let r = &out.mcp_ops[0].record;
    assert_eq!(r.parent_span_id, SpanId::from_bytes([1; 8]));
    assert_eq!(r.transport_link_trace_id, TraceId::from_bytes([9; 16]));
    assert_eq!(r.transport_link_span_id, SpanId::from_bytes([8; 8]));
    assert_eq!((&*r.server_name, &*r.server_version), ("orders", "2.1.0"));
    assert_eq!((&*r.deploy_id, &*r.instance_id), ("deploy-42", "pod-3"));
    assert_eq!(r.side, McpSide::Server);
    assert_eq!(r.capture_source, CaptureSource::ProxyHttp);
    assert_eq!((r.era, &*r.protocol_version), (Era::Modern, "2026-07-28"));
    // The Crucible key wins over `network.transport`.
    assert_eq!(r.transport, Transport::Sse);
    assert_eq!(
        (r.target_kind, &*r.target_name),
        (McpTargetKind::Tool, "lookup_order")
    );
    assert_eq!(&*r.jsonrpc_id, "req-7");
    assert_eq!(r.direction, Direction::ServerToClient);
    assert!(r.is_batch && !r.is_notification);
    assert_eq!(
        (r.result_type, r.mrtr_round),
        (ResultType::InputRequired, 2)
    );
    assert_eq!(r.mrtr_state_in.map(|h| hex(&h)).as_deref(), Some(state_in));
    assert_eq!(
        r.mrtr_state_out.map(|h| hex(&h)).as_deref(),
        Some(state_out)
    );
    assert_eq!((&*r.task_id, &*r.subscription_id), ("task-1", "sub-1"));
    assert_eq!(r.progress_count, Some(3));
    assert_eq!(
        (&*r.client_name, &*r.client_version),
        ("inspector", "0.9.1")
    );
    assert_eq!(r.client_capabilities_hash, u64::MAX - 1);
    assert_eq!(
        (&*r.oauth_client_id, &*r.tenant, &*r.user_id),
        ("client-9", "acme", "user-7")
    );
    assert_eq!(&*r.handle_id, "ord_1042");
    assert_eq!(&*r.user_issuer, "https://issuer.example");
    assert_eq!(
        r.user_subject_hash,
        Some(salt.subject_hash("https://issuer.example", "subject-1"))
    );
    assert_eq!(r.identity_source, IdentitySource::TokenVerified);
    // Modern era: no session key, so the handle is the group key (contract 9.4).
    assert_eq!(r.session_key, None);
    assert_eq!(
        (r.group_key_kind, &*r.group_key),
        (GroupKeyKind::Handle, "ord_1042")
    );
    assert_eq!(
        (&*r.ip_prefix, &*r.user_agent),
        ("203.0.113.0/24", "inspector/0.9")
    );
    assert_eq!((r.request_bytes, r.response_bytes), (120, 4_096));
    let blocks: Vec<(&str, u32)> = r.content_blocks.iter().map(|(k, n)| (&**k, *n)).collect();
    assert_eq!(blocks, [("image", 1), ("text", 2)]);
    assert!(r.content_captured);
    assert_eq!(r.content_sample_rate, 0.25);
    assert_eq!(r.content_truncation, Truncation::CapExceeded);
    assert_eq!(&r.args[..], br#"{"order":"ord_1042"}"#);
    assert_eq!(&r.result[..], br#"{"status":"shipped"}"#);
    assert_eq!((r.args_bytes, r.result_bytes), (20, 20));
    assert_eq!(r.http_status, Some(200));
    assert_eq!((r.status, r.error_class), (McpStatus::Ok, ErrorClass::None));

    // Consumed keys leave the residual attributes (contract 9.1); a key nobody reads stays.
    let residual = core::str::from_utf8(&out.spans[0].payload.attributes).unwrap();
    assert_eq!(residual, r#"{"unrelated":"kept"}"#);
    let resource = core::str::from_utf8(&out.spans[0].payload.resource_attributes).unwrap();
    assert!(resource.contains("deploy-42") && resource.contains("pod-3"));
}

#[test]
fn side_era_transport_and_target_follow_their_fallbacks() {
    // SpanKind decides the side; the key only when the kind is neither server nor client.
    let client = op_of(span(
        CLIENT,
        1,
        1,
        None,
        vec![s(keys::METHOD, "tools/call"), s(keys::SIDE, "server")],
    ));
    assert_eq!(client.side, McpSide::Client);
    let keyed = op_of(span_with(
        1,
        1,
        None,
        vec![s(keys::METHOD, "tools/call"), s(keys::SIDE, "client")],
    ));
    assert_eq!(keyed.side, McpSide::Client);

    for (version, era) in [
        ("2025-11-25", Era::Legacy),
        ("2026-07-27", Era::Legacy),
        ("2026-07-28", Era::Modern),
        ("2027-01-01", Era::Modern),
    ] {
        let r = op_of(call(1, vec![s(keys::PROTOCOL_VERSION, version)]));
        assert_eq!(r.era, era, "{version}");
    }
    // The era key wins over the version.
    let r = op_of(call(
        1,
        vec![
            s(keys::PROTOCOL_VERSION, "2026-07-28"),
            s(keys::ERA, "legacy"),
        ],
    ));
    assert_eq!(r.era, Era::Legacy);

    for (network, transport) in [
        ("pipe", Transport::Stdio),
        ("tcp", Transport::StreamableHttp),
        ("udp", Transport::Unknown),
    ] {
        let r = op_of(call(1, vec![s(keys::NETWORK_TRANSPORT, network)]));
        assert_eq!(r.transport, transport, "{network}");
    }

    for (method, key, kind) in [
        ("prompts/get", keys::PROMPT_NAME, McpTargetKind::Prompt),
        (
            "resources/read",
            keys::RESOURCE_TEMPLATE,
            McpTargetKind::Resource,
        ),
        (
            "resources/subscribe",
            keys::RESOURCE_TEMPLATE,
            McpTargetKind::Resource,
        ),
    ] {
        let r = op_of(span(
            SERVER,
            1,
            1,
            None,
            vec![
                s(keys::METHOD, method),
                s(key, "the-target"),
                s(keys::TOOL_NAME, "not-this"),
            ],
        ));
        assert_eq!(
            (r.target_kind, &*r.target_name),
            (kind, "the-target"),
            "{method}"
        );
    }
    let r = op_of(span(
        SERVER,
        1,
        1,
        None,
        vec![s(keys::METHOD, "tools/call")],
    ));
    assert_eq!((r.target_kind, &*r.target_name), (McpTargetKind::Tool, ""));
    let r = op_of(span(
        SERVER,
        1,
        1,
        None,
        vec![s(keys::METHOD, "ping"), s(keys::TOOL_NAME, "echo")],
    ));
    assert_eq!((r.target_kind, &*r.target_name), (McpTargetKind::None, ""));

    // A JSON-RPC id may be a number.
    assert_eq!(
        &*op_of(call(1, vec![int(keys::JSONRPC_ID, 42)])).jsonrpc_id,
        "42"
    );
}

/// One row of the taxonomy as a span carries it: the attributes, then the expected class, status and stored code.
struct Row {
    attributes: Vec<KeyValue>,
    class: ErrorClass,
    status: McpStatus,
    code: Option<i32>,
}

fn row(attributes: Vec<KeyValue>, class: ErrorClass, status: McpStatus, code: Option<i32>) -> Row {
    Row {
        attributes,
        class,
        status,
        code,
    }
}

fn rpc(code: i32) -> Vec<KeyValue> {
    vec![
        s(keys::ERROR_TYPE, &code.to_string()),
        s(keys::RPC_STATUS_CODE, &code.to_string()),
    ]
}

#[test]
fn the_taxonomy_of_contract_9_2_row_by_row() {
    use ErrorClass as C;
    use McpStatus as S;
    let with = |mut attributes: Vec<KeyValue>, more: Vec<KeyValue>| {
        attributes.extend(more);
        attributes
    };
    let rows = vec![
        // 1: cancelled, before anything else that also holds.
        row(
            with(
                rpc(-32603),
                vec![
                    s(keys::CANCEL_SOURCE, "notification"),
                    flag(keys::TIMED_OUT, true),
                ],
            ),
            C::Cancelled,
            S::Cancelled,
            Some(-32603),
        ),
        row(
            vec![s(keys::CANCEL_SOURCE, "stream_close")],
            C::Cancelled,
            S::Cancelled,
            None,
        ),
        // 2: a deadline.
        row(
            vec![flag(keys::TIMED_OUT, true), int(keys::HTTP_STATUS, 401)],
            C::Timeout,
            S::TransportError,
            None,
        ),
        row(
            vec![s(keys::ERROR_CLASS, "timeout")],
            C::Timeout,
            S::TransportError,
            None,
        ),
        // 3: the transport failed, which only the capture source can say.
        row(
            vec![
                s(keys::ERROR_CLASS, "transport"),
                s(keys::ERROR_TYPE, "transport"),
            ],
            C::Transport,
            S::TransportError,
            None,
        ),
        // 4 and 5: HTTP statuses, before a JSON-RPC code.
        row(
            with(rpc(-32603), vec![int(keys::HTTP_STATUS, 401)]),
            C::Auth,
            S::ProtocolError,
            Some(-32603),
        ),
        row(
            vec![int(keys::HTTP_STATUS, 403)],
            C::Auth,
            S::ProtocolError,
            None,
        ),
        row(
            vec![int(keys::HTTP_STATUS, 429)],
            C::RateLimit,
            S::ProtocolError,
            None,
        ),
        // A proxy reports an HTTP failure with the status as its `error.type`: that is no JSON-RPC code.
        row(
            vec![
                int(keys::HTTP_STATUS, 401),
                s(keys::ERROR_TYPE, "401"),
                s(keys::ERROR_CLASS, "auth"),
            ],
            C::Auth,
            S::ProtocolError,
            None,
        ),
        // 6: protocol codes.
        row(rpc(-32700), C::Protocol, S::ProtocolError, Some(-32700)),
        row(rpc(-32600), C::Protocol, S::ProtocolError, Some(-32600)),
        row(rpc(-32601), C::Protocol, S::ProtocolError, Some(-32601)),
        row(rpc(-32020), C::Protocol, S::ProtocolError, Some(-32020)),
        row(rpc(-32021), C::Protocol, S::ProtocolError, Some(-32021)),
        row(rpc(-32022), C::Protocol, S::ProtocolError, Some(-32022)),
        // 7: not found, by code or by the annotation.
        row(rpc(-32002), C::NotFound, S::ProtocolError, Some(-32002)),
        row(
            with(rpc(-32602), vec![s(keys::ERROR_CLASS, "not_found")]),
            C::NotFound,
            S::ProtocolError,
            Some(-32602),
        ),
        // 8: invalid params.
        row(rpc(-32602), C::Validation, S::ProtocolError, Some(-32602)),
        // 9: any other code, a code only in `error.type`, and an exception nothing reported.
        row(rpc(-32603), C::Internal, S::ProtocolError, Some(-32603)),
        row(rpc(-1), C::Internal, S::ProtocolError, Some(-1)),
        row(
            vec![s(keys::ERROR_TYPE, "-32000")],
            C::Internal,
            S::ProtocolError,
            Some(-32000),
        ),
        row(
            vec![s(keys::ERROR_TYPE, "TypeError")],
            C::Internal,
            S::ProtocolError,
            None,
        ),
        // 10 to 12: `isError`, with and without the annotations.
        row(
            vec![
                s(keys::ERROR_TYPE, "tool_error"),
                s(keys::ERROR_CLASS, "validation"),
            ],
            C::Validation,
            S::ToolError,
            None,
        ),
        row(
            vec![
                s(keys::ERROR_TYPE, "tool_error"),
                s(keys::ERROR_CLASS, "not_found"),
            ],
            C::NotFound,
            S::ToolError,
            None,
        ),
        row(
            vec![s(keys::ERROR_TYPE, "tool_error")],
            C::ToolExecution,
            S::ToolError,
            None,
        ),
        // 13: any other HTTP failure without a JSON-RPC body.
        row(
            vec![int(keys::HTTP_STATUS, 500)],
            C::Internal,
            S::ProtocolError,
            None,
        ),
        row(
            vec![int(keys::HTTP_STATUS, 502), s(keys::ERROR_TYPE, "502")],
            C::Internal,
            S::ProtocolError,
            None,
        ),
        // 14: none of the above; an explicit class that does not fit the channel changes nothing.
        row(Vec::new(), C::None, S::Ok, None),
        row(vec![int(keys::HTTP_STATUS, 200)], C::None, S::Ok, None),
        row(
            vec![s(keys::ERROR_CLASS, "tool_execution")],
            C::None,
            S::Ok,
            None,
        ),
        row(
            with(rpc(-32603), vec![s(keys::ERROR_CLASS, "tool_execution")]),
            C::Internal,
            S::ProtocolError,
            Some(-32603),
        ),
    ];
    for (at, row) in rows.into_iter().enumerate() {
        let r = op_of(call(1, row.attributes));
        assert_eq!(
            (r.error_class, r.status, r.jsonrpc_code),
            (row.class, row.status, row.code),
            "row {at}"
        );
        // Contract 9.1: `complete` for a successful request, else `none`.
        let expected = if row.class == C::None {
            ResultType::Complete
        } else {
            ResultType::None
        };
        assert_eq!(r.result_type, expected, "row {at}");
    }

    // The subclass refines `tool_execution` only, and the error type and message are kept as sent.
    let mut failed = call(
        1,
        vec![
            s(keys::ERROR_TYPE, "tool_error"),
            s(keys::ERROR_SUBCLASS, "upstream"),
        ],
    );
    failed.status = Some(opentelemetry_proto::tonic::trace::v1::Status {
        message: "the upstream said no".to_owned(),
        code: 2,
    });
    let r = op_of(failed);
    assert_eq!(r.error_subclass, ErrorSubclass::Upstream);
    assert_eq!(
        (&*r.error_type, &*r.error_message),
        ("tool_error", "the upstream said no")
    );
    let r = op_of(call(
        1,
        with(rpc(-32603), vec![s(keys::ERROR_SUBCLASS, "upstream")]),
    ));
    assert_eq!(r.error_subclass, ErrorSubclass::None);
    assert_eq!(&*r.error_type, "-32603");
    // A notification has no result.
    let r = op_of(span(
        SERVER,
        1,
        1,
        None,
        vec![
            s(keys::METHOD, "notifications/initialized"),
            flag(keys::IS_NOTIFICATION, true),
        ],
    ));
    assert!(r.is_notification);
    assert_eq!(r.result_type, ResultType::None);
}

#[test]
fn the_caller_fault_rule() {
    for code in [-32700, -32600, -32601, -32602, -32002] {
        let server = op_of(call(1, rpc(code)));
        assert!(server.caller_fault, "{code} on the server side");
        let mut attributes = rpc(code);
        attributes.insert(0, s(keys::METHOD, "tools/call"));
        let client = op_of(span(CLIENT, 1, 1, None, attributes));
        assert!(!client.caller_fault, "{code} on the client side");
    }
    for code in [-32603, -32020, -32000, 0] {
        assert!(!op_of(call(1, rpc(code))).caller_fault, "{code}");
    }
    // The annotation changes the class, not whose fault the call was.
    let r = op_of(call(1, {
        let mut attributes = rpc(-32602);
        attributes.push(s(keys::ERROR_CLASS, "not_found"));
        attributes
    }));
    assert!(r.caller_fault);
    assert_eq!(r.error_class, ErrorClass::NotFound);
    assert!(!op_of(call(1, vec![s(keys::ERROR_TYPE, "tool_error")])).caller_fault);
}

/// What a sender observed, as the keys carry it.
#[derive(Clone, Debug)]
struct Observed {
    client: bool,
    code: Option<i32>,
    is_error: bool,
    http: Option<u16>,
    cancel: CancelSource,
    timed_out: bool,
    class: Option<ErrorClass>,
    subclass: ErrorSubclass,
}

fn observed() -> impl Strategy<Value = Observed> {
    let codes = prop::sample::select(vec![
        -32700, -32600, -32601, -32602, -32603, -32002, -32020, -32021, -32022, -32000, 7,
    ]);
    let https = prop::sample::select(vec![200u16, 202, 400, 401, 403, 404, 429, 500, 503]);
    (
        any::<bool>(),
        prop::option::weighted(0.4, codes),
        any::<bool>(),
        prop::option::weighted(0.4, https),
        prop::sample::select(CancelSource::ALL.to_vec()),
        prop::bool::weighted(0.15),
        prop::option::weighted(0.4, prop::sample::select(ErrorClass::ALL.to_vec())),
        prop::sample::select(ErrorSubclass::ALL.to_vec()),
    )
        .prop_map(
            |(client, code, is_error, http, cancel, timed_out, class, subclass)| Observed {
                client,
                code,
                // A response is an error or a result, never both.
                is_error: is_error && code.is_none(),
                http,
                cancel,
                timed_out,
                class,
                subclass,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// I8 at the normalizer: the record of every exchange is what the shared classifier gives for what the
    /// sender observed.
    #[test]
    fn every_exchange_classifies_as_the_shared_table_says(o in observed()) {
        let mut attributes = vec![s(keys::METHOD, "tools/call")];
        if let Some(code) = o.code {
            attributes.extend(rpc(code));
        } else if o.is_error {
            attributes.push(s(keys::ERROR_TYPE, "tool_error"));
        }
        if let Some(status) = o.http {
            attributes.push(int(keys::HTTP_STATUS, i64::from(status)));
        }
        if o.cancel != CancelSource::None {
            attributes.push(s(keys::CANCEL_SOURCE, o.cancel.as_str()));
        }
        if o.timed_out {
            attributes.push(flag(keys::TIMED_OUT, true));
        }
        if let Some(class) = o.class {
            attributes.push(s(keys::ERROR_CLASS, class.as_str()));
        }
        if o.subclass != ErrorSubclass::None {
            attributes.push(s(keys::ERROR_SUBCLASS, o.subclass.as_str()));
        }
        let side = if o.client { McpSide::Client } else { McpSide::Server };
        let r = op_of(span(if o.client { CLIENT } else { SERVER }, 1, 1, None, attributes));
        let expected: McpClassification = classify_mcp_error(&McpErrorInput {
            era: r.era,
            side,
            jsonrpc_code: o.code,
            is_error: o.is_error,
            http_status: o.http,
            cancel_source: o.cancel,
            timed_out: o.timed_out || o.class == Some(ErrorClass::Timeout),
            transport_failure: o.class == Some(ErrorClass::Transport),
            unhandled_exception: false,
            annotations: McpErrorAnnotations {
                explicit_class: o.class,
                mark_validation: false,
                mark_not_found: false,
                subclass: o.subclass,
            },
        });
        prop_assert_eq!(
            (r.error_class, r.error_subclass, r.status, r.caller_fault),
            (expected.error_class, expected.error_subclass, expected.status, expected.caller_fault)
        );
        prop_assert_eq!((r.jsonrpc_code, r.http_status, r.cancel_source), (o.code, o.http, o.cancel));
        prop_assert_eq!(r.side, side);
        // The caller-fault rule, stated on its own.
        let caller_codes = [-32700, -32600, -32601, -32602, -32002];
        prop_assert_eq!(
            r.caller_fault,
            !o.client && o.code.is_some_and(|code| caller_codes.contains(&code))
        );
    }
}

#[test]
fn the_group_key_is_the_first_rule_that_matches() {
    let raw = "3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5";
    let hashed = "84757436d5e0791bd4a8eaa6f1d05986";
    let legacy = s(keys::PROTOCOL_VERSION, "2025-11-25");
    let modern = s(keys::PROTOCOL_VERSION, "2026-07-28");
    let instance = vec![
        s("service.name", "svc"),
        s(keys::RESOURCE_INSTANCE_ID, "pod-3"),
    ];
    let op = |span: Span| {
        let out = stages(instance.clone(), vec![span], &IdentityCtx::Salted(None));
        out.mcp_ops[0].record.clone()
    };

    // 1: a legacy session, whatever else is there. A raw id is hashed, a hashed one is taken as it is.
    for sent in [raw, hashed] {
        let r = op(span(
            SERVER,
            1,
            2,
            Some(1),
            vec![
                s(keys::METHOD, "tools/call"),
                legacy.clone(),
                s(keys::SESSION_ID, sent),
                s(keys::HANDLE_ID, "ord_1"),
            ],
        ));
        assert_eq!(r.session_key.map(|k| hex(&k)).as_deref(), Some(hashed));
        assert_eq!(
            (r.group_key_kind, &*r.group_key),
            (GroupKeyKind::LegacySession, hashed)
        );
    }
    // A session id counts in the legacy era only.
    let r = op(span(
        SERVER,
        1,
        2,
        Some(1),
        vec![
            s(keys::METHOD, "tools/call"),
            modern.clone(),
            s(keys::SESSION_ID, raw),
            s(keys::HANDLE_ID, "ord_1"),
        ],
    ));
    assert_eq!(r.session_key, None);
    // 2: the handle.
    assert_eq!(
        (r.group_key_kind, &*r.group_key),
        (GroupKeyKind::Handle, "ord_1")
    );
    // 3: a parent, so trace context was propagated.
    let r = op(span(
        SERVER,
        1,
        2,
        Some(1),
        vec![s(keys::METHOD, "tools/call"), modern.clone()],
    ));
    assert_eq!(
        (r.group_key_kind, &*r.group_key),
        (GroupKeyKind::Trace, "01010101010101010101010101010101")
    );
    // 4: the process.
    let r = op(span(
        SERVER,
        1,
        2,
        None,
        vec![s(keys::METHOD, "tools/call"), modern],
    ));
    assert_eq!(
        (r.group_key_kind, &*r.group_key),
        (GroupKeyKind::Process, "pod-3")
    );
    // A legacy operation without a session id falls through.
    let r = op(span(
        SERVER,
        1,
        2,
        None,
        vec![s(keys::METHOD, "tools/call"), legacy],
    ));
    assert_eq!(
        (r.group_key_kind, &*r.group_key),
        (GroupKeyKind::Process, "pod-3")
    );
}

#[test]
fn multi_round_state_is_read_as_sixteen_bytes_or_not_at_all() {
    let r = op_of(call(
        1,
        vec![
            s(keys::MRTR_STATE_OUT, "59c50f1c773bc1354d420d974c06c5af"),
            s(keys::MRTR_STATE_IN, "not a hash"),
            int(keys::MRTR_ROUND, 70_000),
        ],
    ));
    assert_eq!(
        r.mrtr_state_out.map(|h| hex(&h)).as_deref(),
        Some("59c50f1c773bc1354d420d974c06c5af")
    );
    assert_eq!(r.mrtr_state_in, None);
    assert_eq!(r.mrtr_round, u16::MAX);
}

#[test]
fn the_deploy_id_is_the_resource_key_then_the_server_version() {
    let op = |resource: Vec<KeyValue>, attributes: Vec<KeyValue>| {
        let out = stages(
            resource,
            vec![call(1, attributes)],
            &IdentityCtx::Salted(None),
        );
        out.mcp_ops[0].record.clone()
    };
    let r = op(
        vec![
            s("service.version", "1.0.0"),
            s(keys::RESOURCE_DEPLOY_ID, "blue"),
        ],
        Vec::new(),
    );
    assert_eq!((&*r.deploy_id, &*r.server_version), ("blue", "1.0.0"));
    // The server version key wins over the resource, and is the deploy id when there is no deploy key.
    let r = op(
        vec![s("service.version", "1.0.0")],
        vec![s(keys::SERVER_VERSION, "2.0.0")],
    );
    assert_eq!((&*r.deploy_id, &*r.server_version), ("2.0.0", "2.0.0"));
    // Neither: the empty string, shown as "unknown".
    let r = op(Vec::new(), Vec::new());
    assert_eq!(
        (&*r.deploy_id, &*r.server_version, &*r.server_name),
        ("", "", "")
    );
}

#[test]
fn every_capture_source_is_read_and_an_unknown_one_is_otlp() {
    for source in CaptureSource::ALL {
        let r = op_of(call(1, vec![s(keys::CAPTURE_SOURCE, source.as_str())]));
        assert_eq!(r.capture_source, *source);
    }
    let r = op_of(call(1, vec![s(keys::CAPTURE_SOURCE, "elsewhere")]));
    assert_eq!(r.capture_source, CaptureSource::Otlp);
}

#[test]
fn strings_are_cut_at_their_caps() {
    let long = "é".repeat(2_000);
    let r = op_of(call(
        1,
        vec![
            s(keys::HANDLE_ID, &long),
            s(keys::CLIENT_NAME, &long),
            s(keys::USER_AGENT, &long),
            s(keys::CLIENT_IP_PREFIX, &long),
            s(keys::JSONRPC_ID, &long),
        ],
    ));
    for (value, cap) in [
        (&r.handle_id, caps::MCP_HANDLE_ID_BYTES),
        (&r.client_name, caps::MCP_CLIENT_NAME_BYTES),
        (&r.user_agent, caps::MCP_USER_AGENT_BYTES),
        (&r.ip_prefix, caps::MCP_IP_PREFIX_BYTES),
        (&r.jsonrpc_id, caps::MCP_JSONRPC_ID_BYTES),
        (&r.group_key, caps::MCP_GROUP_KEY_BYTES),
    ] {
        assert_eq!(value.len(), cap, "cut at the cap on a character boundary");
    }
    assert_eq!(r.group_key_kind, GroupKeyKind::Handle);
}

#[test]
fn content_is_kept_only_when_captured_and_is_cut_at_the_hard_cap() {
    // Arguments without the flag: a plain semantic-convention span keeps them.
    let r = op_of(call(1, vec![s(keys::TOOL_ARGUMENTS, r#"{"a":1}"#)]));
    assert!(r.content_captured);
    assert_eq!(
        (&r.args[..], r.args_bytes, r.result_bytes),
        (&br#"{"a":1}"#[..], 7, 0)
    );
    assert_eq!(r.content_sample_rate, 1.0);
    // The sender says it captured nothing: nothing is kept.
    let r = op_of(call(
        1,
        vec![
            s(keys::TOOL_ARGUMENTS, r#"{"a":1}"#),
            flag(keys::CONTENT_CAPTURED, false),
        ],
    ));
    assert!(!r.content_captured && r.args.is_empty());
    assert_eq!((r.args_bytes, r.content_sample_rate), (0, 0.0));
    // Structured arguments are stored as JSON text.
    let r = op_of(call(
        1,
        vec![kvlist(keys::TOOL_ARGUMENTS, vec![int("a", 1)])],
    ));
    assert_eq!(&r.args[..], br#"{"a":1}"#);
    // The hard cap of the normalizer.
    let big = "x".repeat(caps::MCP_CONTENT_MAX_BYTES + 5);
    let r = op_of(call(
        1,
        vec![
            s(keys::TOOL_RESULT, &big),
            flag(keys::CONTENT_CAPTURED, true),
        ],
    ));
    assert_eq!(r.result.len(), caps::MCP_CONTENT_MAX_BYTES);
    assert_eq!(r.result_bytes as usize, caps::MCP_CONTENT_MAX_BYTES + 5);
    assert_eq!(r.content_truncation, Truncation::CapExceeded);
    // A sample rate outside the unit interval is clamped.
    let r = op_of(call(
        1,
        vec![
            flag(keys::CONTENT_CAPTURED, true),
            double(keys::CONTENT_SAMPLE_RATE, 7.0),
        ],
    ));
    assert_eq!(r.content_sample_rate, 1.0);
}

fn tools_json(count: usize) -> String {
    let tools: Vec<String> = (0..count)
        .map(|i| format!(r#"{{"name":"tool-{i}","inputSchema":{{"type":"object","n":{i}}}}}"#))
        .collect();
    format!("[{}]", tools.join(","))
}

#[test]
fn a_tool_list_yields_inventory_entries() {
    let listing = span(
        SERVER,
        1,
        1,
        None,
        vec![
            s(keys::METHOD, "tools/list"),
            s(keys::CAPTURE_SOURCE, "sdk"),
            s(
                keys::TOOLS,
                r#"[{"name":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},
                    {"name":"echo","inputSchema":{"properties":{"text":{"type":"string"}},"type":"object"}},
                    {"name":"lookup","inputSchema":{"type":"object"},"outputSchema":{"type":"object"}},
                    {"description":"no name"},
                    {"name":""}]"#,
            ),
        ],
    );
    let out = stage_a(
        vec![s("service.name", "orders"), s("service.version", "3.0.0")],
        vec![span_with(1, 9, None, Vec::new()), listing],
    );
    let entries: Vec<_> = out.inventory.iter().collect();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|e| e.span == 1));
    let names: Vec<&str> = entries.iter().map(|e| &*e.record.tool_name).collect();
    assert_eq!(names, ["echo", "echo", "lookup"]);
    // The hash is of the canonical schemas: member order does not matter, the schema does.
    assert_eq!(entries[0].record.schema_hash, entries[1].record.schema_hash);
    assert_ne!(entries[0].record.schema_hash, entries[2].record.schema_hash);
    let e = &entries[2].record;
    assert_eq!((&*e.server_name, &*e.deploy_id), ("orders", "3.0.0"));
    assert_eq!(e.capture_source, CaptureSource::Sdk);
    assert_eq!(e.project_id, out.spans[1].core.project_id);
    assert_eq!(
        (e.first_seen, e.last_seen),
        (out.spans[1].core.start_time, out.spans[1].core.start_time)
    );
    // The list is consumed, and the reporting span's digest covers it.
    let residual = core::str::from_utf8(&out.spans[1].payload.attributes).unwrap();
    assert!(!residual.contains(keys::TOOLS));

    // A span that is no operation may report tools too (an SDK's snapshot at start), as an attribute array.
    let tool = |name: &str| AnyValue {
        value: Some(any_value::Value::KvlistValue(
            opentelemetry_proto::tonic::common::v1::KeyValueList {
                values: vec![s("name", name)],
            },
        )),
    };
    let snapshot = span_with(
        1,
        2,
        None,
        vec![kv(
            keys::TOOLS,
            any_value::Value::ArrayValue(ArrayValue {
                values: vec![tool("a"), tool("b")],
            }),
        )],
    );
    let out = stage_a(vec![s("service.name", "orders")], vec![snapshot]);
    assert!(out.mcp_ops.is_empty());
    assert_eq!(out.inventory.len(), 2);
    assert_eq!(out.inventory[0].record.capture_source, CaptureSource::Otlp);
    assert_ne!(out.spans[0].core.row_digest, row_digest(&out.spans[0]));
    // A value that is no tool list yields nothing.
    let out = stage_a(
        Vec::new(),
        vec![span_with(1, 2, None, vec![s(keys::TOOLS, "not json")])],
    );
    assert!(out.inventory.is_empty());
}

#[test]
fn inventory_entries_are_capped_per_frame_and_the_excess_is_counted() {
    let listing = |id: u8, count: usize| {
        span(
            SERVER,
            1,
            id,
            None,
            vec![
                s(keys::METHOD, "tools/list"),
                s(keys::TOOLS, &tools_json(count)),
            ],
        )
    };
    let out = stage_a(Vec::new(), vec![listing(1, 60_000), listing(2, 6_000)]);
    assert_eq!(out.inventory.len(), caps::MCP_INVENTORY_MAX_ROWS_PER_FRAME);
    assert_eq!(
        out.summary.inventory_dropped as usize,
        66_000 - caps::MCP_INVENTORY_MAX_ROWS_PER_FRAME
    );
    // The first in wire order are kept.
    assert_eq!(&*out.inventory[0].record.tool_name, "tool-0");
    assert_eq!(out.inventory.last().map(|e| e.span), Some(1));
}

#[test]
fn stage_b_hashes_the_subject_with_the_project_salt() {
    let identified = || {
        call(
            1,
            vec![
                s(keys::USER_ISSUER, "https://issuer.example"),
                s(keys::USER_SUBJECT, "subject-1"),
                s(keys::USER_ID, "user-7"),
                s(keys::TENANT, "acme"),
                s(keys::OAUTH_CLIENT_ID, "client-9"),
                s(keys::HANDLE_ID, "ord_1"),
            ],
        )
    };
    // Stage A leaves the hash to stage B and says that a salt is needed.
    let a = stage_a(Vec::new(), vec![identified()]);
    assert!(a.needs_salt());
    assert_eq!(a.mcp_ops[0].record.user_subject_hash, None);
    assert!(!stage_a(Vec::new(), vec![call(1, vec![s(keys::USER_ID, "user-7")])]).needs_salt());

    // With the salt: the first 16 bytes of HMAC-SHA256 over issuer, a zero byte and the subject.
    let salt = ProjectSalt::new([0x5a; 32]);
    let out = stages(
        Vec::new(),
        vec![identified()],
        &IdentityCtx::Salted(Some(&salt)),
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(&[0x5a; 32]).unwrap();
    mac.update(b"https://issuer.example\0subject-1");
    let expected = mac.finalize().into_bytes();
    let r = &out.mcp_ops[0].record;
    assert_eq!(
        r.user_subject_hash.map(|h| h.to_vec()),
        Some(expected[..16].to_vec())
    );
    assert_eq!(&*r.user_issuer, "https://issuer.example");
    assert_eq!(r.identity_source, IdentitySource::IdentifyHook);
    assert!(!out.needs_salt(), "the subject is discarded");

    // Without a control store: the identity keys are dropped, as with `identity: none` in the sender.
    let out = stages(Vec::new(), vec![identified()], &IdentityCtx::Suppressed);
    let r = &out.mcp_ops[0].record;
    assert_eq!(r.user_subject_hash, None);
    assert_eq!(
        (
            &*r.user_issuer,
            &*r.user_id,
            &*r.tenant,
            &*r.oauth_client_id
        ),
        ("", "", "", "")
    );
    assert_eq!(r.identity_source, IdentitySource::None);
    // The handle is not an identity key.
    assert_eq!(&*r.handle_id, "ord_1");

    // A subject that no salt was fetched for is dropped with its issuer; the identify hook's values stay.
    let out = stages(Vec::new(), vec![identified()], &IdentityCtx::Salted(None));
    let r = &out.mcp_ops[0].record;
    assert_eq!(
        (r.user_subject_hash, &*r.user_issuer, &*r.user_id),
        (None, "", "user-7")
    );
}

#[test]
fn a_dropped_repeat_takes_its_operation_and_its_inventory_with_it() {
    let listing = |version: &str| {
        span(
            SERVER,
            1,
            2,
            None,
            vec![
                s(keys::METHOD, "tools/list"),
                s(keys::SERVER_VERSION, version),
                s(keys::TOOLS, &tools_json(2)),
            ],
        )
    };
    let body = |spans: Vec<Span>| request_with(Vec::new(), spans).encode_to_vec();
    let at = |body: &[u8], lsn: u64| {
        let mut f = frame(body, &[]);
        f.lsn = Lsn::new(lsn);
        normalize(&f, &ctx()).unwrap()
    };
    let identity = IdentityCtx::Salted(None);
    let mut stage = StageB::default();

    let first = body(vec![
        call(1, Vec::new()),
        listing("1.0.0"),
        call(3, Vec::new()),
    ]);
    let mut frame_1 = at(&first, 1);
    assert_eq!(stage.admit(&mut frame_1, &identity).unwrap().duplicates, 0);
    assert_eq!(
        frame_1.mcp_ops.iter().map(|op| op.span).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(frame_1.inventory.len(), 2);

    // Sent again with a new call between two repeats: the repeats go, with their operations and entries, and
    // the new operation names its span where it now stands.
    let again = body(vec![
        call(1, Vec::new()),
        listing("1.0.0"),
        call(4, Vec::new()),
        call(3, Vec::new()),
    ]);
    let mut frame_2 = at(&again, 2);
    let admission = stage.admit(&mut frame_2, &identity).unwrap();
    assert_eq!((admission.duplicates, frame_2.spans.len()), (3, 1));
    assert_eq!(admission.stage_a_ordinals, [2]);
    assert_eq!(frame_2.mcp_ops.len(), 1);
    assert_eq!(frame_2.mcp_ops[0].span, 0);
    assert_eq!(
        frame_2.mcp_ops[0].record.span_id,
        frame_2.spans[0].core.span_id
    );
    assert!(frame_2.inventory.is_empty());

    // The same span with another operation is no exact repeat: the key that changed is one the projector
    // consumes, so only the operation tells the two apart.
    let changed = body(vec![listing("2.0.0")]);
    let mut frame_3 = at(&changed, 3);
    assert_eq!(stage.admit(&mut frame_3, &identity).unwrap().duplicates, 0);
    assert_eq!(&*frame_3.mcp_ops[0].record.server_version, "2.0.0");
    assert_eq!(frame_3.inventory.len(), 2);
    assert_eq!(frame_3.inventory[0].span, 0);
    // And so is the same span with another tool list.
    let relisted = body(vec![span(
        SERVER,
        1,
        2,
        None,
        vec![
            s(keys::METHOD, "tools/list"),
            s(keys::SERVER_VERSION, "2.0.0"),
            s(keys::TOOLS, &tools_json(3)),
        ],
    )]);
    let mut frame_4 = at(&relisted, 4);
    assert_eq!(stage.admit(&mut frame_4, &identity).unwrap().duplicates, 0);
    assert_eq!(frame_4.inventory.len(), 3);
}

#[test]
fn the_server_operation_hangs_under_the_agents_tool_call() {
    // The agent's side: a client span of its tool call, under the agent's own span. The client wrapper puts
    // the trace context into `params._meta`, so the server's span is in the same trace with the client span as
    // its parent (contract 9.1).
    let agent = span_with(1, 1, None, vec![s("gen_ai.operation.name", "invoke_agent")]);
    let tool_call = span(
        CLIENT,
        1,
        2,
        Some(1),
        vec![
            s(keys::METHOD, "tools/call"),
            s(keys::TOOL_NAME, "lookup_order"),
        ],
    );
    let served = span(
        SERVER,
        1,
        3,
        Some(2),
        vec![
            s(keys::METHOD, "tools/call"),
            s(keys::TOOL_NAME, "lookup_order"),
            s(keys::CAPTURE_SOURCE, "sdk"),
            s(keys::PROTOCOL_VERSION, "2026-07-28"),
        ],
    );
    // The two sides export on their own: two requests, the server's first.
    let server_frame = stages(
        vec![s("service.name", "orders")],
        vec![served],
        &IdentityCtx::Salted(None),
    );
    let agent_frame = stages(
        vec![s("service.name", "agent")],
        vec![agent, tool_call],
        &IdentityCtx::Salted(None),
    );

    let server = &server_frame.mcp_ops[0].record;
    let client = &agent_frame.mcp_ops[0].record;
    assert_eq!(
        (client.side, server.side),
        (McpSide::Client, McpSide::Server)
    );
    // The link: one trace, and the server operation's parent is the client operation's span.
    assert_eq!(server.trace_id, client.trace_id);
    assert_eq!(server.parent_span_id, client.span_id);
    assert_eq!(client.parent_span_id, agent_frame.spans[0].core.span_id);
    // The span rows carry the same chain, which is what the trace view nests by.
    let served_span = &server_frame.spans[0].core;
    assert_eq!(
        served_span.parent_span_id,
        agent_frame.spans[1].core.span_id
    );
    assert!(!served_span.is_root);
    assert_eq!(served_span.mcp_side, McpSide::Server);
    assert_eq!(agent_frame.spans[1].core.mcp_side, McpSide::Client);
    // Both sides group by the trace, so a session query finds the server operation from the agent's trace.
    assert_eq!(server.group_key_kind, GroupKeyKind::Trace);
    assert_eq!(
        (client.group_key_kind, &client.group_key),
        (GroupKeyKind::Trace, &server.group_key)
    );
    // The two rows are never the same row.
    assert_ne!(server.span_id, client.span_id);
    assert_eq!(
        (&*server.server_name, &*client.server_name),
        ("orders", "agent")
    );
}

/// Whether `needle` occurs in `haystack`.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Every byte a frame's records hold: the debug rendering of every record, which prints strings and bodies.
fn everything(out: &NormalizedFrame) -> Vec<u8> {
    format!("{out:?}").into_bytes()
}

fn planted() -> impl Strategy<Value = String> {
    "[a-z]{12,20}"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Contract 3.5: no record and no residual attribute holds a raw user subject, a raw session id, a client
    /// address, a credential header or an `Mcp-Param-*` value, on an MCP operation or on any other span.
    #[test]
    fn what_must_never_be_stored_is_in_no_record(
        subject in planted(),
        session in planted(),
        secrets in vec(planted(), 6),
        is_op in any::<bool>(),
        legacy in any::<bool>(),
        salted in any::<bool>(),
        suppressed in any::<bool>(),
    ) {
        let mut attributes = vec![
            s(keys::USER_ISSUER, "https://issuer.example"),
            s(keys::USER_SUBJECT, &subject),
            s(keys::SESSION_ID, &session),
            s("client.address", &secrets[0]),
            s("network.peer.address", &secrets[1]),
            s("http.request.header.authorization", &secrets[2]),
            s("http.request.header.cookie", &secrets[3]),
            s("http.request.header.mcp-param-region", &secrets[4]),
            s("HTTP.Request.Header.Cookie", &secrets[5]),
            s("kept", "this-stays"),
        ];
        if is_op {
            attributes.push(s(keys::METHOD, "tools/call"));
            let version = if legacy { "2025-11-25" } else { "2026-07-28" };
            attributes.push(s(keys::PROTOCOL_VERSION, version));
        }
        let salt = ProjectSalt::new([9; 32]);
        let identity = if suppressed {
            IdentityCtx::Suppressed
        } else {
            IdentityCtx::Salted(salted.then_some(&salt))
        };
        let resource = vec![s("service.name", "svc"), s("client.address", &secrets[0])];
        let a = stage_a(resource.clone(), vec![span(SERVER, 1, 1, None, attributes.clone())]);
        let b = stages(resource, vec![span(SERVER, 1, 1, None, attributes)], &identity);
        prop_assert_eq!(b.mcp_ops.len(), usize::from(is_op));
        // Stage A holds the subject only where no record can show it; stage B discards it.
        for out in [&a, &b] {
            let bytes = everything(out);
            prop_assert!(!holds(&bytes, subject.as_bytes()), "the subject is stored");
            prop_assert!(!holds(&bytes, session.as_bytes()), "the raw session id is stored");
            for secret in &secrets {
                prop_assert!(!holds(&bytes, secret.as_bytes()), "a forbidden value is stored");
            }
            prop_assert!(holds(&bytes, b"this-stays"));
        }
        if is_op {
            let r = &b.mcp_ops[0].record;
            // The session id is stored as its hash, in the legacy era; the subject as its keyed hash.
            prop_assert_eq!(r.session_key.is_some(), legacy);
            prop_assert_eq!(r.user_subject_hash.is_some(), salted && !suppressed);
            prop_assert_eq!(
                r.user_subject_hash,
                (salted && !suppressed)
                    .then(|| salt.subject_hash("https://issuer.example", &subject))
            );
        }
    }
}

fn any_value() -> impl Strategy<Value = any_value::Value> {
    prop_oneof![
        "\\PC{0,12}".prop_map(any_value::Value::StringValue),
        prop::sample::select(vec![
            "tools/call",
            "tools/list",
            "sdk",
            "proxy_stdio",
            "legacy",
            "modern",
            "stdio",
            "client",
            "server",
            "tool_error",
            "-32602",
            "-32603",
            "transport",
            "timeout",
            "cancelled",
            "notification",
            "input_required",
            "cap_exceeded",
            "2026-07-28",
            "true",
            "18446744073709551615",
            "59c50f1c773bc1354d420d974c06c5af",
            r#"[{"name":"t","inputSchema":{"b":1,"a":[2,{"z":0,"y":1}]}}]"#,
        ])
        .prop_map(|text| any_value::Value::StringValue(text.to_owned())),
        any::<i64>().prop_map(any_value::Value::IntValue),
        any::<bool>().prop_map(any_value::Value::BoolValue),
        any::<f64>().prop_map(any_value::Value::DoubleValue),
        vec(any::<u8>(), 0..8).prop_map(any_value::Value::BytesValue),
        vec("[a-z]{0,4}", 0..3).prop_map(|items| any_value::Value::ArrayValue(ArrayValue {
            values: items
                .into_iter()
                .map(|t| AnyValue {
                    value: Some(any_value::Value::StringValue(t))
                })
                .collect(),
        })),
    ]
}

fn any_key() -> impl Strategy<Value = String> {
    let known = vec![
        keys::METHOD,
        keys::PROTOCOL_VERSION,
        keys::SESSION_ID,
        keys::SIDE,
        keys::CAPTURE_SOURCE,
        keys::TOOL_NAME,
        keys::PROMPT_NAME,
        keys::RESOURCE_TEMPLATE,
        keys::ERA,
        keys::TRANSPORT,
        keys::NETWORK_TRANSPORT,
        keys::JSONRPC_ID,
        keys::DIRECTION,
        keys::IS_NOTIFICATION,
        keys::IS_BATCH,
        keys::ERROR_TYPE,
        keys::RPC_STATUS_CODE,
        keys::ERROR_CLASS,
        keys::ERROR_SUBCLASS,
        keys::RESULT_TYPE,
        keys::MRTR_ROUND,
        keys::MRTR_STATE_IN,
        keys::MRTR_STATE_OUT,
        keys::TASK_ID,
        keys::SUBSCRIPTION_ID,
        keys::PROGRESS_COUNT,
        keys::CANCEL_SOURCE,
        keys::TIMED_OUT,
        keys::CLIENT_NAME,
        keys::CLIENT_VERSION,
        keys::CLIENT_CAPABILITIES_HASH,
        keys::OAUTH_CLIENT_ID,
        keys::TENANT,
        keys::USER_ID,
        keys::HANDLE_ID,
        keys::USER_ISSUER,
        keys::USER_SUBJECT,
        keys::IDENTITY_SOURCE,
        keys::REQUEST_BYTES,
        keys::RESPONSE_BYTES,
        keys::TOOL_ARGUMENTS,
        keys::TOOL_RESULT,
        keys::CONTENT_CAPTURED,
        keys::CONTENT_SAMPLE_RATE,
        keys::CONTENT_TRUNCATION,
        keys::SERVER_NAME,
        keys::SERVER_VERSION,
        keys::CLIENT_IP_PREFIX,
        keys::USER_AGENT,
        keys::HTTP_STATUS,
        keys::TOOLS,
        "crucible.mcp.content.blocks.text",
        "crucible.mcp.content.blocks.",
        "crucible.mcp.content.blocks.image",
        keys::RESOURCE_DEPLOY_ID,
        keys::RESOURCE_INSTANCE_ID,
        "service.name",
        "service.version",
    ];
    prop_oneof![
        6 => prop::sample::select(known).prop_map(str::to_owned),
        1 => "[a-z.]{0,10}",
    ]
}

fn any_attributes() -> impl Strategy<Value = Vec<KeyValue>> {
    vec(
        (any_key(), any_value()).prop_map(|(key, value)| kv(&key, value)),
        0..24,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_500))]

    /// The projector is total and pure over the keys of the contract with values of any type: it never panics,
    /// gives equal records for equal frames, keeps every cap, and yields an operation exactly for the method key.
    #[test]
    fn the_projector_is_total_over_its_keys(
        spans in vec((any_attributes(), 0i32..6, prop::option::of(1u8..4)), 1..4),
        resource in any_attributes(),
        salted in any::<bool>(),
    ) {
        let wire: Vec<Span> = spans
            .iter()
            .enumerate()
            .map(|(at, (attributes, kind, parent))| span(*kind, 1, at as u8 + 1, *parent, attributes.clone()))
            .collect();
        let body = request_with(resource, wire).encode_to_vec();
        let f = frame(&body, &[]);
        let a = normalize(&f, &ctx()).unwrap();
        prop_assert_eq!(&a, &normalize(&f, &ctx()).unwrap());
        for (at, (attributes, ..)) in spans.iter().enumerate() {
            let carries = attributes.iter().any(|pair| pair.key == keys::METHOD);
            let op = a.mcp_ops.iter().find(|op| op.span as usize == at);
            prop_assert_eq!(op.is_some(), carries);
            prop_assert_eq!(
                a.spans[at].core.mcp_side,
                op.map_or(McpSide::None, |op| op.record.side)
            );
        }
        let salt = ProjectSalt::new([1; 32]);
        let identity = IdentityCtx::Salted(salted.then_some(&salt));
        let mut b = a.clone();
        StageB::default().admit(&mut b, &identity).unwrap();
        prop_assert!(!b.needs_salt());
        for op in &b.mcp_ops {
            let r = &op.record;
            prop_assert_ne!(r.side, McpSide::None);
            for (value, cap) in [
                (&r.server_name, caps::MCP_SERVER_NAME_BYTES),
                (&r.server_version, caps::MCP_SERVER_VERSION_BYTES),
                (&r.deploy_id, caps::MCP_DEPLOY_ID_BYTES),
                (&r.instance_id, caps::MCP_INSTANCE_ID_BYTES),
                (&r.protocol_version, caps::MCP_PROTOCOL_VERSION_BYTES),
                (&r.method, caps::MCP_METHOD_BYTES),
                (&r.target_name, caps::MCP_TARGET_NAME_BYTES),
                (&r.jsonrpc_id, caps::MCP_JSONRPC_ID_BYTES),
                (&r.task_id, caps::MCP_TASK_ID_BYTES),
                (&r.subscription_id, caps::MCP_TASK_ID_BYTES),
                (&r.error_type, caps::MCP_ERROR_TYPE_BYTES),
                (&r.error_message, caps::MCP_ERROR_MESSAGE_BYTES),
                (&r.client_name, caps::MCP_CLIENT_NAME_BYTES),
                (&r.client_version, caps::MCP_CLIENT_VERSION_BYTES),
                (&r.oauth_client_id, caps::MCP_OAUTH_CLIENT_ID_BYTES),
                (&r.user_issuer, caps::MCP_USER_ISSUER_BYTES),
                (&r.user_id, caps::MCP_USER_ID_BYTES),
                (&r.tenant, caps::MCP_TENANT_BYTES),
                (&r.handle_id, caps::MCP_HANDLE_ID_BYTES),
                (&r.group_key, caps::MCP_GROUP_KEY_BYTES),
                (&r.ip_prefix, caps::MCP_IP_PREFIX_BYTES),
                (&r.user_agent, caps::MCP_USER_AGENT_BYTES),
            ] {
                prop_assert!(value.len() <= cap);
            }
            prop_assert!(r.content_blocks.len() <= caps::MCP_CONTENT_BLOCKS_MAX_KEYS);
            prop_assert!(r.content_blocks.is_sorted_by(|a, b| a.0 < b.0));
            prop_assert!((0.0..=1.0).contains(&r.content_sample_rate));
            prop_assert!(r.content_captured || (r.args.is_empty() && r.result.is_empty()));
            // The class and the status are a pair of the taxonomy, and the subclass refines one class only.
            prop_assert!(crucible_records::class_fits_status(r.error_class, r.status));
            prop_assert!(r.error_subclass == ErrorSubclass::None || r.error_class == ErrorClass::ToolExecution);
            prop_assert!(!r.caller_fault || r.side == McpSide::Server);
            // The group key is the value its kind names.
            match r.group_key_kind {
                GroupKeyKind::LegacySession => {
                    prop_assert_eq!(r.era, Era::Legacy);
                    prop_assert_eq!(r.session_key.map(|k| hex(&k)), Some(r.group_key.to_string()));
                }
                GroupKeyKind::Handle => prop_assert_eq!(&r.group_key, &r.handle_id),
                GroupKeyKind::Trace => {
                    prop_assert!(!r.parent_span_id.is_zero());
                    prop_assert_eq!(&*r.group_key, hex(r.trace_id.as_bytes()));
                }
                GroupKeyKind::Process => prop_assert_eq!(&r.group_key, &r.instance_id),
            }
            prop_assert_eq!(r.session_key.is_some() && r.era == Era::Modern, false);
        }
        for entry in &b.inventory {
            prop_assert!(entry.record.tool_name.len() <= caps::MCP_INVENTORY_TOOL_NAME_BYTES);
            prop_assert!(!entry.record.tool_name.is_empty());
            prop_assert!((entry.span as usize) < b.spans.len());
        }
    }
}

/// The key registry of the wire contract is `conformance/registry.json`, shared by every SDK, the proxy and the
/// normalizer (contract 9.1). Every key the projector reads is in it, with a type the projector takes, so that
/// a producer and the projector cannot drift apart.
#[test]
fn every_key_the_projector_reads_is_in_the_registry_with_its_type() {
    let registry: serde_json::Value =
        serde_json::from_str(include_str!("../../../conformance/registry.json")).unwrap();
    let entries = registry["keys"].as_array().unwrap();
    let types = |key: &str, prefix: bool| -> Vec<String> {
        let field = if prefix { "prefix" } else { "key" };
        let entry = entries
            .iter()
            .find(|entry| entry[field].as_str() == Some(key))
            .unwrap_or_else(|| panic!("{key} is not in the registry"));
        match &entry["type"] {
            serde_json::Value::String(one) => vec![one.clone()],
            many => many
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().to_owned())
                .collect(),
        }
    };
    // Each key with the registry types the projector reads it as.
    let string = &["string"][..];
    let read: &[(&str, &[&str])] = &[
        (keys::METHOD, string),
        (keys::PROTOCOL_VERSION, string),
        (keys::SESSION_ID, string),
        (keys::SIDE, string),
        (keys::CAPTURE_SOURCE, string),
        (keys::TOOL_NAME, string),
        (keys::PROMPT_NAME, string),
        (keys::RESOURCE_TEMPLATE, string),
        (keys::ERA, string),
        (keys::TRANSPORT, string),
        (keys::NETWORK_TRANSPORT, string),
        (keys::JSONRPC_ID, &["string", "int"]),
        (keys::DIRECTION, string),
        (keys::IS_NOTIFICATION, &["bool"]),
        (keys::IS_BATCH, &["bool"]),
        (keys::ERROR_TYPE, string),
        (keys::RPC_STATUS_CODE, &["string", "int"]),
        (keys::ERROR_CLASS, string),
        (keys::ERROR_SUBCLASS, string),
        (keys::RESULT_TYPE, string),
        (keys::MRTR_ROUND, &["int"]),
        (keys::MRTR_STATE_IN, string),
        (keys::MRTR_STATE_OUT, string),
        (keys::TASK_ID, string),
        (keys::SUBSCRIPTION_ID, string),
        (keys::PROGRESS_COUNT, &["int"]),
        (keys::CANCEL_SOURCE, string),
        (keys::TIMED_OUT, &["bool"]),
        (keys::CLIENT_NAME, string),
        (keys::CLIENT_VERSION, string),
        (keys::CLIENT_CAPABILITIES_HASH, &["int", "string"]),
        (keys::OAUTH_CLIENT_ID, string),
        (keys::TENANT, string),
        (keys::USER_ID, string),
        (keys::HANDLE_ID, string),
        (keys::USER_ISSUER, string),
        (keys::USER_SUBJECT, string),
        (keys::IDENTITY_SOURCE, string),
        (keys::REQUEST_BYTES, &["int"]),
        (keys::RESPONSE_BYTES, &["int"]),
        (keys::TOOL_ARGUMENTS, &["string", "kvlist", "array"]),
        (keys::TOOL_RESULT, &["string", "kvlist", "array"]),
        (keys::CONTENT_CAPTURED, &["bool"]),
        (keys::CONTENT_SAMPLE_RATE, &["number", "double", "int"]),
        (keys::CONTENT_TRUNCATION, string),
        (keys::SERVER_NAME, string),
        (keys::SERVER_VERSION, string),
        (keys::CLIENT_IP_PREFIX, string),
        (keys::USER_AGENT, string),
        (keys::HTTP_STATUS, &["int"]),
        (keys::TOOLS, &["string", "array"]),
        (keys::RESOURCE_SERVICE_NAME, string),
        (keys::RESOURCE_SERVICE_VERSION, string),
        (keys::RESOURCE_DEPLOY_ID, string),
        (keys::RESOURCE_INSTANCE_ID, string),
    ];
    for (key, accepted) in read {
        for ty in types(key, false) {
            assert!(
                accepted.contains(&ty.as_str()),
                "the registry types {key} as {ty}, which the projector does not read"
            );
        }
    }
    assert_eq!(types(keys::CONTENT_BLOCKS_PREFIX, true), ["int"]);
    // The list above is every key the projector consumes, and the registry's forbidden keys are never stored.
    let listed = |key: &str| read.iter().any(|(k, _)| *k == key);
    for entry in entries {
        if let Some(key) = entry["key"].as_str()
            && keys::consumed(key)
        {
            assert!(listed(key), "{key} is consumed and not checked here");
        }
    }
    for key in registry["forbidden"]["keys"].as_array().unwrap() {
        assert!(keys::never_stored(key.as_str().unwrap()), "{key}");
    }
    for prefix in registry["forbidden"]["prefixes"].as_array().unwrap() {
        let key = format!("{}x", prefix.as_str().unwrap());
        assert!(keys::never_stored(&key), "{key}");
    }
}
