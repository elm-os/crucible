//! Helpers shared by the integration tests. Each test crate uses a subset.
#![allow(dead_code)]

pub mod double;
pub mod generate;

use crucible_normalize::{IngestFrame, NormCtx, NormalizedFrame, PriceTable, normalize};
use crucible_records::{ApiKeyId, Lsn, PayloadCaps, ProjectId, Timestamp, WalEpoch};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message;

/// 2026-11-17T09:30:00Z in nanoseconds.
pub const BASE_NANOS: u64 = 1_794_907_800_000_000_000;

/// The `received_at` of test frames: one second after [`BASE_NANOS`].
pub const RECEIVED_AT: Timestamp = Timestamp::from_micros(1_794_907_801_000_000);

pub fn value(v: any_value::Value) -> AnyValue {
    AnyValue { value: Some(v) }
}

pub fn kv(key: &str, v: any_value::Value) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(value(v)),
        key_strindex: 0,
    }
}

pub fn s(key: &str, text: &str) -> KeyValue {
    kv(key, any_value::Value::StringValue(text.to_owned()))
}

pub fn int(key: &str, i: i64) -> KeyValue {
    kv(key, any_value::Value::IntValue(i))
}

pub fn strings(key: &str, items: &[&str]) -> KeyValue {
    kv(
        key,
        any_value::Value::ArrayValue(ArrayValue {
            values: items
                .iter()
                .map(|t| value(any_value::Value::StringValue((*t).to_owned())))
                .collect(),
        }),
    )
}

pub fn kvlist(key: &str, pairs: Vec<KeyValue>) -> KeyValue {
    kv(
        key,
        any_value::Value::KvlistValue(KeyValueList { values: pairs }),
    )
}

/// A trace id made of one repeated byte.
pub fn trace_id(t: u8) -> Vec<u8> {
    vec![t; 16]
}

/// A span id made of one repeated byte.
pub fn span_id(s: u8) -> Vec<u8> {
    vec![s; 8]
}

/// A span of trace `t` with id `id`, a child of `parent` when given, starting at [`BASE_NANOS`] and lasting
/// 250 ms.
pub fn span(t: u8, id: u8, parent: Option<u8>) -> Span {
    Span {
        trace_id: trace_id(t),
        span_id: span_id(id),
        parent_span_id: parent.map(span_id).unwrap_or_default(),
        name: format!("span {id}"),
        kind: 1,
        start_time_unix_nano: BASE_NANOS,
        end_time_unix_nano: BASE_NANOS + 250_000_000,
        status: Some(Status {
            message: String::new(),
            code: 0,
        }),
        ..Span::default()
    }
}

/// A span with attributes.
pub fn span_with(t: u8, id: u8, parent: Option<u8>, attributes: Vec<KeyValue>) -> Span {
    Span {
        attributes,
        ..span(t, id, parent)
    }
}

/// One resource and one scope holding `spans`.
pub fn request_with(resource: Vec<KeyValue>, spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: resource,
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "scope.name".to_owned(),
                    version: "1.0.0".to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

/// One resource with `service.name` and one scope holding `spans`.
pub fn request(spans: Vec<Span>) -> Vec<u8> {
    request_with(vec![s("service.name", "svc")], spans).encode_to_vec()
}

/// A frame at LSN 7 of a fixed epoch, from a fixed project and key, with no SDK headers or hint.
pub fn frame<'a>(otlp: &'a [u8], rejected: &'a [u8]) -> IngestFrame<'a> {
    IngestFrame {
        wal_epoch: WalEpoch::from_bytes([0x11; 16]),
        lsn: Lsn::new(7),
        received_at: RECEIVED_AT,
        project_id: ProjectId::from_bytes([0x22; 16]),
        api_key_id: ApiKeyId::from_bytes([0x33; 16]),
        compat_dialect: 0,
        sdk_name: "",
        sdk_version: "",
        rejected,
        otlp,
    }
}

/// The bundled price table.
pub fn prices() -> &'static PriceTable {
    PriceTable::bundled().unwrap()
}

/// The default caps and the bundled prices.
pub fn ctx() -> NormCtx<'static> {
    NormCtx::new(prices(), PayloadCaps::DEFAULT)
}

/// Normalizes a body with no rejected spans and the default context.
pub fn run(otlp: &[u8]) -> NormalizedFrame {
    normalize(&frame(otlp, &[]), &ctx()).unwrap()
}

/// A bitmap with the given ordinals set.
pub fn bitmap(ordinals: &[u32]) -> Vec<u8> {
    let len = ordinals.iter().max().map_or(0, |&o| o as usize / 8 + 1);
    let mut bits = vec![0u8; len];
    for &o in ordinals {
        bits[o as usize / 8] |= 1 << (o % 8);
    }
    bits
}
