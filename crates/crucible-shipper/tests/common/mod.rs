//! Helpers shared by the integration tests.
#![allow(dead_code)]

use bytes::Bytes;
use crucible_chfmt::{DecodedBlock, EncodedBlock, Values, decode_block, decompress};
use crucible_normalize::{Admission, IngestFrame, NormCtx, NormalizedFrame, StageB, normalize};
use crucible_records::{
    ApiKeyId, CostSource, Ingestion, IoShape, Level, Lsn, McpSide, NanoUsd, ObservationType,
    PayloadCaps, PayloadField, PayloadFields, ProjectId, SourceFormat, SpanCore, SpanId, SpanKind,
    SpanPayload, SpanRecord, StatusCode, Timestamp, TraceId, TracePublic, Truncation, UsageEntry,
    Uuid, WalEpoch,
};
use crucible_schema::TableDef;
use crucible_shipper::value::Value;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

/// 2026-11-17T09:30:00Z.
pub const BASE: i64 = 1_794_907_800_000_000;

/// The epoch of test batches.
pub const EPOCH: WalEpoch = WalEpoch::from_bytes([0xE0; 16]);

fn text(s: &str) -> Box<str> {
    s.into()
}

fn texts(items: &[&str]) -> Vec<Box<str>> {
    items.iter().map(|s| text(s)).collect()
}

/// A record in which every field has its own value, so that a cell in the wrong column shows. Every string
/// field holds its own name.
pub fn distinct() -> SpanRecord {
    let mut truncated = PayloadFields::EMPTY;
    truncated.insert(PayloadField::Output);
    truncated.insert(PayloadField::Events);
    SpanRecord {
        core: SpanCore {
            project_id: ProjectId::from_bytes([1; 16]),
            trace_id: TraceId::from_bytes([2; 16]),
            span_id: SpanId::from_bytes([3; 8]),
            parent_span_id: SpanId::from_bytes([4; 8]),
            is_root: true,
            is_app_root: false,
            kind: SpanKind::Client,
            name: text("name"),
            start_time: Timestamp::from_micros(BASE + 61_000_000),
            end_time: Timestamp::from_micros(BASE + 62_500_000),
            completion_start_time: Some(Timestamp::from_micros(BASE + 61_200_000)),
            status_code: StatusCode::Error,
            status_message: text("status_message"),
            service_name: text("service_name"),
            service_version: text("service_version"),
            scope_name: text("scope_name"),
            scope_version: text("scope_version"),
            telemetry_sdk_name: text("telemetry_sdk_name"),
            telemetry_sdk_language: text("telemetry_sdk_language"),
            telemetry_sdk_version: text("telemetry_sdk_version"),
            sdk_name: text("sdk_name"),
            sdk_version: text("sdk_version"),
            observation_type: ObservationType::Generation,
            level: Level::Warning,
            environment: text("environment"),
            release: text("release"),
            version: text("version"),
            trace_name: text("trace_name"),
            user_id: text("user_id"),
            session_id: text("session_id"),
            trace_tags: texts(&["trace_tag_a", "trace_tag_b"]),
            trace_public: TracePublic::Public,
            tags: texts(&["tag"]),
            model_requested: text("model_requested"),
            model_response: text("model_response"),
            provider: text("provider"),
            finish_reasons: texts(&["stop"]),
            usage: vec![
                UsageEntry {
                    key: text("input"),
                    units: 11,
                    cost: NanoUsd::from_nanos(110),
                },
                UsageEntry {
                    key: text("output"),
                    units: 22,
                    cost: NanoUsd::from_nanos(220),
                },
            ],
            input_tokens: 11,
            output_tokens: 22,
            total_tokens: 33,
            total_cost_nusd: NanoUsd::from_nanos(330),
            cost_source: CostSource::Provided,
            price_model_id: Some(Uuid::from_bytes([5; 16])),
            price_tier_id: Some(Uuid::from_bytes([6; 16])),
            prompt_name: text("prompt_name"),
            prompt_version: text("prompt_version"),
            tool_names: texts(&["tool_a", "tool_b", "tool_c"]),
            tool_call_count: 44,
            mcp_side: McpSide::Client,
            io_shape: IoShape::GenaiMessages,
            input_preview: text("input_preview"),
            output_preview: text("output_preview"),
            input_bytes: 55,
            output_bytes: 66,
            input_truncation: Truncation::None,
            output_truncation: Truncation::CapExceeded,
            input_sha256: Some([7; 32]),
            output_sha256: Some([8; 32]),
            source_format: SourceFormat::Compat,
            compat_dialect: 9,
            normalizer_version: 77,
            clock_skew: true,
            ingestion: Ingestion {
                wal_epoch: WalEpoch::from_bytes([10; 16]),
                batch_seq: Lsn::new(88),
                lsn: Lsn::new(99),
                received_at: Timestamp::from_micros(BASE + 70_000_000),
            },
            api_key_id: ApiKeyId::from_bytes([11; 16]),
            row_digest: 0x1234_5678_9abc_def0,
            event_bytes: 111,
        },
        payload: SpanPayload {
            input: Bytes::from_static(b"input"),
            output: Bytes::from_static(b"output"),
            system_instructions: Bytes::from_static(b"system_instructions"),
            metadata: Bytes::from_static(b"metadata"),
            metadata_keys: texts(&["k1", "k2"]),
            metadata_values: texts(&["v1", "v2"]),
            metadata_kv: texts(&["k1=v1", "k2=v2"]),
            trace_metadata: Bytes::from_static(b"trace_metadata"),
            attributes: Bytes::from_static(b"attributes"),
            resource_attributes: Bytes::from_static(b"resource_attributes"),
            events: Bytes::from_static(b"events"),
            links: Bytes::from_static(b"links"),
            tool_definitions: Bytes::from_static(b"tool_definitions"),
            tool_calls: Bytes::from_static(b"tool_calls"),
            model_parameters: Bytes::from_static(b"model_parameters"),
            usage_provided: vec![(text("input"), 12), (text("total"), 13)],
            cost_provided: vec![(text("output"), NanoUsd::from_nanos(14))],
            trace_state: Bytes::from_static(b"trace_state"),
            truncated_fields: truncated,
        },
    }
}

/// The cell of one decoded row, in the vocabulary of the builders.
pub fn cell(values: &Values, row: usize) -> Value {
    match values {
        Values::UInt8(v) => Value::U8(v[row]),
        Values::UInt16(v) => Value::U16(v[row]),
        Values::UInt32(v) => Value::U32(v[row]),
        Values::UInt64(v) => Value::U64(v[row]),
        Values::Int8(v) => Value::I8(v[row]),
        Values::Int32(v) => Value::I32(v[row]),
        Values::Int64(v) => Value::I64(v[row]),
        Values::Float32(v) => Value::F32(f32::from_bits(v[row])),
        Values::Float64(v) => Value::F64(f64::from_bits(v[row])),
        Values::Uuid(v) => Value::Uuid(v[row]),
        Values::String(v) | Values::FixedString(_, v) => Value::bytes(&v[row]),
        Values::Nullable(nulls, inner) => {
            if nulls[row] {
                Value::Null
            } else {
                cell(inner, row)
            }
        }
        Values::Array(offsets, inner) => {
            let start = if row == 0 {
                0
            } else {
                offsets[row - 1] as usize
            };
            Value::Array(
                (start..offsets[row] as usize)
                    .map(|i| cell(inner, i))
                    .collect(),
            )
        }
        Values::Tuple(elements) => Value::Tuple(Box::new((
            cell(&elements[0], row),
            cell(&elements[1], row),
        ))),
    }
}

/// Decodes a block and checks its header: every column of the table, in `TableDef` order, with the header type
/// of contract 5.2.
pub fn decode(def: &TableDef, block: &EncodedBlock) -> DecodedBlock {
    let native = decompress(block.compressed()).unwrap();
    assert_eq!(native.len(), block.uncompressed_size());
    let (decoded, used) = decode_block(&native).unwrap();
    assert_eq!(used, native.len());
    assert_eq!(decoded.rows, block.rows());
    assert_eq!(decoded.columns.len(), def.columns.len());
    for (sent, column) in decoded.columns.iter().zip(def.columns) {
        assert_eq!(sent.name, column.name);
        assert_eq!(sent.ty, column.ty.wire().to_string());
    }
    decoded
}

/// The rows of a decoded block.
pub fn decoded_rows(decoded: &DecodedBlock) -> Vec<Vec<Value>> {
    (0..decoded.rows)
        .map(|row| {
            decoded
                .columns
                .iter()
                .map(|c| cell(&c.values, row))
                .collect()
        })
        .collect()
}

/// The position of a column in its table.
pub fn column_index(def: &TableDef, name: &str) -> usize {
    def.columns.iter().position(|c| c.name == name).unwrap()
}

/// One generated span: (trace, span, has a parent, day offset, minute offset, duration in ms).
pub type SpanDesc = (u8, u8, bool, u8, u16, u16);

/// An OTLP request with the described spans.
pub fn otlp(spans: &[SpanDesc]) -> Vec<u8> {
    let wire = spans
        .iter()
        .map(|&(trace, span, child, day, minute, millis)| {
            let start =
                (BASE as u64 + u64::from(day) * 86_400_000_000 + u64::from(minute) * 60_000_000)
                    * 1_000;
            Span {
                trace_id: vec![trace + 1; 16],
                span_id: vec![span + 1; 8],
                parent_span_id: if child { vec![200; 8] } else { Vec::new() },
                name: format!("span-{span}"),
                kind: 1,
                start_time_unix_nano: start,
                end_time_unix_nano: start + u64::from(millis) * 1_000_000,
                ..Span::default()
            }
        })
        .collect();
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: None,
            scope_spans: vec![ScopeSpans {
                scope: None,
                spans: wire,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
    .encode_to_vec()
}

/// Normalizes frames at LSNs 10, 11, ... through stage A and stage B, stamped with batch 10.
pub fn normalized(frames: &[Vec<SpanDesc>]) -> Vec<(NormalizedFrame, Admission)> {
    let mut stage_b = StageB::default();
    frames
        .iter()
        .enumerate()
        .map(|(i, spans)| {
            let body = otlp(spans);
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
            let mut normalized =
                normalize(&frame, &NormCtx::new(0, PayloadCaps::DEFAULT)).unwrap();
            let admission = stage_b.admit(&mut normalized).unwrap();
            normalized.stamp_batch_seq(Lsn::new(10));
            (normalized, admission)
        })
        .collect()
}
