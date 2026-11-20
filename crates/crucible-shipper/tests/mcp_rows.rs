//! The rows of the six MCP tables (contract sections 3.5 to 3.7, 3.10 and 6.4): every field in its column, and
//! the blocks of generated MCP traffic, decoded again, against what the operations say.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use common::{BASE, EPOCH, column_index, decode, decoded_rows};
use crucible_chfmt::Compression;
use crucible_normalize::{
    Admission, IdentityCtx, IngestFrame, NormCtx, NormalizedFrame, PriceTable, ProjectSalt, StageB,
    normalize,
};
use crucible_records::{
    ApiKeyId, Bucket, CancelSource, CaptureSource, Direction, Era, ErrorClass, ErrorSubclass,
    GroupKeyKind, IdentitySource, Ingestion, Lsn, McpDeployRecord, McpOpRecord, McpSide, McpStatus,
    McpTargetKind, McpToolInventoryRecord, PartitionId, PayloadCaps, ProjectId, ResultType, SpanId,
    TableId, Timestamp, TraceId, Transport, Truncation, WalEpoch, semconv_bucket,
};
use crucible_schema::{Granularity, TableKind, route, table};
use crucible_shipper::tables::{
    RoutedRow, derive, mcp_content_row, mcp_deploy, mcp_deploys_row, mcp_ops_row,
    mcp_tool_inventory, mcp_tool_inventory_row, mcp_tool_rollup, mcp_tool_rollup_hourly_row,
    mcp_tool_rollup_row,
};
use crucible_shipper::value::{Row, Value};
use crucible_shipper::{BatchId, pair_block};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use proptest::prelude::*;
use prost::Message;

fn s(v: &str) -> Value {
    Value::str(v)
}

/// Asserts that `cells` holds, for every column of the table and in its order, the value `expected` names.
fn assert_columns(table_id: TableId, cells: &[Value], expected: &[(&str, Value)]) {
    let def = table(table_id);
    let by_name: BTreeMap<&str, &Value> = expected.iter().map(|(n, v)| (*n, v)).collect();
    assert_eq!(by_name.len(), expected.len(), "a column is named twice");
    assert_eq!(cells.len(), def.columns.len(), "{}", def.name());
    for (column, cell) in def.columns.iter().zip(cells) {
        let want = by_name
            .get(column.name)
            .unwrap_or_else(|| panic!("{}.{} has no expected value", def.name(), column.name));
        assert_eq!(cell, *want, "{}.{}", def.name(), column.name);
    }
    assert_eq!(
        expected.len(),
        def.columns.len(),
        "an expected column does not exist"
    );
}

/// An operation with a distinct value in every field.
fn distinct() -> McpOpRecord {
    McpOpRecord {
        project_id: ProjectId::from_bytes([1; 16]),
        trace_id: TraceId::from_bytes([2; 16]),
        span_id: SpanId::from_bytes([3; 8]),
        parent_span_id: SpanId::from_bytes([4; 8]),
        transport_link_trace_id: TraceId::from_bytes([5; 16]),
        transport_link_span_id: SpanId::from_bytes([6; 8]),
        server_name: "server_name".into(),
        server_version: "server_version".into(),
        deploy_id: "deploy_id".into(),
        instance_id: "instance_id".into(),
        side: McpSide::Server,
        capture_source: CaptureSource::ProxyHttp,
        era: Era::Modern,
        protocol_version: "protocol_version".into(),
        transport: Transport::StreamableHttp,
        method: "method".into(),
        target_kind: McpTargetKind::Prompt,
        target_name: "target_name".into(),
        jsonrpc_id: "jsonrpc_id".into(),
        direction: Direction::ServerToClient,
        is_notification: true,
        is_batch: true,
        result_type: ResultType::Task,
        mrtr_round: 7,
        mrtr_state_in: Some([8; 16]),
        mrtr_state_out: Some([9; 16]),
        task_id: "task_id".into(),
        subscription_id: "subscription_id".into(),
        start_time: Timestamp::from_micros(BASE + 61_000_000),
        duration_us: 1_500_000,
        progress_count: Some(11),
        cancel_source: CancelSource::StreamClose,
        timed_out: true,
        status: McpStatus::ToolError,
        error_class: ErrorClass::ToolExecution,
        error_subclass: ErrorSubclass::Upstream,
        jsonrpc_code: Some(-32602),
        http_status: Some(429),
        caller_fault: true,
        error_type: "error_type".into(),
        error_message: "error_message".into(),
        client_name: "client_name".into(),
        client_version: "client_version".into(),
        client_capabilities_hash: 0xfeed_face_cafe_beef,
        oauth_client_id: "oauth_client_id".into(),
        user_subject_hash: Some([12; 16]),
        user_issuer: "user_issuer".into(),
        user_id: "user_id".into(),
        tenant: "tenant".into(),
        identity_source: IdentitySource::TokenVerified,
        session_key: Some([0xab; 16]),
        handle_id: "handle_id".into(),
        group_key: "group_key".into(),
        group_key_kind: GroupKeyKind::Handle,
        ip_prefix: "ip_prefix".into(),
        user_agent: "user_agent".into(),
        request_bytes: 13,
        response_bytes: 14,
        content_blocks: vec![("image".into(), 2), ("text".into(), 5)],
        content_captured: true,
        content_sample_rate: 0.25,
        content_truncation: Truncation::CapExceeded,
        args: Bytes::from_static(b"{\"args\":1}"),
        result: Bytes::from_static(b"{\"result\":2}"),
        args_bytes: 15,
        result_bytes: 16,
        ingestion: Ingestion {
            wal_epoch: WalEpoch::from_bytes([10; 16]),
            batch_seq: Lsn::new(88),
            lsn: Lsn::new(99),
            received_at: Timestamp::from_micros(BASE + 70_000_000),
        },
        normalizer_version: 5,
    }
}

fn ingestion_columns() -> Vec<(&'static str, Value)> {
    vec![
        ("wal_epoch", Value::Uuid([10; 16])),
        ("batch_seq", Value::U64(88)),
        ("lsn", Value::U64(99)),
        ("received_at", Value::I64(BASE + 70_000_000)),
    ]
}

/// 09:31:01 falls in the minute that starts 60 s after the base.
fn start_minute() -> Value {
    Value::U32(((BASE + 60_000_000) / 1_000_000) as u32)
}

fn no_marker() -> Vec<(&'static str, Value)> {
    vec![
        ("marker_epoch", Value::Uuid([0; 16])),
        ("marker_batch_seq", Value::U64(0)),
        ("marker_at", Value::U32(0)),
    ]
}

#[test]
fn mcp_ops_puts_every_field_in_its_column() {
    let op = distinct();
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("side", Value::I8(2)),
        ("capture_source", Value::I8(3)),
        ("era", Value::I8(2)),
        ("transport", Value::I8(2)),
        ("target_kind", Value::I8(2)),
        ("direction", Value::I8(2)),
        ("result_type", Value::I8(3)),
        ("cancel_source", Value::I8(2)),
        ("status", Value::I8(1)),
        ("error_class", Value::I8(4)),
        ("error_subclass", Value::I8(3)),
        ("identity_source", Value::I8(3)),
        ("group_key_kind", Value::I8(2)),
        ("content_truncation", Value::I8(1)),
        ("start_minute", start_minute()),
        (
            "trace_hash",
            Value::U32(TraceId::from_bytes([2; 16]).trace_hash()),
        ),
        ("start_time", Value::I64(BASE + 61_000_000)),
        ("span_id", Value::bytes(&[3; 8])),
        ("parent_span_id", Value::bytes(&[4; 8])),
        ("transport_link_span_id", Value::bytes(&[6; 8])),
        ("trace_id", Value::bytes(&[2; 16])),
        ("transport_link_trace_id", Value::bytes(&[5; 16])),
        ("user_subject_hash", Value::bytes(&[12; 16])),
        ("mrtr_state_in", Value::bytes(&[8; 16])),
        ("mrtr_state_out", Value::bytes(&[9; 16])),
        // The session key is stored as its 32 hexadecimal characters.
        ("session_key", s(&"ab".repeat(16))),
        ("is_notification", Value::U8(1)),
        ("is_batch", Value::U8(1)),
        ("timed_out", Value::U8(1)),
        ("caller_fault", Value::U8(1)),
        ("content_captured", Value::U8(1)),
        ("mrtr_round", Value::U16(7)),
        ("duration_us", Value::U64(1_500_000)),
        ("request_bytes", Value::U64(13)),
        ("response_bytes", Value::U64(14)),
        (
            "client_capabilities_hash",
            Value::U64(0xfeed_face_cafe_beef),
        ),
        ("progress_count", Value::U32(11)),
        ("jsonrpc_code", Value::I32(-32602)),
        ("http_status", Value::U16(429)),
        (
            "content_block_types",
            Value::Array(vec![s("image"), s("text")]),
        ),
        (
            "content_block_counts",
            Value::Array(vec![Value::U32(2), Value::U32(5)]),
        ),
        ("content_sample_rate", Value::F32(0.25)),
        ("normalizer_version", Value::U32(5)),
    ];
    // Every string column holds the name of its field.
    for name in [
        "server_name",
        "method",
        "target_name",
        "client_name",
        "server_version",
        "deploy_id",
        "instance_id",
        "protocol_version",
        "jsonrpc_id",
        "task_id",
        "subscription_id",
        "error_type",
        "error_message",
        "client_version",
        "oauth_client_id",
        "user_id",
        "user_issuer",
        "tenant",
        "handle_id",
        "group_key",
        "ip_prefix",
        "user_agent",
    ] {
        expected.push((name, s(name)));
    }
    expected.extend(ingestion_columns());
    assert_columns(TableId::McpOps, &mcp_ops_row(&op), &expected);
}

#[test]
fn mcp_ops_writes_absent_optionals_as_null_zero_and_empty() {
    let mut op = distinct();
    op.progress_count = None;
    op.jsonrpc_code = None;
    op.http_status = None;
    op.user_subject_hash = None;
    op.mrtr_state_in = None;
    op.mrtr_state_out = None;
    op.session_key = None;
    op.parent_span_id = SpanId::ZERO;
    op.transport_link_trace_id = TraceId::ZERO;
    let def = table(TableId::McpOps);
    let cells = mcp_ops_row(&op);
    let at = |name: &str| &cells[column_index(def, name)];
    for name in ["progress_count", "jsonrpc_code", "http_status"] {
        assert_eq!(at(name), &Value::Null, "{name}");
    }
    for name in [
        "user_subject_hash",
        "mrtr_state_in",
        "mrtr_state_out",
        "transport_link_trace_id",
    ] {
        assert_eq!(at(name), &Value::bytes(&[0; 16]), "{name}");
    }
    assert_eq!(at("parent_span_id"), &Value::bytes(&[0; 8]));
    assert_eq!(at("session_key"), &s(""));
}

#[test]
fn mcp_content_puts_every_field_in_its_column() {
    let op = distinct();
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("side", Value::I8(2)),
        ("start_minute", start_minute()),
        (
            "trace_hash",
            Value::U32(TraceId::from_bytes([2; 16]).trace_hash()),
        ),
        ("span_id", Value::bytes(&[3; 8])),
        ("trace_id", Value::bytes(&[2; 16])),
        ("start_time", Value::I64(BASE + 61_000_000)),
        ("user_id", s("user_id")),
        ("user_subject_hash", Value::bytes(&[12; 16])),
        ("target_name", s("target_name")),
        ("args", s("{\"args\":1}")),
        ("result", s("{\"result\":2}")),
        ("args_bytes", Value::U64(15)),
        ("result_bytes", Value::U64(16)),
        ("content_truncation", Value::I8(1)),
    ];
    expected.extend(ingestion_columns());
    assert_columns(TableId::McpContent, &mcp_content_row(&op), &expected);
}

fn hist16(micros: u64) -> Value {
    Value::Tuple(Box::new((
        Value::Array(vec![Value::U16(Bucket::of(micros).index())]),
        Value::Array(vec![Value::U64(1)]),
    )))
}

#[test]
fn the_mcp_partials_put_every_measure_in_its_column() {
    let op = distinct();
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("side", Value::I8(2)),
        ("minute", start_minute()),
        ("server_name", s("server_name")),
        ("deploy_id", s("deploy_id")),
        ("capture_source", Value::I8(3)),
        ("era", Value::I8(2)),
        ("method", s("method")),
        ("target_name", s("target_name")),
        ("client_name", s("client_name")),
        ("error_class", Value::I8(4)),
        ("caller_fault", Value::U8(1)),
        ("count", Value::U64(1)),
        ("request_bytes", Value::U64(13)),
        ("response_bytes", Value::U64(14)),
        ("dur_hist", hist16(1_500_000)),
        // 1.5 s falls in the bucket above 1 s and at or below 2 s.
        (
            "semconv_hist",
            Value::Tuple(Box::new((
                Value::Array(vec![Value::U8(semconv_bucket(1_500_000))]),
                Value::Array(vec![Value::U64(1)]),
            ))),
        ),
        ("last_seen", Value::I64(BASE + 61_000_000)),
    ];
    expected.extend(no_marker());
    assert_eq!(semconv_bucket(1_500_000), 7);
    assert_columns(
        TableId::McpToolRollup,
        &mcp_tool_rollup_row(&mcp_tool_rollup(&op)),
        &expected,
    );
    // The hourly partial holds the same measures under the start of the operation's hour.
    let hour = ((BASE + 61_000_000) / 3_600_000_000 * 3_600) as u32;
    let hourly: Vec<(&str, Value)> = expected
        .iter()
        .map(|(name, value)| match *name {
            "minute" => ("hour", Value::U32(hour)),
            _ => (*name, value.clone()),
        })
        .collect();
    assert_columns(
        TableId::McpToolRollupHourly,
        &mcp_tool_rollup_hourly_row(&mcp_tool_rollup(&op)),
        &hourly,
    );

    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("server_name", s("server_name")),
        ("deploy_id", s("deploy_id")),
        ("server_version", s("server_version")),
        ("capture_source", Value::I8(3)),
        ("first_seen", Value::I64(BASE + 61_000_000)),
        ("last_seen", Value::I64(BASE + 61_000_000)),
    ];
    expected.extend(no_marker());
    let deploy: McpDeployRecord = mcp_deploy(&op).unwrap();
    assert_columns(TableId::McpDeploys, &mcp_deploys_row(&deploy), &expected);
    // Only server-side rows record a deploy (contract 3.7).
    let mut client = distinct();
    client.side = McpSide::Client;
    assert!(mcp_deploy(&client).is_none());

    let record = McpToolInventoryRecord {
        project_id: ProjectId::from_bytes([1; 16]),
        server_name: "server_name".into(),
        deploy_id: "deploy_id".into(),
        tool_name: "tool_name".into(),
        schema_hash: 0xdead_beef,
        capture_source: CaptureSource::ProxyStdio,
        first_seen: Timestamp::from_micros(BASE + 1),
        last_seen: Timestamp::from_micros(BASE + 2),
    };
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("server_name", s("server_name")),
        ("deploy_id", s("deploy_id")),
        ("tool_name", s("tool_name")),
        ("schema_hash", Value::U64(0xdead_beef)),
        ("first_seen", Value::I64(BASE + 1)),
        ("last_seen", Value::I64(BASE + 2)),
        ("capture_sources", Value::Array(vec![Value::U8(2)])),
    ];
    expected.extend(no_marker());
    assert_columns(
        TableId::McpToolInventory,
        &mcp_tool_inventory_row(&mcp_tool_inventory(&record)),
        &expected,
    );
}

fn text(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_owned())),
        }),
        ..KeyValue::default()
    }
}

/// One generated MCP operation: (span id, server side, capture source, tool, JSON-RPC code or success, minute
/// offset, duration in ms, captures content, month offset).
type OpDesc = (u8, bool, u8, u8, Option<i8>, u8, u16, bool, bool);

const CODES: [i32; 3] = [-32602, -32603, -32002];

fn op_span(desc: &OpDesc) -> Span {
    let &(id, server, source, tool, code, minute, millis, captures, later) = desc;
    let start = (BASE as u64
        + u64::from(minute) * 60_000_000
        + if later { 31 * 86_400_000_000 } else { 0 })
        * 1_000;
    let mut attributes = vec![
        text("mcp.method.name", "tools/call"),
        text("gen_ai.tool.name", &format!("tool-{tool}")),
        text(
            "crucible.mcp.capture_source",
            CaptureSource::ALL[usize::from(source) % 4].as_str(),
        ),
        text("crucible.mcp.server.version", &format!("1.{}", tool % 2)),
    ];
    if let Some(code) = code {
        let code = CODES[code.unsigned_abs() as usize % 3].to_string();
        attributes.push(text("rpc.response.status_code", &code));
        attributes.push(text("error.type", &code));
    }
    if captures {
        attributes.push(text("gen_ai.tool.call.arguments", "{\"q\":1}"));
    }
    if tool == 0 {
        attributes.push(text(
            "crucible.mcp.tools",
            "[{\"name\":\"tool-0\"},{\"name\":\"tool-1\",\"inputSchema\":{\"type\":\"object\"}}]",
        ));
    }
    Span {
        trace_id: vec![1 + id % 3; 16],
        span_id: vec![1 + id; 8],
        kind: if server { 2 } else { 3 },
        name: "tools/call".to_owned(),
        start_time_unix_nano: start,
        end_time_unix_nano: start + u64::from(millis) * 1_000_000,
        attributes,
        ..Span::default()
    }
}

/// Normalizes frames of MCP spans at LSNs 10, 11, ... through stage A and stage B, stamped with batch 10.
fn normalized(frames: &[Vec<OpDesc>]) -> Vec<(NormalizedFrame, Admission)> {
    let mut stage_b = StageB::default();
    let salt = ProjectSalt::new([4; 32]);
    frames
        .iter()
        .enumerate()
        .map(|(i, ops)| {
            let body = ExportTraceServiceRequest {
                resource_spans: vec![ResourceSpans {
                    resource: Some(Resource {
                        attributes: vec![text("service.name", "orders")],
                        ..Resource::default()
                    }),
                    scope_spans: vec![ScopeSpans {
                        spans: ops.iter().map(op_span).collect(),
                        ..ScopeSpans::default()
                    }],
                    ..ResourceSpans::default()
                }],
            }
            .encode_to_vec();
            let frame = IngestFrame {
                wal_epoch: EPOCH,
                lsn: Lsn::new(10 + i as u64),
                received_at: Timestamp::from_micros(BASE + 5_000_000),
                project_id: ProjectId::from_bytes([1 + (i % 2) as u8; 16]),
                api_key_id: ApiKeyId::from_bytes([3; 16]),
                compat_dialect: 0,
                sdk_name: "",
                sdk_version: "",
                rejected: &[],
                otlp: &body,
            };
            let mut normalized = normalize(
                &frame,
                &NormCtx::new(PriceTable::bundled().unwrap(), PayloadCaps::DEFAULT),
            )
            .unwrap();
            let admission = stage_b
                .admit(&mut normalized, &IdentityCtx::Salted(Some(&salt)))
                .unwrap();
            normalized.stamp_batch_seq(Lsn::new(10));
            (normalized, admission)
        })
        .collect()
}

/// Per rollup key: the count, both histograms and the last start.
type ToolAggs = BTreeMap<String, (u64, BTreeMap<u16, u64>, BTreeMap<u8, u64>, i64)>;

fn arb_frames() -> impl Strategy<Value = Vec<Vec<OpDesc>>> {
    let op = (
        0u8..24,
        any::<bool>(),
        0u8..4,
        0u8..3,
        prop::option::of(0i8..3),
        0u8..3,
        0u16..4_000,
        any::<bool>(),
        prop::bool::weighted(0.2),
    );
    proptest::collection::vec(proptest::collection::vec(op, 1..8), 1..6)
}

/// The key of a rollup row as a reader groups it: every ORDER BY column but the marker columns.
fn rollup_key(op: &McpOpRecord) -> Vec<Value> {
    mcp_tool_rollup_row(&mcp_tool_rollup(op))[..12].to_vec()
}

/// The key of an hourly rollup row as a reader groups it.
fn hourly_key(op: &McpOpRecord) -> Vec<Value> {
    mcp_tool_rollup_hourly_row(&mcp_tool_rollup(op))[..12].to_vec()
}

proptest! {
    /// Every MCP row routes through `crucible_schema::route`: operations and content by day (or week), the
    /// tool rollup by month, and deploys and inventory to the one partition of an unpartitioned table.
    #[test]
    fn mcp_rows_route_by_their_table(frames in arb_frames(), weekly in any::<bool>()) {
        let granularity = if weekly { Granularity::Week } else { Granularity::Day };
        for (frame, admission) in normalized(&frames) {
            let rows = derive(&frame, &admission, granularity).unwrap();
            let count = |table| rows.iter().filter(|r| r.table == table).count();
            prop_assert_eq!(count(TableId::McpOps), frame.mcp_ops.len());
            prop_assert_eq!(count(TableId::McpToolRollup), frame.mcp_ops.len());
            prop_assert_eq!(count(TableId::McpToolRollupHourly), frame.mcp_ops.len());
            prop_assert_eq!(count(TableId::SpanCore), frame.spans.len());
            let captured = frame.mcp_ops.iter().filter(|op| op.record.content_captured).count();
            prop_assert_eq!(count(TableId::McpContent), captured);
            let served = frame.mcp_ops.iter().filter(|op| op.record.side == McpSide::Server).count();
            prop_assert_eq!(count(TableId::McpDeploys), served);
            prop_assert_eq!(count(TableId::McpToolInventory), frame.inventory.len());
            for routed in &rows {
                prop_assert_eq!(routed.row.lsn, frame.lsn);
                match routed.table {
                    TableId::McpOps
                    | TableId::McpContent
                    | TableId::McpToolRollup
                    | TableId::McpToolRollupHourly
                    | TableId::McpDeploys => {
                        let span = &frame.spans[routed.row.ordinal as usize];
                        let expected = route(routed.table, granularity, span.core.start_time).unwrap();
                        prop_assert_eq!(routed.partition, expected);
                        let date = span.core.start_time.date();
                        let partition = match routed.table {
                            TableId::McpToolRollup | TableId::McpToolRollupHourly => {
                                PartitionId::month(date.year(), date.month()).unwrap()
                            }
                            TableId::McpDeploys => PartitionId::UNPARTITIONED,
                            _ if weekly => PartitionId::date(date.monday_on_or_before()),
                            _ => PartitionId::date(date),
                        };
                        prop_assert_eq!(routed.partition, partition);
                    }
                    TableId::McpToolInventory => {
                        prop_assert_eq!(routed.partition, PartitionId::UNPARTITIONED);
                    }
                    _ => {}
                }
            }
        }
    }

    /// The block of every MCP pair, decoded again, holds what the operations say: one `mcp_ops` row per
    /// operation and one `mcp_content` row per captured one; the tool rollup merged by its key, the capture
    /// source included, so that rows of different sources are never summed; one deploy row per distinct
    /// deploy of the server side; one inventory row per distinct tool; and one marker row per aggregating
    /// block.
    #[test]
    fn mcp_blocks_hold_what_the_operations_say(frames in arb_frames(), zstd in any::<bool>()) {
        let compression = if zstd { Compression::Zstd } else { Compression::Lz4 };
        let batch = BatchId { epoch: EPOCH, first_lsn: Lsn::new(10), last_lsn: Lsn::new(20) };
        let marker_at = Timestamp::from_micros(BASE + 5_000_000);
        let mut pairs: BTreeMap<(TableId, PartitionId), Vec<Row>> = BTreeMap::new();
        let mut ops: Vec<McpOpRecord> = Vec::new();
        let mut tools: BTreeSet<Vec<u8>> = BTreeSet::new();
        for (frame, admission) in normalized(&frames) {
            ops.extend(frame.mcp_ops.iter().map(|op| op.record.clone()));
            for entry in &frame.inventory {
                let r = &entry.record;
                let mut key = r.project_id.to_bytes().to_vec();
                key.extend_from_slice(r.tool_name.as_bytes());
                key.extend_from_slice(&r.schema_hash.to_le_bytes());
                tools.insert(key);
            }
            for routed in derive(&frame, &admission, Granularity::Day).unwrap() {
                let RoutedRow { table, partition, row } = routed;
                pairs.entry((table, partition)).or_default().push(row);
            }
        }
        // The reference: per rollup key, the count, the byte sums, the durations and the last start; the same
        // per hourly key.
        let mut expected: ToolAggs = BTreeMap::new();
        let mut expected_hourly: ToolAggs = BTreeMap::new();
        let mut deploys: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for op in &ops {
            for (aggs, key) in [(&mut expected, rollup_key(op)), (&mut expected_hourly, hourly_key(op))] {
                let entry = aggs.entry(format!("{key:?}")).or_default();
                entry.0 += 1;
                *entry.1.entry(Bucket::of(op.duration_us).index()).or_default() += 1;
                *entry.2.entry(semconv_bucket(op.duration_us)).or_default() += 1;
                entry.3 = entry.3.max(op.start_time.micros());
            }
            if let Some(deploy) = mcp_deploy(op) {
                let key = format!("{:?}", &mcp_deploys_row(&deploy)[..5]);
                let seen = deploys.entry(key).or_insert((i64::MAX, i64::MIN));
                seen.0 = seen.0.min(op.start_time.micros());
                seen.1 = seen.1.max(op.start_time.micros());
            }
        }
        let (mut op_rows, mut content_rows, mut inventory_rows) = (0usize, 0usize, 0usize);
        let mut found: ToolAggs = BTreeMap::new();
        let mut found_hourly: ToolAggs = BTreeMap::new();
        let mut found_deploys: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        let mut per_source: BTreeMap<i8, u64> = BTreeMap::new();
        let mcp = [
            TableId::McpOps,
            TableId::McpContent,
            TableId::McpToolRollup,
            TableId::McpToolRollupHourly,
            TableId::McpDeploys,
            TableId::McpToolInventory,
        ];
        for (pair, rows) in pairs {
            if !mcp.contains(&pair.0) {
                continue;
            }
            let def = table(pair.0);
            let block = pair_block(&batch, pair, rows, marker_at, compression).unwrap();
            let decoded = decoded_rows(&decode(def, &block));
            let project = column_index(def, "project_id");
            let (markers, data): (Vec<_>, Vec<_>) =
                decoded.iter().partition(|row| row[project] == Value::Uuid([0; 16]));
            prop_assert_eq!(markers.len(), usize::from(def.kind == TableKind::Aggregating));
            for marker in markers {
                prop_assert_eq!(&marker[column_index(def, "marker_epoch")], &Value::Uuid(EPOCH.to_bytes()));
                prop_assert_eq!(&marker[column_index(def, "marker_batch_seq")], &Value::U64(10));
            }
            let at = |row: &[Value], name: &str| row[column_index(def, name)].clone();
            let map16 = |cell: Value| -> BTreeMap<u16, u64> {
                let Value::Tuple(pair) = cell else { return BTreeMap::new() };
                let (Value::Array(keys), Value::Array(counts)) = *pair else { return BTreeMap::new() };
                keys.into_iter()
                    .zip(counts)
                    .filter_map(|(k, c)| match (k, c) {
                        (Value::U16(k), Value::U64(c)) => Some((k, c)),
                        (Value::U8(k), Value::U64(c)) => Some((u16::from(k), c)),
                        _ => None,
                    })
                    .collect()
            };
            match pair.0 {
                TableId::McpOps => {
                    op_rows += data.len();
                    prop_assert!(data.iter().all(|row| at(row, "batch_seq") == Value::U64(10)));
                }
                TableId::McpContent => content_rows += data.len(),
                TableId::McpToolRollup | TableId::McpToolRollupHourly => {
                    // Within a block every key appears once: the shipper combined the partials.
                    let keys: BTreeSet<String> = data.iter().map(|row| format!("{:?}", &row[..12])).collect();
                    prop_assert_eq!(keys.len(), data.len());
                    let hourly = pair.0 == TableId::McpToolRollupHourly;
                    for row in data {
                        let aggs = if hourly { &mut found_hourly } else { &mut found };
                        let entry = aggs.entry(format!("{:?}", &row[..12])).or_default();
                        let Value::U64(count) = at(row, "count") else { panic!("count") };
                        entry.0 += count;
                        for (bucket, n) in map16(at(row, "dur_hist")) {
                            *entry.1.entry(bucket).or_default() += n;
                        }
                        for (bucket, n) in map16(at(row, "semconv_hist")) {
                            *entry.2.entry(bucket as u8).or_default() += n;
                        }
                        let Value::I64(last) = at(row, "last_seen") else { panic!("last_seen") };
                        entry.3 = entry.3.max(last);
                        let Value::I8(source) = at(row, "capture_source") else { panic!("source") };
                        if !hourly {
                            *per_source.entry(source).or_default() += count;
                        }
                    }
                }
                TableId::McpDeploys => {
                    for row in data {
                        let (Value::I64(first), Value::I64(last)) = (at(row, "first_seen"), at(row, "last_seen"))
                        else {
                            panic!("seen")
                        };
                        let key = format!("{:?}", &row[..5]);
                        let fresh = found_deploys.insert(key, (first, last)).is_none();
                        prop_assert!(fresh);
                    }
                }
                _ => inventory_rows += data.len(),
            }
        }
        prop_assert_eq!(op_rows, ops.len());
        prop_assert_eq!(content_rows, ops.iter().filter(|op| op.content_captured).count());
        prop_assert_eq!(&found, &expected);
        prop_assert_eq!(&found_hourly, &expected_hourly);
        prop_assert_eq!(&found_deploys, &deploys);
        prop_assert_eq!(inventory_rows, tools.len());
        // Per capture source the rollup counts the operations of that source and no other.
        for source in CaptureSource::ALL {
            let sent = ops.iter().filter(|op| op.capture_source == *source).count() as u64;
            prop_assert_eq!(per_source.get(&(source.value() as i8)).copied().unwrap_or(0), sent);
        }
    }
}
