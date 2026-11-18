//! Test oracle: the values a request's views must show, computed from the message as prost decodes
//! it, and the same values read through the views. Shared by the property tests and the fuzz targets.

use crucible_otlp::{AnyValueView, Attributes, Rejection, SpanView, TraceRequest};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};

/// An attribute value in owned form. Doubles compare by bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    Empty,
    Str(String),
    Bool(bool),
    Int(i64),
    Double(u64),
    Bytes(Vec<u8>),
    Array(Vec<Val>),
    KvList(Attrs),
}

pub type Attrs = Vec<(String, Val)>;

/// Everything a span view exposes, in owned form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub ordinal: u32,
    pub rejection: Option<Rejection>,
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    pub trace_state: String,
    pub name: String,
    pub kind: u8,
    pub start_us: i64,
    pub end_us: i64,
    pub status_code: u8,
    pub status_message: String,
    pub attributes: Attrs,
    pub events: Vec<(i64, String, Attrs)>,
    pub links: Vec<([u8; 16], [u8; 8], String, Attrs)>,
    pub resource: Attrs,
    pub scope_name: String,
    pub scope_version: String,
    pub scope_attributes: Attrs,
}

/// Every span of a scanned request, read through the views.
pub fn observe(request: &TraceRequest<'_>) -> Vec<Span> {
    request.spans().map(observe_span).collect()
}

fn observe_span(span: SpanView<'_>) -> Span {
    Span {
        ordinal: span.ordinal(),
        rejection: span.rejection(),
        trace_id: span.trace_id().to_bytes(),
        span_id: span.span_id().to_bytes(),
        parent_span_id: span.parent_span_id().map(|id| id.to_bytes()),
        trace_state: span.trace_state().to_owned(),
        name: span.name().to_owned(),
        kind: span.kind().value(),
        start_us: span.start_time().micros(),
        end_us: span.end_time().micros(),
        status_code: span.status().code().value(),
        status_message: span.status().message().to_owned(),
        attributes: observe_attrs(span.attributes()),
        events: span
            .events()
            .map(|e| {
                (
                    e.time().micros(),
                    e.name().to_owned(),
                    observe_attrs(e.attributes()),
                )
            })
            .collect(),
        links: span
            .links()
            .map(|l| {
                (
                    l.trace_id().to_bytes(),
                    l.span_id().to_bytes(),
                    l.trace_state().to_owned(),
                    observe_attrs(l.attributes()),
                )
            })
            .collect(),
        resource: observe_attrs(span.resource().attributes()),
        scope_name: span.scope().name().to_owned(),
        scope_version: span.scope().version().to_owned(),
        scope_attributes: observe_attrs(span.scope().attributes()),
    }
}

fn observe_attrs(attributes: Attributes<'_>) -> Attrs {
    attributes
        .map(|(key, value)| (key.to_owned(), observe_value(value)))
        .collect()
}

fn observe_value(value: AnyValueView<'_>) -> Val {
    match value {
        AnyValueView::Empty => Val::Empty,
        AnyValueView::String(s) => Val::Str(s.to_owned()),
        AnyValueView::Bool(b) => Val::Bool(b),
        AnyValueView::Int(i) => Val::Int(i),
        AnyValueView::Double(d) => Val::Double(d.to_bits()),
        AnyValueView::Bytes(b) => Val::Bytes(b.to_vec()),
        AnyValueView::Array(values) => Val::Array(values.map(observe_value).collect()),
        AnyValueView::KvList(pairs) => Val::KvList(observe_attrs(pairs)),
    }
}

/// Every span of a prost-decoded request, as the views must show it. `min_start_us` is the age cutoff
/// passed to the scan. Ids must have valid lengths, which a successful scan guarantees.
pub fn expect(request: &ExportTraceServiceRequest, min_start_us: Option<i64>) -> Vec<Span> {
    let mut out = Vec::new();
    for rs in &request.resource_spans {
        let resource = rs
            .resource
            .as_ref()
            .map(|r| expect_attrs(&r.attributes))
            .unwrap_or_default();
        for ss in &rs.scope_spans {
            let (scope_name, scope_version, scope_attributes) = match &ss.scope {
                Some(s) => (
                    s.name.clone(),
                    s.version.clone(),
                    expect_attrs(&s.attributes),
                ),
                None => (String::new(), String::new(), Vec::new()),
            };
            for span in &ss.spans {
                let trace_id = id::<16>(&span.trace_id);
                let span_id = id::<8>(&span.span_id);
                let start_us = micros(span.start_time_unix_nano);
                let rejection = if trace_id == [0; 16] {
                    Some(Rejection::MissingTraceId)
                } else if span_id == [0; 8] {
                    Some(Rejection::MissingSpanId)
                } else if min_start_us.is_some_and(|min| start_us < min) {
                    Some(Rejection::TooOld)
                } else {
                    None
                };
                let parent = id::<8>(&span.parent_span_id);
                let (status_code, status_message) = match &span.status {
                    Some(s) => (enum_value(s.code, 2), s.message.clone()),
                    None => (0, String::new()),
                };
                out.push(Span {
                    ordinal: u32::try_from(out.len()).unwrap_or(u32::MAX),
                    rejection,
                    trace_id,
                    span_id,
                    parent_span_id: (parent != [0; 8]).then_some(parent),
                    trace_state: span.trace_state.clone(),
                    name: span.name.clone(),
                    kind: enum_value(span.kind, 5),
                    start_us,
                    end_us: micros(span.end_time_unix_nano),
                    status_code,
                    status_message,
                    attributes: expect_attrs(&span.attributes),
                    events: span
                        .events
                        .iter()
                        .map(|e| {
                            (
                                micros(e.time_unix_nano),
                                e.name.clone(),
                                expect_attrs(&e.attributes),
                            )
                        })
                        .collect(),
                    links: span
                        .links
                        .iter()
                        .map(|l| {
                            (
                                id::<16>(&l.trace_id),
                                id::<8>(&l.span_id),
                                l.trace_state.clone(),
                                expect_attrs(&l.attributes),
                            )
                        })
                        .collect(),
                    resource: resource.clone(),
                    scope_name: scope_name.clone(),
                    scope_version: scope_version.clone(),
                    scope_attributes: scope_attributes.clone(),
                });
            }
        }
    }
    out
}

fn id<const N: usize>(bytes: &[u8]) -> [u8; N] {
    <[u8; N]>::try_from(bytes).unwrap_or([0; N])
}

fn micros(nanos: u64) -> i64 {
    i64::try_from(nanos / 1_000).unwrap_or(i64::MAX)
}

/// An OTel enum value as the views map it: values outside `0..=max` read as 0.
fn enum_value(value: i32, max: u8) -> u8 {
    u8::try_from(value).ok().filter(|v| *v <= max).unwrap_or(0)
}

fn expect_attrs(pairs: &[KeyValue]) -> Attrs {
    pairs
        .iter()
        .map(|kv| {
            let value = kv.value.as_ref().map_or(Val::Empty, expect_value);
            (kv.key.clone(), value)
        })
        .collect()
}

fn expect_value(value: &AnyValue) -> Val {
    match &value.value {
        None | Some(any_value::Value::StringValueStrindex(_)) => Val::Empty,
        Some(any_value::Value::StringValue(s)) => Val::Str(s.clone()),
        Some(any_value::Value::BoolValue(b)) => Val::Bool(*b),
        Some(any_value::Value::IntValue(i)) => Val::Int(*i),
        Some(any_value::Value::DoubleValue(d)) => Val::Double(d.to_bits()),
        Some(any_value::Value::BytesValue(b)) => Val::Bytes(b.clone()),
        Some(any_value::Value::ArrayValue(a)) => {
            Val::Array(a.values.iter().map(expect_value).collect())
        }
        Some(any_value::Value::KvlistValue(kv)) => Val::KvList(expect_attrs(&kv.values)),
    }
}
