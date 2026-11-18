//! Proptest strategies for requests that exercise the normalizer.
//!
//! Attribute keys come from the compat tables (iterated, so the generator follows the tables) mixed with random
//! keys, so mapping, consumption and the residuals are all hit. Values have the expected type, another type, or an
//! edge value (empty, negative or extreme integers, NaN). Trace and span ids come from small domains so that
//! traces group and keys repeat, with some empty or all-zero ids for the rejection paths. Times stray outside the
//! valid range and ahead of `received_at`. With `finite` set, doubles are finite: OTLP/JSON has no lossless form for
//! the others. Model keys mostly carry names that bundled price rows match, so usage meets the calculated cost path.

use crucible_compat::{Location, TABLES, Target};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::{Event, Link};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;
use proptest::sample::select;

use super::BASE_NANOS;

/// Every key the tables read at `location`, with the observation-type keys for span attributes.
pub fn table_keys(location: Location) -> Vec<&'static str> {
    let mut keys = Vec::new();
    for table in TABLES {
        for rule in table.fields {
            for source in rule.sources {
                if source.location == location {
                    keys.push(source.key.text());
                }
            }
        }
        if location == Location::Span {
            keys.extend(table.observation_types.iter().map(|rule| rule.key));
        }
    }
    keys
}

/// Model names that bundled price rows match; `gemini-2.5-pro` has a tier chosen by input size.
const PRICED_MODELS: [&str; 6] = [
    "gpt-4o",
    "GPT-4O",
    "claude-3-haiku-20240307",
    "gemini-2.5-pro",
    "text-embedding-3-small",
    "us.anthropic.claude-opus-4-7",
];

/// Every key the tables read for a model name.
fn model_keys() -> Vec<&'static str> {
    TABLES
        .iter()
        .flat_map(|t| t.fields)
        .filter(|rule| matches!(rule.target, Target::ModelRequested | Target::ModelResponse))
        .flat_map(|rule| rule.sources.iter().map(|s| s.key.text()))
        .collect()
}

/// Every value an observation-type rule recognizes.
fn type_values() -> Vec<&'static str> {
    TABLES
        .iter()
        .flat_map(|t| t.observation_types)
        .flat_map(|rule| rule.values.iter().map(|(v, _)| *v))
        .collect()
}

fn text() -> BoxedStrategy<String> {
    prop_oneof![
        vec(any::<char>(), 0..8).prop_map(String::from_iter),
        "[a-z]{0,4}",
    ]
    .boxed()
}

fn key(location: Location) -> BoxedStrategy<String> {
    let keys = table_keys(location);
    if keys.is_empty() {
        return text();
    }
    prop_oneof![3 => select(keys).prop_map(str::to_owned), 1 => text()].boxed()
}

fn double(finite: bool) -> BoxedStrategy<f64> {
    if finite {
        prop_oneof![
            any::<f64>().prop_filter("finite", |d| d.is_finite()),
            Just(-0.0),
            Just(1e300),
        ]
        .boxed()
    } else {
        any::<u64>().prop_map(f64::from_bits).boxed()
    }
}

fn leaf(finite: bool) -> BoxedStrategy<AnyValue> {
    use any_value::Value as V;
    prop_oneof![
        1 => Just(None),
        4 => text().prop_map(|s| Some(V::StringValue(s))),
        2 => select(type_values()).prop_map(|s| Some(V::StringValue(s.to_owned()))),
        1 => any::<bool>().prop_map(|b| Some(V::BoolValue(b))),
        2 => prop_oneof![any::<i64>(), -3i64..3, Just(i64::MAX), Just(i64::MIN)]
            .prop_map(|i| Some(V::IntValue(i))),
        1 => double(finite).prop_map(|d| Some(V::DoubleValue(d))),
        1 => vec(any::<u8>(), 0..6).prop_map(|b| Some(V::BytesValue(b))),
        1 => any::<i32>().prop_map(|i| Some(V::StringValueStrindex(i))),
    ]
    .prop_map(|value| AnyValue { value })
    .boxed()
}

/// A value: mostly a scalar, so that keys meet values of the type their source decodes; else an array or a
/// key-value list, nested up to three levels.
fn value(finite: bool) -> BoxedStrategy<AnyValue> {
    prop_oneof![4 => leaf(finite), 1 => nested(finite)].boxed()
}

fn nested(finite: bool) -> BoxedStrategy<AnyValue> {
    use any_value::Value as V;
    leaf(finite)
        .prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                vec(text(), 0..4).prop_map(|items| AnyValue {
                    value: Some(V::ArrayValue(ArrayValue {
                        values: items
                            .into_iter()
                            .map(|s| AnyValue {
                                value: Some(V::StringValue(s)),
                            })
                            .collect(),
                    })),
                }),
                vec(inner.clone(), 0..3).prop_map(|values| AnyValue {
                    value: Some(V::ArrayValue(ArrayValue { values })),
                }),
                vec((text(), option::of(inner)), 0..3).prop_map(|pairs| AnyValue {
                    value: Some(V::KvlistValue(KeyValueList {
                        values: pairs
                            .into_iter()
                            .map(|(key, value)| KeyValue {
                                key,
                                value,
                                key_strindex: 0,
                            })
                            .collect(),
                    })),
                }),
            ]
        })
        .boxed()
}

/// The values that an observation-type rule recognizes on `key`; empty for any other key.
fn type_values_of(key: &str) -> Vec<&'static str> {
    TABLES
        .iter()
        .flat_map(|t| t.observation_types)
        .filter(|rule| rule.key == key)
        .flat_map(|rule| rule.values.iter().map(|(v, _)| *v))
        .collect()
}

/// One attribute. An observation-type key mostly carries a value its rule recognizes, so every rule yields, and a
/// model key mostly carries a priced model name.
fn pair(location: Location, finite: bool) -> BoxedStrategy<KeyValue> {
    key(location)
        .prop_flat_map(move |key| {
            let mut known = type_values_of(&key);
            if model_keys().contains(&key.as_str()) {
                known.extend(PRICED_MODELS);
            }
            let any = option::weighted(0.9, value(finite));
            let value = if known.is_empty() {
                any.boxed()
            } else {
                prop_oneof![
                    3 => select(known).prop_map(|v| Some(AnyValue {
                        value: Some(any_value::Value::StringValue(v.to_owned())),
                    })),
                    1 => any,
                ]
                .boxed()
            };
            (Just(key), value)
        })
        .prop_map(|(key, value)| KeyValue {
            key,
            value,
            key_strindex: 0,
        })
        .boxed()
}

fn attributes(location: Location, finite: bool) -> BoxedStrategy<Vec<KeyValue>> {
    vec(pair(location, finite), 0..6).boxed()
}

/// An id of `len` bytes: mostly from the small domain `1..=small`, sometimes random, empty or all zero.
fn id(len: usize, small: u8) -> BoxedStrategy<Vec<u8>> {
    prop_oneof![
        1 => Just(Vec::new()),
        1 => Just(vec![0; len]),
        8 => (1..=small).prop_map(move |b| vec![b; len]),
        2 => vec(any::<u8>(), len),
    ]
    .boxed()
}

fn time() -> BoxedStrategy<u64> {
    prop_oneof![
        6 => (0u64..10_000_000_000).prop_map(|d| BASE_NANOS + d),
        1 => (86_401_000_000_000u64..86_500_000_000_000).prop_map(|d| BASE_NANOS + d),
        1 => 4_102_444_800_000_000_000u64..u64::MAX,
        1 => any::<u64>(),
        1 => Just(0),
    ]
    .boxed()
}

fn event(finite: bool) -> BoxedStrategy<Event> {
    (time(), text(), attributes(Location::Span, finite))
        .prop_map(|(time_unix_nano, name, attributes)| Event {
            time_unix_nano,
            name,
            attributes,
            dropped_attributes_count: 0,
        })
        .boxed()
}

fn link(finite: bool) -> BoxedStrategy<Link> {
    (
        id(16, 3),
        id(8, 6),
        text(),
        attributes(Location::Span, finite),
    )
        .prop_map(|(trace_id, span_id, trace_state, attributes)| Link {
            trace_id,
            span_id,
            trace_state,
            attributes,
            dropped_attributes_count: 0,
            flags: 0,
        })
        .boxed()
}

fn span(finite: bool) -> BoxedStrategy<Span> {
    let ids = (
        id(16, 3),
        id(8, 6),
        prop_oneof![2 => Just(Vec::new()), 3 => id(8, 6)],
        text(),
    );
    let body = (
        text(),
        prop_oneof![4 => 0i32..6, 1 => any::<i32>()],
        time(),
        time(),
        attributes(Location::Span, finite),
    );
    let tail = (
        vec(event(finite), 0..3),
        vec(link(finite), 0..2),
        option::of(
            (text(), prop_oneof![4 => 0i32..3, 1 => any::<i32>()])
                .prop_map(|(message, code)| Status { message, code }),
        ),
    );
    (ids, body, tail)
        .prop_map(
            |(
                (trace_id, span_id, parent_span_id, trace_state),
                (name, kind, start_time_unix_nano, end_time_unix_nano, attributes),
                (events, links, status),
            )| Span {
                trace_id,
                span_id,
                trace_state,
                parent_span_id,
                flags: 0,
                name,
                kind,
                start_time_unix_nano,
                end_time_unix_nano,
                attributes,
                dropped_attributes_count: 0,
                events,
                dropped_events_count: 0,
                links,
                dropped_links_count: 0,
                status,
            },
        )
        .boxed()
}

fn scope_spans(finite: bool) -> BoxedStrategy<ScopeSpans> {
    let scope = (text(), text(), attributes(Location::Span, finite)).prop_map(
        |(name, version, attributes)| InstrumentationScope {
            name,
            version,
            attributes,
            dropped_attributes_count: 0,
        },
    );
    (option::of(scope), vec(span(finite), 0..5))
        .prop_map(|(scope, spans)| ScopeSpans {
            scope,
            spans,
            schema_url: String::new(),
        })
        .boxed()
}

fn resource_spans(finite: bool) -> BoxedStrategy<ResourceSpans> {
    let resource = attributes(Location::Resource, finite).prop_map(|attributes| Resource {
        attributes,
        dropped_attributes_count: 0,
        entity_refs: Vec::new(),
    });
    (option::of(resource), vec(scope_spans(finite), 1..3))
        .prop_map(|(resource, scope_spans)| ResourceSpans {
            resource,
            scope_spans,
            schema_url: String::new(),
        })
        .boxed()
}

/// A random request. With `finite` set, every double is finite.
pub fn request(finite: bool) -> BoxedStrategy<ExportTraceServiceRequest> {
    vec(resource_spans(finite), 0..3)
        .prop_map(|resource_spans| ExportTraceServiceRequest { resource_spans })
        .boxed()
}
