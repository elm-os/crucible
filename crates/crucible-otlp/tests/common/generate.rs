//! Proptest strategies for random `ExportTraceServiceRequest`s. Ids are empty, all zero or random, so
//! every rejection path is exercised; enum values stray outside their ranges; strings are arbitrary
//! Unicode. With `finite` set, doubles are finite (OTLP/JSON has no lossless form for the others).

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, EntityRef, InstrumentationScope, KeyValue, KeyValueList, any_value,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::{Event, Link};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

fn text() -> impl Strategy<Value = String> {
    vec(any::<char>(), 0..8).prop_map(String::from_iter)
}

fn id(len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => Just(Vec::new()),
        1 => Just(vec![0; len]),
        6 => vec(any::<u8>(), len),
    ]
}

fn enum_value(max: i32) -> impl Strategy<Value = i32> {
    prop_oneof![4 => 0..=max, 1 => any::<i32>()]
}

fn double(finite: bool) -> BoxedStrategy<f64> {
    if finite {
        prop_oneof![
            any::<f64>().prop_filter("finite", |d| d.is_finite()),
            Just(-0.0),
            Just(f64::MIN_POSITIVE / 4.0),
        ]
        .boxed()
    } else {
        any::<u64>().prop_map(f64::from_bits).boxed()
    }
}

fn leaf(finite: bool) -> impl Strategy<Value = AnyValue> {
    use any_value::Value as V;
    prop_oneof![
        Just(None),
        text().prop_map(|s| Some(V::StringValue(s))),
        any::<bool>().prop_map(|b| Some(V::BoolValue(b))),
        any::<i64>().prop_map(|i| Some(V::IntValue(i))),
        double(finite).prop_map(|d| Some(V::DoubleValue(d))),
        vec(any::<u8>(), 0..8).prop_map(|b| Some(V::BytesValue(b))),
        any::<i32>().prop_map(|i| Some(V::StringValueStrindex(i))),
    ]
    .prop_map(|value| AnyValue { value })
}

fn pair(value: impl Strategy<Value = AnyValue>) -> impl Strategy<Value = KeyValue> {
    (
        text(),
        option::of(value),
        prop_oneof![3 => Just(0), 1 => any::<i32>()],
    )
        .prop_map(|(key, value, key_strindex)| KeyValue {
            key,
            value,
            key_strindex,
        })
}

fn value(finite: bool) -> impl Strategy<Value = AnyValue> {
    use any_value::Value as V;
    leaf(finite).prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..3).prop_map(|values| AnyValue {
                value: Some(V::ArrayValue(ArrayValue { values })),
            }),
            vec(pair(inner), 0..3).prop_map(|values| AnyValue {
                value: Some(V::KvlistValue(KeyValueList { values })),
            }),
        ]
    })
}

fn attributes(finite: bool) -> impl Strategy<Value = Vec<KeyValue>> {
    vec(pair(value(finite)), 0..3)
}

fn event(finite: bool) -> impl Strategy<Value = Event> {
    (any::<u64>(), text(), attributes(finite), any::<u32>()).prop_map(
        |(time_unix_nano, name, attributes, dropped_attributes_count)| Event {
            time_unix_nano,
            name,
            attributes,
            dropped_attributes_count,
        },
    )
}

fn link(finite: bool) -> impl Strategy<Value = Link> {
    (
        id(16),
        id(8),
        text(),
        attributes(finite),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(
            |(trace_id, span_id, trace_state, attributes, dropped_attributes_count, flags)| Link {
                trace_id,
                span_id,
                trace_state,
                attributes,
                dropped_attributes_count,
                flags,
            },
        )
}

fn span(finite: bool) -> impl Strategy<Value = Span> {
    let ids = (id(16), id(8), id(8), text(), any::<u32>());
    let body = (
        text(),
        enum_value(5),
        any::<u64>(),
        any::<u64>(),
        attributes(finite),
    );
    let tail = (
        vec(event(finite), 0..2),
        vec(link(finite), 0..2),
        option::of((text(), enum_value(2)).prop_map(|(message, code)| Status { message, code })),
        (any::<u32>(), any::<u32>(), any::<u32>()),
    );
    (ids, body, tail).prop_map(
        |(
            (trace_id, span_id, parent_span_id, trace_state, flags),
            (name, kind, start_time_unix_nano, end_time_unix_nano, attributes),
            (
                events,
                links,
                status,
                (dropped_attributes_count, dropped_events_count, dropped_links_count),
            ),
        )| Span {
            trace_id,
            span_id,
            trace_state,
            parent_span_id,
            flags,
            name,
            kind,
            start_time_unix_nano,
            end_time_unix_nano,
            attributes,
            dropped_attributes_count,
            events,
            dropped_events_count,
            links,
            dropped_links_count,
            status,
        },
    )
}

fn entity_ref() -> impl Strategy<Value = EntityRef> {
    (text(), text(), vec(text(), 0..2), vec(text(), 0..2)).prop_map(
        |(schema_url, r#type, id_keys, description_keys)| EntityRef {
            schema_url,
            r#type,
            id_keys,
            description_keys,
        },
    )
}

fn scope_spans(finite: bool) -> impl Strategy<Value = ScopeSpans> {
    let scope = (text(), text(), attributes(finite), any::<u32>()).prop_map(
        |(name, version, attributes, dropped_attributes_count)| InstrumentationScope {
            name,
            version,
            attributes,
            dropped_attributes_count,
        },
    );
    (option::of(scope), vec(span(finite), 0..4), text()).prop_map(|(scope, spans, schema_url)| {
        ScopeSpans {
            scope,
            spans,
            schema_url,
        }
    })
}

fn resource_spans(finite: bool) -> impl Strategy<Value = ResourceSpans> {
    let resource = (attributes(finite), any::<u32>(), vec(entity_ref(), 0..2)).prop_map(
        |(attributes, dropped_attributes_count, entity_refs)| Resource {
            attributes,
            dropped_attributes_count,
            entity_refs,
        },
    );
    (option::of(resource), vec(scope_spans(finite), 0..3), text()).prop_map(
        |(resource, scope_spans, schema_url)| ResourceSpans {
            resource,
            scope_spans,
            schema_url,
        },
    )
}

/// A random request. With `finite` set, every double is finite.
pub fn request(finite: bool) -> impl Strategy<Value = ExportTraceServiceRequest> {
    vec(resource_spans(finite), 0..3)
        .prop_map(|resource_spans| ExportTraceServiceRequest { resource_spans })
}
