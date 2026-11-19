//! An export re-ingested through OTLP yields the same spans (contract 12.8): a natively instrumented request is
//! normalized, each span is written as its `otlp-json` line, the line goes through the OTLP/JSON transcoder and
//! the normalizer again, and the record comes back equal in both tiers, except the ingestion group, `event_bytes`
//! and `api_key_id`, which the new request sets.
//!
//! The domain is the native keys (contract 10.6), message bodies in the semantic-convention shape, a tool's raw
//! arguments and result, observation types carried by `gen_ai.operation.name` or `crucible.observation.type`,
//! residual values without bytes or non-finite doubles, metadata string values that are not JSON text of another
//! type, MCP operations (contract 9.1) without a user subject or a tool list, and no field that a cap cut. The
//! writer sees each record as the store returns it: the span without the `metadata` object, which the store keeps
//! only as its keys and text values (contract 3.1), and the MCP operation as its `mcp_ops` and `mcp_content` rows
//! as the shipper encodes them. A metadata string value that is JSON text of a number, a flag or null therefore
//! comes back as that value, the one known asymmetry; the operation comes back equal.

// Test code throughout: lets the helpers use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support::scope;
use crucible_export::{Written, write_otlp};
use crucible_normalize::{
    IdentityCtx, IngestFrame, NormCtx, NormalizedFrame, PriceTable, normalize,
};
use crucible_otlp::transcode_json;
use crucible_plan::Value;
use crucible_query::{ExportColumn, ExportMcp, ExportRecord};
use crucible_records::{
    ApiKeyId, Ingestion, Lsn, McpOpRecord, PayloadCaps, ProjectId, SpanRecord, Timestamp, Uuid,
    WalEpoch,
};
use crucible_schema::{Column, McpContentCol, McpOpsCol, Ty};
use crucible_shipper::tables::{mcp_content_row, mcp_ops_row};
use crucible_shipper::value::Value as Cell;
use proptest::prelude::*;
use serde_json::{Value as J, json};

/// 2026-11-17T09:30:00Z in nanoseconds.
const BASE_NANOS: u64 = 1_794_907_800_000_000_000;
const RECEIVED_AT: Timestamp = Timestamp::from_micros(1_794_907_900_000_000);
const PROJECT: ProjectId = ProjectId::from_bytes([0x22; 16]);

fn frame(otlp: &[u8], lsn: u64) -> IngestFrame<'_> {
    IngestFrame {
        wal_epoch: WalEpoch::from_bytes([0x11; 16]),
        lsn: Lsn::new(lsn),
        received_at: RECEIVED_AT,
        project_id: PROJECT,
        api_key_id: ApiKeyId::from_bytes([0x33; 16]),
        compat_dialect: 0,
        sdk_name: "",
        sdk_version: "",
        rejected: &[],
        otlp,
    }
}

/// The frame of a request, normalized with the identity keys kept (a control store, no user subject).
fn normalized(json: &[u8], lsn: u64) -> NormalizedFrame {
    let otlp = transcode_json(json).expect("the request transcodes");
    let ctx = NormCtx::new(PriceTable::bundled().unwrap(), PayloadCaps::DEFAULT)
        .with_identity(IdentityCtx::Salted(None));
    normalize(&frame(&otlp, lsn), &ctx).expect("the request normalizes")
}

const NO_INGESTION: Ingestion = Ingestion {
    wal_epoch: WalEpoch::from_bytes([0; 16]),
    batch_seq: Lsn::ZERO,
    lsn: Lsn::ZERO,
    received_at: Timestamp::EPOCH,
};

/// The record without what each request sets for itself, and without the row digest, a function of the rest
/// and of the MCP operation, which are compared themselves.
fn comparable(mut record: SpanRecord) -> SpanRecord {
    record.core.ingestion = NO_INGESTION;
    record.core.event_bytes = 0;
    record.core.api_key_id = ApiKeyId::from_bytes([0; 16]);
    record.core.row_digest = 0;
    record
}

/// The re-ingested record without the `gen_ai.operation.name` the writer adds to a span that came in typed
/// without one (contract 12.8 re-emits the operation name): the key is a type source that the mapping leaves in
/// the residual, where the writer's first residual key lands first.
fn without_added_operation(mut again: SpanRecord, original: &SpanRecord) -> SpanRecord {
    const KEY: &str = "\"gen_ai.operation.name\":";
    let had = String::from_utf8_lossy(&original.payload.attributes).contains(KEY);
    let residual = String::from_utf8_lossy(&again.payload.attributes).into_owned();
    if let Some(rest) = residual.strip_prefix(&format!("{{{KEY}")).filter(|_| !had) {
        let after = rest.find(['"']).and_then(|open| {
            let close = open + 1 + rest[open + 1..].find('"')?;
            Some(&rest[close + 1..])
        });
        again.payload.attributes = match after {
            Some("}") => bytes::Bytes::new(),
            Some(more) => format!("{{{}", more.trim_start_matches(',')).into(),
            None => again.payload.attributes,
        };
    }
    again
}

/// The operation without what each request sets for itself.
fn comparable_op(mut op: McpOpRecord) -> McpOpRecord {
    op.ingestion = NO_INGESTION;
    op
}

/// A cell as the store returns it: the column type decides the value, as the store's decoder does.
fn stored(ty: &Ty, cell: &Cell) -> Value {
    match (ty, cell) {
        (_, Cell::Null) => Value::Null,
        (Ty::Nullable(inner), cell) => stored(inner, cell),
        (Ty::Enum8(_), Cell::I8(v)) => Value::Enum(u8::try_from(*v).unwrap()),
        (Ty::Array(inner), Cell::Array(items)) => {
            Value::Array(items.iter().map(|item| stored(inner, item)).collect())
        }
        (Ty::String | Ty::LowCardinalityString, Cell::Bytes(b)) => {
            match String::from_utf8(b.to_vec()) {
                Ok(s) => Value::String(s),
                Err(e) => Value::Bytes(e.into_bytes()),
            }
        }
        (_, Cell::Bytes(b)) => Value::Bytes(b.to_vec()),
        (_, Cell::U8(v)) => Value::UInt(u64::from(*v)),
        (_, Cell::U16(v)) => Value::UInt(u64::from(*v)),
        (_, Cell::U32(v)) => Value::UInt(u64::from(*v)),
        (_, Cell::U64(v)) => Value::UInt(*v),
        (_, Cell::I8(v)) => Value::Int(i64::from(*v)),
        (_, Cell::I32(v)) => Value::Int(i64::from(*v)),
        (_, Cell::I64(v)) => Value::Int(*v),
        (_, Cell::F32(v)) => Value::Float(f64::from(*v)),
        (_, Cell::F64(v)) => Value::Float(*v),
        (_, Cell::Uuid(b)) => Value::Uuid(Uuid::from_bytes(*b)),
        (_, Cell::Tuple(_) | Cell::Array(_)) => panic!("no MCP column holds {cell:?} as {ty:?}"),
    }
}

/// The columns of a shipper row (the project id first, then the table's columns), but `skip`, as the export
/// reads return them.
fn columns<C: Column>(cells: &[Cell], skip: &[&str]) -> Vec<ExportColumn> {
    assert_eq!(
        cells.len(),
        C::ALL.len() + 1,
        "the project id and every column"
    );
    C::ALL
        .iter()
        .zip(&cells[1..])
        .map(|(c, cell)| (c.column_ref(), cell))
        .filter(|(column, _)| !skip.contains(&column.name()))
        .map(|(column, cell)| ExportColumn {
            name: column.name(),
            ty: *column.ty(),
            value: stored(column.ty(), cell),
        })
        .collect()
}

/// The MCP operation as the export reads return it: the `mcp_ops` row and, when content was captured, the
/// `mcp_content` row, without the key and ingestion columns.
fn stored_mcp(op: &McpOpRecord) -> ExportMcp {
    const INGESTION: [&str; 4] = ["wal_epoch", "batch_seq", "lsn", "received_at"];
    const KEY: [&str; 5] = [
        "start_minute",
        "trace_hash",
        "start_time",
        "span_id",
        "trace_id",
    ];
    let skip_op: Vec<&str> = KEY
        .into_iter()
        .chain(["parent_span_id"])
        .chain(INGESTION)
        .collect();
    let skip_content: Vec<&str> = KEY
        .into_iter()
        .chain(["user_id"])
        .chain(INGESTION)
        .collect();
    ExportMcp {
        op: columns::<McpOpsCol>(&mcp_ops_row(op), &skip_op),
        content: op
            .content_captured
            .then(|| columns::<McpContentCol>(&mcp_content_row(op), &skip_content)),
    }
}

fn s(key: &str, value: &str) -> J {
    json!({"key": key, "value": {"stringValue": value}})
}

fn int(key: &str, value: i64) -> J {
    json!({"key": key, "value": {"intValue": value.to_string()}})
}

fn attr(key: &str, value: J) -> J {
    json!({"key": key, "value": value})
}

fn text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .,\\-_\"\\\\\n\u{e9}]{0,12}"
}

fn word() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,6}"
}

/// A residual attribute value: a string, an integer, a finite double, a flag or a list of strings.
fn residual_value() -> impl Strategy<Value = J> {
    prop_oneof![
        text().prop_map(|t| json!({"stringValue": t})),
        any::<i64>().prop_map(|i| json!({"intValue": i.to_string()})),
        prop_oneof![Just(0.5), Just(1.0), Just(-2.25), Just(1e300)].prop_map(|d: f64| json!({"doubleValue": d})),
        any::<bool>().prop_map(|b| json!({"boolValue": b})),
        prop::collection::vec(word(), 0..3)
            .prop_map(|items| json!({"arrayValue": {"values": items.iter().map(|w| json!({"stringValue": w})).collect::<Vec<_>>()}})),
    ]
}

/// GenAI messages as JSON text, sometimes with whitespace between the tokens.
fn messages(role: &'static str) -> impl Strategy<Value = String> {
    (text(), any::<bool>()).prop_map(move |(content, pretty)| {
        let value = json!([{"role": role, "parts": [{"type": "text", "content": content}]}]);
        if pretty {
            serde_json::to_string_pretty(&value).unwrap()
        } else {
            value.to_string()
        }
    })
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Generation,
    Embedding,
    Tool,
    Agent,
    Plain(&'static str),
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        Just(Kind::Generation),
        Just(Kind::Embedding),
        Just(Kind::Tool),
        Just(Kind::Agent),
        prop_oneof![
            Just("span"),
            Just("chain"),
            Just("event"),
            Just("retriever"),
            Just("guardrail")
        ]
        .prop_map(Kind::Plain),
        Just(Kind::Generation),
    ]
}

const METHODS: [&str; 6] = [
    "tools/call",
    "prompts/get",
    "resources/read",
    "initialize",
    "notifications/progress",
    "tools/list",
];

/// The keys of an MCP operation (contract 9.1): the method, its target, and each key that `picks` names at
/// most once, its value chosen by the pick's number or word. `tool_span` says that the span is a tool span,
/// which carries its own tool name, arguments and result.
fn mcp_attributes(method: usize, picks: &[(usize, u64, String)], tool_span: bool) -> Vec<J> {
    let method = METHODS[method % METHODS.len()];
    let mut out = vec![s("mcp.method.name", method)];
    match method {
        "tools/call" if !tool_span => out.push(s("gen_ai.tool.name", "search_docs")),
        "prompts/get" => out.push(s("gen_ai.prompt.name", "triage")),
        "resources/read" => out.push(s("crucible.mcp.resource.template", "file:///{path}")),
        _ => {}
    }
    let mut seen = Vec::new();
    for (which, n, word) in picks {
        if seen.contains(which) {
            continue;
        }
        seen.push(*which);
        let pick = |options: &[&str]| options[(*n % options.len() as u64) as usize].to_owned();
        let small = |m: u64| i64::try_from(*n % m).unwrap();
        let flag = json!({"boolValue": n % 2 == 0});
        out.push(match which {
            0 => s(
                "mcp.protocol.version",
                &pick(&["2024-11-05", "2025-06-18", "2026-07-28"]),
            ),
            1 => s("crucible.mcp.era", &pick(&["legacy", "modern"])),
            2 => s("crucible.mcp.side", &pick(&["client", "server"])),
            3 => s(
                "crucible.mcp.capture_source",
                &pick(&["sdk", "proxy_stdio", "proxy_http", "otlp"]),
            ),
            4 => s(
                "crucible.mcp.transport",
                &pick(&["stdio", "streamable_http", "sse"]),
            ),
            5 => s("network.transport", &pick(&["pipe", "tcp", "udp"])),
            6 => s("jsonrpc.request.id", word),
            7 => int("jsonrpc.request.id", small(1000)),
            8 => s(
                "crucible.mcp.direction",
                &pick(&["client_to_server", "server_to_client"]),
            ),
            9 => attr("crucible.mcp.is_notification", flag),
            10 => attr("crucible.mcp.is_batch", flag),
            11 => s(
                "error.type",
                &pick(&[
                    "tool_error",
                    "-32602",
                    "-32601",
                    "TimeoutError",
                    "500",
                    "-32002",
                ]),
            ),
            12 => int(
                "rpc.response.status_code",
                [-32700, -32602, -32601, -32002, -32000, -1][small(6) as usize],
            ),
            13 => int(
                "http.response.status_code",
                [200, 401, 403, 429, 500][small(5) as usize],
            ),
            14 => s(
                "crucible.mcp.error.class",
                &pick(&[
                    "none",
                    "protocol",
                    "validation",
                    "not_found",
                    "tool_execution",
                    "auth",
                    "rate_limit",
                    "timeout",
                    "transport",
                    "cancelled",
                    "internal",
                ]),
            ),
            15 => s(
                "crucible.mcp.error.subclass",
                &pick(&["auth", "rate_limit", "upstream", "business"]),
            ),
            16 => s(
                "crucible.mcp.result_type",
                &pick(&["none", "complete", "input_required", "task"]),
            ),
            17 => int("crucible.mcp.mrtr.round", small(5)),
            18 => s(
                "crucible.mcp.mrtr.state_in",
                &format!("{:032x}", u128::from(*n) + 1),
            ),
            19 => s(
                "crucible.mcp.mrtr.state_out",
                &format!("{:032X}", u128::from(*n) + 1),
            ),
            20 => s("crucible.mcp.task.id", word),
            21 => s("crucible.mcp.subscription.id", word),
            22 => int("crucible.mcp.progress.count", small(100)),
            23 => s(
                "crucible.mcp.cancel.source",
                &pick(&["notification", "stream_close"]),
            ),
            24 => attr("crucible.mcp.timed_out", flag),
            25 => s("crucible.mcp.client.name", word),
            26 => s("crucible.mcp.client.version", word),
            27 => int(
                "crucible.mcp.client.capabilities_hash",
                i64::from_ne_bytes(n.to_ne_bytes()),
            ),
            28 => s("crucible.mcp.oauth.client_id", word),
            29 => s("crucible.mcp.tenant", word),
            30 => s("user.id", word),
            31 => s("crucible.mcp.handle.id", word),
            32 => s("crucible.mcp.user.issuer", word),
            33 => s(
                "crucible.mcp.identity.source",
                &pick(&["none", "identify_hook", "token_claims", "token_verified"]),
            ),
            34 if n % 2 == 0 => s("mcp.session.id", word),
            34 => s("mcp.session.id", &format!("{:032x}", u128::from(*n))),
            35 => int("crucible.mcp.request.bytes", small(100_000)),
            36 => int("crucible.mcp.response.bytes", small(100_000)),
            37 => int(&format!("crucible.mcp.content.blocks.{word}"), small(9)),
            38 => attr("crucible.mcp.content.captured", flag),
            39 => {
                let rate = [0.0, 0.25, 1.0][small(3) as usize];
                attr(
                    "crucible.mcp.content.sample_rate",
                    json!({"doubleValue": rate}),
                )
            }
            40 => s("crucible.mcp.server.name", word),
            41 => s("crucible.mcp.server.version", word),
            42 => s("crucible.mcp.client.ip_prefix", "10.1.2.0/24"),
            43 => s("user_agent.original", word),
            44 if !tool_span => s(
                "gen_ai.tool.call.arguments",
                &format!("{{\"q\": \"{word}\"}}"),
            ),
            45 if !tool_span => s("gen_ai.tool.call.result", word),
            _ => s("crucible.mcp.content.truncation", "none"),
        });
    }
    out
}

prop_compose! {
    /// One natively instrumented span of trace `t`, its id `id`, a child of `parent` when given.
    fn native_span(t: [u8; 16], id: u8, parent: Option<u8>)(
        kind in kind(),
        name in text(),
        span_kind in 0u8..6,
        start in 0u64..10_000_000,
        duration in 0u64..5_000_000,
        status in (0u8..3, text()),
        trace_state in prop::option::of("[a-z]{1,4}=[a-z0-9]{1,4}"),
        model in prop::option::of(("[a-z]{1,6}/[a-z0-9-]{1,8}", any::<bool>())),
        provider in prop::option::of(word()),
        usage in (prop::option::of(0i64..100_000), prop::option::of(0i64..100_000), prop::option::of(0i64..500)),
        bodies in (messages("user"), messages("assistant"), any::<bool>(), any::<bool>()),
        finish in prop::collection::vec(word(), 0..2),
        params in (prop::option::of(prop_oneof![Just(0.0), Just(0.7), Just(1.0)]), prop::option::of(1i64..4096)),
        natives in prop::collection::vec((0usize..14, word(), text()), 0..6),
        residual in prop::collection::vec((word(), residual_value()), 0..4),
        events in prop::collection::vec((0u64..1_000_000, word(), prop::collection::vec((word(), residual_value()), 0..2)), 0..2),
        link in prop::option::of((any::<[u8; 16]>(), any::<[u8; 8]>())),
        mcp in prop::option::of((0usize..6, prop::collection::vec((0usize..46, any::<u64>(), word()), 0..16))),
    ) -> J {
        let start = BASE_NANOS + start * 1_000;
        let mut attributes = Vec::new();
        match kind {
            Kind::Generation => attributes.push(s("gen_ai.operation.name", "chat")),
            Kind::Embedding => attributes.push(s("gen_ai.operation.name", "embeddings")),
            Kind::Tool => {
                attributes.push(s("gen_ai.operation.name", "execute_tool"));
                attributes.push(s("gen_ai.tool.name", "lookup_order"));
            }
            Kind::Agent => attributes.push(s("gen_ai.operation.name", "invoke_agent")),
            Kind::Plain(name) => attributes.push(s("crucible.observation.type", name)),
        }
        if let Some((model, response)) = &model {
            attributes.push(s("gen_ai.request.model", model));
            if *response {
                attributes.push(s("gen_ai.response.model", &format!("{model}-0801")));
            }
        }
        if let Some(provider) = &provider {
            attributes.push(s("gen_ai.provider.name", provider));
        }
        for (key, value) in [
            ("gen_ai.usage.input_tokens", usage.0),
            ("gen_ai.usage.output_tokens", usage.1),
            ("gen_ai.usage.cache_read.input_tokens", usage.2),
        ] {
            if let Some(value) = value {
                attributes.push(int(key, value));
            }
        }
        match kind {
            Kind::Tool => {
                if bodies.2 {
                    attributes.push(s("gen_ai.tool.call.arguments", "{\"order_id\": \"ORD-104\"}"));
                }
                if bodies.3 {
                    attributes.push(s("gen_ai.tool.call.result", "shipped"));
                }
            }
            Kind::Generation | Kind::Agent => {
                if bodies.2 {
                    attributes.push(s("gen_ai.input.messages", &bodies.0));
                }
                if bodies.3 {
                    attributes.push(s("gen_ai.output.messages", &bodies.1));
                    attributes.push(s("gen_ai.system_instructions", "[{\"type\":\"text\",\"content\":\"Be brief.\"}]"));
                }
            }
            _ => {}
        }
        if !finish.is_empty() {
            let values: Vec<J> = finish.iter().map(|f| json!({"stringValue": f})).collect();
            attributes.push(attr("gen_ai.response.finish_reasons", json!({"arrayValue": {"values": values}})));
        }
        if let Some(temperature) = params.0 {
            attributes.push(attr("gen_ai.request.temperature", json!({"doubleValue": temperature})));
        }
        if let Some(max) = params.1 {
            attributes.push(int("gen_ai.request.max_tokens", max));
        }
        // The native keys of the fields without a semantic convention, each at most once.
        let mut seen = Vec::new();
        for (which, word, value) in &natives {
            if seen.contains(which) {
                continue;
            }
            seen.push(*which);
            attributes.push(match which {
                0 => s("crucible.session.id", word),
                1 => s("crucible.user.id", word),
                2 => s("crucible.trace.name", value),
                3 => s("crucible.environment", word),
                4 => s("crucible.release", word),
                5 => s("crucible.version", word),
                6 => attr("crucible.tags", json!({"arrayValue": {"values": [{"stringValue": word}]}})),
                7 => s(&format!("crucible.metadata.{word}"), &format!("v{value}")),
                8 => s("crucible.level", ["debug", "default", "warning", "error"][word.len() % 4]),
                9 => s("crucible.prompt.name", word),
                10 => s("crucible.cost.total", "0.000340000"),
                11 => s("crucible.completion_start_time", "2026-11-17T09:30:00.500000Z"),
                12 => attr("crucible.trace.public", json!({"boolValue": word.len() % 2 == 0})),
                _ => int(&format!("crucible.metadata.n_{word}"), 7),
            });
        }
        if let Some((method, picks)) = &mcp {
            attributes.extend(mcp_attributes(*method, picks, matches!(kind, Kind::Tool)));
        }
        for (key, value) in residual {
            attributes.push(attr(&format!("app.{key}"), value));
        }
        let mut span = json!({
            "traceId": t.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "spanId": format!("{:016x}", u64::from(id)),
            "name": name,
            "kind": span_kind,
            "startTimeUnixNano": start.to_string(),
            "endTimeUnixNano": (start + duration * 1_000).to_string(),
            "attributes": attributes,
            "status": {"code": status.0, "message": status.1},
        });
        if let Some(parent) = parent {
            span["parentSpanId"] = J::from(format!("{:016x}", u64::from(parent)));
        }
        if let Some(state) = trace_state {
            span["traceState"] = J::from(state);
        }
        if !events.is_empty() {
            span["events"] = J::from(events.iter().map(|(at, name, attrs)| json!({
                "timeUnixNano": (start + at * 1_000).to_string(),
                "name": name,
                "attributes": attrs.iter().map(|(k, v)| attr(k, v.clone())).collect::<Vec<_>>(),
            })).collect::<Vec<_>>());
        }
        if let Some((trace, span_id)) = link {
            span["links"] = json!([{
                "traceId": trace.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "spanId": span_id.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "attributes": [s("link.kind", "follows")],
            }]);
        }
        span
    }
}

prop_compose! {
    /// A request of one trace: a root and up to two children, under one resource and one scope.
    fn request()(trace in any::<[u8; 16]>().prop_filter("a trace id is never zero", |t| *t != [0; 16]))(
        root in native_span(trace, 1, None),
        children in prop::collection::vec(native_span(trace, 2, Some(1)), 0..2),
        resource in (prop::option::of(word()), prop::option::of("[0-9]\\.[0-9]"), prop::option::of(word()),
            prop::collection::vec((word(), residual_value()), 0..2)),
        // Where an MCP server runs (contracts 9.1 and 9.7).
        server in (prop::option::of(word()), prop::option::of(word())),
    ) -> J {
        let mut spans = vec![root];
        for (i, mut child) in children.into_iter().enumerate() {
            child["spanId"] = J::from(format!("{:016x}", 2 + i as u64));
            spans.push(child);
        }
        let mut attributes = Vec::new();
        if let Some(name) = resource.0 {
            attributes.push(s("service.name", &name));
        }
        if let Some(version) = resource.1 {
            attributes.push(s("service.version", &version));
        }
        if let Some(environment) = resource.2 {
            attributes.push(s("deployment.environment.name", &environment));
        }
        attributes.push(s("telemetry.sdk.language", "python"));
        for (key, value) in resource.3 {
            attributes.push(attr(&format!("host.{key}"), value));
        }
        if let Some(instance) = server.0 {
            attributes.push(s("service.instance.id", &instance));
        }
        if let Some(deploy) = server.1 {
            attributes.push(s("crucible.deploy.id", &deploy));
        }
        json!({"resourceSpans": [{
            "resource": {"attributes": attributes},
            "scopeSpans": [{"scope": {"name": "opentelemetry.instrumentation.genai", "version": "0.9.0"}, "spans": spans}],
        }]})
    }
}

fn round_trip(request: &J) -> Result<(), TestCaseError> {
    let first = normalized(request.to_string().as_bytes(), 7);
    prop_assert!(!first.spans.is_empty());
    let project = scope(PROJECT).unwrap();
    for (n, record) in first.spans.iter().enumerate() {
        let op = first
            .mcp_ops
            .iter()
            .find(|op| op.span as usize == n)
            .map(|op| &op.record);
        // As the store returns it: `metadata` only as its keys and values, the operation as its rows.
        let mut stored = record.clone();
        stored.payload.metadata = bytes::Bytes::new();
        let export = ExportRecord {
            span: stored,
            payload_stored: true,
            mcp: op.map(stored_mcp),
            facts: Vec::new(),
        };
        let mut line = Vec::new();
        prop_assert_eq!(
            write_otlp(project, &export, false, &mut line),
            Written::Line
        );
        let again = normalized(&line, 100 + n as u64);
        prop_assert_eq!(again.spans.len(), 1, "one span per line");
        prop_assert_eq!(
            comparable(without_added_operation(again.spans[0].clone(), record)),
            comparable(record.clone()),
            "line: {}",
            String::from_utf8_lossy(&line)
        );
        prop_assert_eq!(
            again
                .mcp_ops
                .first()
                .map(|op| comparable_op(op.record.clone())),
            op.cloned().map(comparable_op),
            "line: {}",
            String::from_utf8_lossy(&line)
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn an_export_reingested_through_otlp_yields_the_same_spans(request in request()) {
        round_trip(&request)?;
    }
}

#[test]
fn a_generation_with_usage_and_cost_comes_back_the_same() {
    let request = json!({"resourceSpans": [{
        "resource": {"attributes": [s("service.name", "support-app"), s("service.version", "1.4.0")]},
        "scopeSpans": [{"scope": {"name": "opentelemetry.instrumentation.genai"}, "spans": [{
            "traceId": "c3a1e2f4b5d6478990a1b2c3d4e5f601",
            "spanId": "0d1e2f3a4b5c6d7e",
            "name": "chat gpt-4o-mini",
            "kind": 3,
            "startTimeUnixNano": "1794907800050000000",
            "endTimeUnixNano": "1794907800850000000",
            "attributes": [
                s("gen_ai.operation.name", "chat"),
                s("gen_ai.request.model", "gpt-4o-mini"),
                s("gen_ai.provider.name", "openai"),
                int("gen_ai.usage.input_tokens", 27),
                int("gen_ai.usage.output_tokens", 11),
                s("gen_ai.input.messages", "[{\"role\":\"user\",\"parts\":[{\"type\":\"text\",\"content\":\"Where is my order?\"}]}]"),
                s("gen_ai.output.messages", "[{\"role\":\"assistant\",\"parts\":[{\"type\":\"text\",\"content\":\"On its way.\"}],\"finish_reason\":\"stop\"}]"),
                s("crucible.session.id", "session-1"),
                s("crucible.cost.total", "0.000340000"),
                s("server.address", "api.example.test"),
            ],
        }]}],
    }]});
    round_trip(&request).unwrap();
    let record = normalized(request.to_string().as_bytes(), 7)
        .spans
        .remove(0);
    assert_eq!(record.core.input_tokens, 27);
    assert_eq!(record.core.total_cost_nusd.nanos(), 340_000);
}

#[test]
fn a_failed_mcp_tool_call_comes_back_the_same() {
    let request = json!({"resourceSpans": [{
        "resource": {"attributes": [s("service.name", "docs-server"), s("service.version", "2.1.0")]},
        "scopeSpans": [{"scope": {"name": "crucible.mcp"}, "spans": [{
            "traceId": "c3a1e2f4b5d6478990a1b2c3d4e5f602",
            "spanId": "0d1e2f3a4b5c6d7f",
            "name": "tools/call search_docs",
            "kind": 2,
            "startTimeUnixNano": "1794907800050000000",
            "endTimeUnixNano": "1794907800250000000",
            "attributes": [
                s("mcp.method.name", "tools/call"),
                s("gen_ai.tool.name", "search_docs"),
                s("mcp.protocol.version", "2025-06-18"),
                s("mcp.session.id", "session-7"),
                int("jsonrpc.request.id", 12),
                int("rpc.response.status_code", -32602),
                s("error.type", "-32602"),
                s("gen_ai.tool.call.arguments", "{\"query\": 7}"),
                s("crucible.mcp.client.name", "claude-code"),
                int("crucible.mcp.content.blocks.text", 2),
                s("user.id", "u-42"),
            ],
            "status": {"code": 2, "message": "invalid params"},
        }]}],
    }]});
    round_trip(&request).unwrap();
    let frame = normalized(request.to_string().as_bytes(), 7);
    let op = &frame.mcp_ops[0].record;
    assert_eq!(&*op.method, "tools/call");
    assert_eq!(op.jsonrpc_code, Some(-32602));
    assert!(op.caller_fault && op.content_captured && op.session_key.is_some());
}
