//! Proptest strategies for requests that exercise the normalizer.
//!
//! Attribute keys, scope names and event names come from the compat layers (iterated, so the generator follows the
//! tables of every dialect and names none of their keys itself) mixed with random ones, so detection, mapping,
//! consumption and the residuals are all hit. Values have the expected type, another type, or an
//! edge value (empty, negative or extreme integers, NaN). Trace and span ids come from small domains so that
//! traces group and keys repeat, with some empty or all-zero ids for the rejection paths. Times stray outside the
//! valid range and ahead of `received_at`. With `finite` set, doubles are finite: OTLP/JSON has no lossless form for
//! the others. Model keys mostly carry names that bundled price rows match, so usage meets the calculated cost path.

use crucible_compat::{
    Decoder, DetectOn, KeyMatch, LAYERS, Location, Target, ToolCallShape, UsageMap, ValueEncoding,
};
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

/// Key suffixes that make a prefix source's key: a member name, a dotted member name, and the keys of an indexed
/// family with a wrapper segment and a nested list.
const SUFFIXES: [&str; 8] = [
    "total",
    "a.b",
    "0.role",
    "0.content",
    "1.message.role",
    "0.message.tool_calls.0.tool_call.function.name",
    "0.tool_calls.0.name",
    "0.finish_reason",
];

/// Every key the layers read at `location`, with the observation-type and detection keys for span attributes. A
/// prefix gives the bare prefix (which names no member), the prefix with each of [`SUFFIXES`], and for a usage
/// prefix the member names its usage map knows.
pub fn table_keys(location: Location) -> Vec<String> {
    let mut keys = Vec::new();
    for table in LAYERS {
        for rule in table.fields {
            for source in rule.sources {
                if source.location != location {
                    continue;
                }
                let text = source.key.text();
                keys.push(text.to_owned());
                if matches!(source.key, KeyMatch::Prefix(_)) {
                    keys.extend(SUFFIXES.iter().map(|suffix| format!("{text}{suffix}")));
                    if let Decoder::PrefixUsage(map) = source.decoder {
                        keys.extend(map.aliases.iter().map(|(m, _)| format!("{text}{m}")));
                        keys.extend(
                            map.sums
                                .iter()
                                .flat_map(|(_, addends)| *addends)
                                .map(|m| format!("{text}{m}")),
                        );
                    }
                }
            }
        }
        if location == Location::Span {
            keys.extend(
                table
                    .observation_types
                    .iter()
                    .map(|rule| rule.key.to_owned()),
            );
            for rule in table.detect {
                match rule.on {
                    DetectOn::SpanKey(KeyMatch::Exact(key)) => keys.push(key.to_owned()),
                    DetectOn::SpanKey(KeyMatch::Prefix(prefix)) => keys.push(format!("{prefix}x")),
                    DetectOn::ScopeName(_) => {}
                }
            }
        }
    }
    keys
}

/// Scope names that detection rules recognize.
fn scope_names() -> Vec<String> {
    LAYERS
        .iter()
        .flat_map(|t| t.detect)
        .filter_map(|rule| match rule.on {
            DetectOn::ScopeName(KeyMatch::Exact(name)) => Some(name.to_owned()),
            DetectOn::ScopeName(KeyMatch::Prefix(prefix)) => Some(format!("{prefix}x")),
            DetectOn::SpanKey(_) => None,
        })
        .collect()
}

/// Event names that sources read.
fn event_names() -> Vec<String> {
    LAYERS
        .iter()
        .flat_map(|t| t.fields)
        .flat_map(|rule| rule.sources)
        .filter_map(|source| match source.location {
            Location::Event(name) | Location::EventTime(name) => Some(name.to_owned()),
            _ => None,
        })
        .collect()
}

fn string(text: impl Into<String>) -> any_value::Value {
    any_value::Value::StringValue(text.into())
}

fn string_array(items: &[&str]) -> any_value::Value {
    any_value::Value::ArrayValue(ArrayValue {
        values: items
            .iter()
            .map(|s| AnyValue {
                value: Some(string(*s)),
            })
            .collect(),
    })
}

/// A JSON object nested along `path` around `leaf`.
fn nest(path: &[&str], leaf: serde_json::Value) -> serde_json::Value {
    path.iter()
        .rev()
        .fold(leaf, |inner, step| serde_json::json!({ *step: inner }))
}

/// JSON text of a usage or cost object that exercises a usage map: its aliases, the addends of its sums, an
/// ignored member, a member it does not know and a details object.
fn usage_object(map: &UsageMap, cost: bool) -> String {
    let mut object = serde_json::Map::new();
    let amount = |n: usize| {
        if cost {
            serde_json::json!(n as f64 / 1000.0)
        } else {
            serde_json::json!(n)
        }
    };
    for (i, (member, _)) in map.aliases.iter().enumerate().filter(|(i, _)| i % 2 == 0) {
        object.insert((*member).to_owned(), amount(i + 1));
    }
    for (_, addends) in map.sums {
        for (i, addend) in addends.iter().enumerate() {
            object.insert((*addend).to_owned(), amount(10 * (i + 1)));
        }
    }
    for member in map.ignore.iter().take(1) {
        object.insert((*member).to_owned(), amount(99));
    }
    object.insert("some_other_count".to_owned(), amount(7));
    object.insert(
        "prompt_tokens_details".to_owned(),
        serde_json::json!({ "cached_tokens": 2 }),
    );
    serde_json::Value::Object(object).to_string()
}

/// JSON text of a tool-call list in a source's shape.
fn tool_call_list(shape: &ToolCallShape) -> String {
    let mut call = serde_json::Map::new();
    for (path, leaf) in [
        (shape.id, serde_json::json!("call-1")),
        (shape.name, serde_json::json!("lookup")),
        (shape.arguments, serde_json::json!({ "q": "x" })),
    ] {
        let Some((first, rest)) = path.split_first() else {
            continue;
        };
        let nested = nest(rest, leaf);
        match (call.get_mut(*first), nested) {
            (Some(serde_json::Value::Object(existing)), serde_json::Value::Object(more)) => {
                existing.extend(more);
            }
            (_, nested) => {
                call.insert((*first).to_owned(), nested);
            }
        }
    }
    serde_json::json!([call, { "unrelated": true }]).to_string()
}

/// Values that the decoder of a source reading `key` accepts, for the decoders random values rarely satisfy. A
/// source with a member path gets an object that holds such a value at its path; a table that encodes values as
/// JSON text also gets each string value as a JSON string literal.
fn decoder_values_of(key: &str) -> Vec<any_value::Value> {
    use any_value::Value as V;
    let mut values = Vec::new();
    for table in LAYERS {
        let sources = table
            .fields
            .iter()
            .flat_map(|rule| rule.sources)
            .filter(|source| source.key.matches(key));
        for source in sources {
            let mut own: Vec<V> = match source.decoder {
                Decoder::Level(map) => map
                    .names
                    .iter()
                    .map(|(name, _)| string(name.to_ascii_uppercase()))
                    .chain(map.numbers.iter().map(|(n, _)| V::IntValue(*n)))
                    .chain(map.numbers.iter().map(|(n, _)| string(n.to_string())))
                    .collect(),
                Decoder::Timestamp => vec![
                    string("2026-11-17T09:30:00.125Z"),
                    string("2026-11-17t09:30:00+02:00"),
                    string("\"2026-11-17T09:30:00.125Z\""),
                    V::IntValue(BASE_NANOS as i64 + 5),
                ],
                Decoder::Usd => vec![
                    string("0.0125"),
                    string("3"),
                    string("-1"),
                    string("1e-3"),
                    V::DoubleValue(0.25),
                ],
                Decoder::StringOrInt => vec![V::IntValue(42), string("v7"), string("\"quoted\"")],
                Decoder::Bool => vec![
                    V::BoolValue(true),
                    V::BoolValue(false),
                    string("true"),
                    string("false"),
                ],
                Decoder::Tags { .. } => vec![
                    string_array(&["a", " b ", "a"]),
                    string("x, y,,x"),
                    string(r#"["p","q"]"#),
                ],
                Decoder::OffsetFromStart(_) => {
                    vec![V::IntValue(100), V::DoubleValue(0.0625), V::IntValue(-1)]
                }
                Decoder::JsonObject => vec![
                    string(r#"{"k":"v","n":1,"nested":{"deep":[1,2]}}"#),
                    string(r#"{"k":"other","model":"m","provider":"p","tools":[{"name":"t"}]}"#),
                    string("[1]"),
                ],
                Decoder::UsageObject(map) => vec![string(usage_object(map, false))],
                Decoder::CostObject(map) => vec![string(usage_object(map, true))],
                Decoder::PrefixUsage(_) | Decoder::Integer => {
                    vec![V::IntValue(7), V::IntValue(0), string("12")]
                }
                Decoder::MapMember(_) => vec![
                    V::DoubleValue(0.5),
                    V::IntValue(3),
                    V::BoolValue(true),
                    string_array(&["stop"]),
                    string(r#"{"type":"auto"}"#),
                ],
                Decoder::JsonStrings => vec![string_array(&[r#"{"a":1}"#, "plain"])],
                Decoder::ToolCalls(shape) => vec![string(tool_call_list(shape))],
                Decoder::Json | Decoder::JsonOrString(_) => vec![
                    string(
                        r#"[{"role":"assistant","parts":[{"type":"tool_call","id":"c","name":"f","arguments":{}}]}]"#,
                    ),
                    string("plain body"),
                ],
                Decoder::Indexed(_) | Decoder::IndexedList(_) => {
                    vec![string("user"), string(r#"{"type":"object"}"#)]
                }
                Decoder::String | Decoder::PrefixMap(_) => vec![string("\"quoted\"")],
            };
            if !source.path.is_empty() {
                own = own
                    .into_iter()
                    .filter_map(|value| {
                        let leaf = match value {
                            V::StringValue(s) => serde_json::json!(s),
                            V::IntValue(i) => serde_json::json!(i),
                            V::DoubleValue(d) => serde_json::json!(d),
                            V::BoolValue(b) => serde_json::json!(b),
                            _ => return None,
                        };
                        // A numeric step indexes an array: the generated path holds its element at index 0.
                        let object = source.path.iter().rev().fold(leaf, |inner, step| {
                            if step.bytes().all(|b| b.is_ascii_digit()) {
                                serde_json::json!([inner])
                            } else {
                                serde_json::json!({ *step: inner })
                            }
                        });
                        Some(string(object.to_string()))
                    })
                    .collect();
            }
            if table.encoding == ValueEncoding::JsonText {
                let quoted: Vec<V> = own
                    .iter()
                    .filter_map(|value| match value {
                        V::StringValue(s) => Some(string(serde_json::json!(s).to_string())),
                        _ => None,
                    })
                    .collect();
                own.extend(quoted);
            }
            values.extend(own);
        }
        for rule in table.observation_types.iter().filter(|r| r.key == key) {
            for (value, _) in rule.values {
                values.push(if rule.path.is_empty() {
                    string(*value)
                } else {
                    string(nest(rule.path, serde_json::json!(value)).to_string())
                });
                if table.encoding == ValueEncoding::JsonText {
                    values.push(string(serde_json::json!(value).to_string()));
                }
            }
        }
    }
    values
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

/// Every key the layers read whole for a model name.
fn model_keys() -> Vec<&'static str> {
    LAYERS
        .iter()
        .flat_map(|t| t.fields)
        .filter(|rule| matches!(rule.target, Target::ModelRequested | Target::ModelResponse))
        .flat_map(|rule| rule.sources)
        .filter(|s| s.path.is_empty())
        .map(|s| s.key.text())
        .collect()
}

/// Every value an observation-type rule recognizes.
fn type_values() -> Vec<&'static str> {
    LAYERS
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
    prop_oneof![3 => select(keys), 1 => text()].boxed()
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

/// One attribute. A key that a source or a type rule reads mostly carries a value its decoder accepts, so every
/// source and rule yields, and a model key mostly carries a priced model name.
fn pair(location: Location, finite: bool) -> BoxedStrategy<KeyValue> {
    key(location)
        .prop_flat_map(move |key| {
            let mut known = decoder_values_of(&key);
            if model_keys().contains(&key.as_str()) {
                known.extend(PRICED_MODELS.map(string));
            }
            let any = option::weighted(0.9, value(finite));
            let value = if known.is_empty() {
                any.boxed()
            } else {
                prop_oneof![
                    3 => select(known).prop_map(|v| Some(AnyValue { value: Some(v) })),
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
    let name = prop_oneof![1 => select(event_names()), 2 => text()];
    (time(), name, attributes(Location::Span, finite))
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
    let name = prop_oneof![1 => select(scope_names()), 1 => text()];
    let version = prop_oneof![1 => text(), 1 => "[1-9]\\.[0-9]\\.[0-9]"];
    let scope = (name, version, attributes(Location::Span, finite)).prop_map(
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
