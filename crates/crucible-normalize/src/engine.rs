//! The mapping engine (contract 10.2 and 10.3): one pure, table-driven function that maps a span's attributes,
//! resource attributes and events onto a draft.
//!
//! - Each field takes the value of its highest-priority source that decodes to a non-empty value. Among
//!   occurrences of one source (a repeated key, or several matching events) the first in wire order wins.
//! - The observation type is the type of the highest-priority rule that yields one, else `span` (contract 10.4).
//! - Only the attribute occurrence that supplied a value is consumed. Losing candidates, values that do not
//!   decode and keys the table does not know stay in the residual `attributes` or `resource_attributes`, so no
//!   span or resource attribute is dropped. Events are never consumed: `events` keeps all of them, up to its item
//!   cap. Instrumentation-scope attributes have no column in contract 3.1 and are not stored.

use bytes::Bytes;
use crucible_compat::{Decoder, DialectTable, Location, Target};
use crucible_otlp::{AnyValueView, SpanView};
use crucible_records::{IoShape, ObservationType, caps, truncate_str};
use serde::de::IgnoredAny;
use smallvec::SmallVec;

use crate::json;
use crate::mapper::{MapCtx, SpanDraft};

/// A decoded candidate, borrowing the request bytes.
#[derive(Clone, Debug)]
enum Value<'a> {
    /// A non-empty string, for a string field.
    Text(&'a str),
    /// A string, or an array of strings, for a list field.
    List(AnyValueView<'a>),
    /// A count.
    Count(u64),
    /// A body and the shape it has.
    Body(AnyValueView<'a>, IoShape),
}

/// The attribute occurrence that supplied a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Span(usize),
    Resource(usize),
    Event,
}

#[derive(Debug)]
struct Candidate<'a> {
    /// Position of the source in its field's source list: lower wins.
    rank: usize,
    origin: Origin,
    value: Value<'a>,
}

/// The best candidate of each field, indexed like the table's field rules.
type Slots<'a> = SmallVec<[Option<Candidate<'a>>; 24]>;

/// Where an attribute was read.
#[derive(Clone, Copy, Debug)]
enum At<'e> {
    Span,
    Resource,
    Event(&'e str),
}

fn located(location: Location, at: At<'_>) -> bool {
    match (location, at) {
        (Location::Span, At::Span) | (Location::Resource, At::Resource) => true,
        (Location::Event(name), At::Event(event)) => name == event,
        _ => false,
    }
}

const fn is_list(target: Target) -> bool {
    matches!(target, Target::FinishReasons)
}

/// Decodes `value` for `target`, or `None` when it does not decode or decodes to an empty value.
fn decode<'a>(target: Target, decoder: Decoder, value: &AnyValueView<'a>) -> Option<Value<'a>> {
    match decoder {
        Decoder::String => match *value {
            AnyValueView::String("") => None,
            AnyValueView::String(s) if !is_list(target) => Some(Value::Text(s)),
            AnyValueView::String(_) => Some(Value::List(value.clone())),
            AnyValueView::Array(ref items) if is_list(target) => {
                let mut any = false;
                for item in items.clone() {
                    if !matches!(item, AnyValueView::String(_)) {
                        return None;
                    }
                    any = true;
                }
                any.then(|| Value::List(value.clone()))
            }
            _ => None,
        },
        Decoder::Integer => match *value {
            AnyValueView::Int(i) => u64::try_from(i).ok().map(Value::Count),
            _ => None,
        },
        Decoder::Json => match *value {
            AnyValueView::Empty | AnyValueView::String("") => None,
            AnyValueView::String(s) if !is_json(s) => None,
            _ => Some(Value::Body(value.clone(), IoShape::Raw)),
        },
        Decoder::JsonOrString(shape) => match *value {
            AnyValueView::Empty | AnyValueView::String("") => None,
            _ => Some(Value::Body(value.clone(), shape)),
        },
    }
}

/// Whether `s` is JSON text, checked without building a value. Text nested beyond the parser's recursion limit
/// (128 levels) counts as not JSON.
fn is_json(s: &str) -> bool {
    serde_json::from_str::<IgnoredAny>(s).is_ok()
}

/// Offers one attribute to every source that reads it.
fn offer<'a>(
    table: &DialectTable,
    slots: &mut Slots<'a>,
    at: At<'_>,
    origin: Origin,
    key: &str,
    value: &AnyValueView<'a>,
) {
    for (slot, rule) in table.fields.iter().enumerate() {
        for (rank, source) in rule.sources.iter().enumerate() {
            if !located(source.location, at) || !source.key.matches(key) {
                continue;
            }
            if slots[slot].as_ref().is_some_and(|c| c.rank <= rank) {
                continue;
            }
            if let Some(value) = decode(rule.target, source.decoder, value) {
                slots[slot] = Some(Candidate {
                    rank,
                    origin,
                    value,
                });
            }
        }
    }
}

fn text(s: &str, cap: usize) -> Box<str> {
    truncate_str(s, cap).into()
}

fn list(value: AnyValueView<'_>, max_items: usize, item_bytes: usize) -> Vec<Box<str>> {
    match value {
        AnyValueView::String(s) => vec![text(s, item_bytes)],
        AnyValueView::Array(items) => items
            .filter_map(|item| match item {
                AnyValueView::String(s) => Some(text(s, item_bytes)),
                _ => None,
            })
            .take(max_items)
            .collect(),
        _ => Vec::new(),
    }
}

/// A body: a string verbatim, any other value as JSON.
fn body(value: AnyValueView<'_>) -> Bytes {
    match value {
        AnyValueView::String(s) => Bytes::copy_from_slice(s.as_bytes()),
        other => json::value(other),
    }
}

/// Maps one span with `table` (the engine behind [`crate::TableMapper`]).
pub(crate) fn map_span(
    table: &DialectTable,
    _ctx: &MapCtx<'_>,
    span: &SpanView<'_>,
    out: &mut SpanDraft,
) {
    let mut slots: Slots<'_> = table.fields.iter().map(|_| None).collect();
    for (index, (key, value)) in span.resource().attributes().enumerate() {
        offer(
            table,
            &mut slots,
            At::Resource,
            Origin::Resource(index),
            key,
            &value,
        );
    }
    // (rule priority, attribute index, type, consume)
    let mut kind: Option<(usize, usize, ObservationType, bool)> = None;
    for (index, (key, value)) in span.attributes().enumerate() {
        offer(
            table,
            &mut slots,
            At::Span,
            Origin::Span(index),
            key,
            &value,
        );
        let AnyValueView::String(text) = value else {
            continue;
        };
        for (priority, rule) in table.observation_types.iter().enumerate() {
            if rule.key != key || kind.is_some_and(|(p, ..)| p <= priority) {
                continue;
            }
            if let Some(&(_, t)) = rule.values.iter().find(|(v, _)| *v == text) {
                kind = Some((priority, index, t, rule.consume));
            }
        }
    }
    let event_sources = table.fields.iter().any(|rule| {
        rule.sources
            .iter()
            .any(|s| matches!(s.location, Location::Event(_)))
    });
    if event_sources {
        for event in span.events() {
            for (key, value) in event.attributes() {
                offer(
                    table,
                    &mut slots,
                    At::Event(event.name()),
                    Origin::Event,
                    key,
                    &value,
                );
            }
        }
    }

    let mut consumed: SmallVec<[Origin; 24]> = slots.iter().flatten().map(|c| c.origin).collect();
    if let Some((_, index, _, true)) = kind {
        consumed.push(Origin::Span(index));
    }
    let record = &mut out.record;
    let (mut input_shape, mut output_shape) = (None, None);
    for (rule, candidate) in table.fields.iter().zip(slots) {
        let Some(candidate) = candidate else {
            continue;
        };
        let (core, payload) = (&mut record.core, &mut record.payload);
        match (rule.target, candidate.value) {
            (Target::ServiceName, Value::Text(s)) => {
                core.service_name = text(s, caps::SERVICE_BYTES)
            }
            (Target::ServiceVersion, Value::Text(s)) => {
                core.service_version = text(s, caps::SERVICE_BYTES);
            }
            (Target::TelemetrySdkName, Value::Text(s)) => {
                core.telemetry_sdk_name = text(s, caps::SDK_BYTES);
            }
            (Target::TelemetrySdkLanguage, Value::Text(s)) => {
                core.telemetry_sdk_language = text(s, caps::SDK_BYTES);
            }
            (Target::TelemetrySdkVersion, Value::Text(s)) => {
                core.telemetry_sdk_version = text(s, caps::SDK_BYTES);
            }
            (Target::UserId, Value::Text(s)) => core.user_id = text(s, caps::TRACE_ATTRIBUTE_BYTES),
            (Target::ModelRequested, Value::Text(s)) => {
                core.model_requested = text(s, caps::MODEL_BYTES);
            }
            (Target::ModelResponse, Value::Text(s)) => {
                core.model_response = text(s, caps::MODEL_BYTES);
            }
            (Target::Provider, Value::Text(s)) => core.provider = text(s, caps::MODEL_BYTES),
            (Target::PromptName, Value::Text(s)) => {
                core.prompt_name = text(s, caps::PROMPT_NAME_BYTES)
            }
            (Target::FinishReasons, Value::List(v)) => {
                core.finish_reasons =
                    list(v, caps::FINISH_REASONS_MAX_ITEMS, caps::FINISH_REASON_BYTES);
            }
            (Target::Usage(key), Value::Count(units)) => {
                payload.usage_provided.push((key.into(), units))
            }
            (Target::Input, Value::Body(v, shape)) => {
                payload.input = body(v);
                input_shape = Some(shape);
            }
            (Target::Output, Value::Body(v, shape)) => {
                payload.output = body(v);
                output_shape = Some(shape);
            }
            (Target::SystemInstructions, Value::Body(v, _)) => {
                payload.system_instructions = body(v)
            }
            (Target::ToolDefinitions, Value::Body(v, _)) => payload.tool_definitions = body(v),
            // A validated table never pairs a target with another kind of value.
            _ => {}
        }
    }
    let (core, payload) = (&mut record.core, &mut record.payload);
    core.io_shape = input_shape.or(output_shape).unwrap_or(IoShape::Raw);
    core.observation_type = kind.map_or(ObservationType::Span, |(_, _, t, _)| t);
    payload.usage_provided.sort_by(|a, b| a.0.cmp(&b.0));
    payload.attributes = json::object_or_empty(
        span.attributes()
            .enumerate()
            .filter(|(i, _)| !consumed.contains(&Origin::Span(*i)))
            .map(|(_, pair)| pair),
    );
    payload.resource_attributes = json::object_or_empty(
        span.resource()
            .attributes()
            .enumerate()
            .filter(|(i, _)| !consumed.contains(&Origin::Resource(*i)))
            .map(|(_, pair)| pair),
    );
    payload.events = json::events(span.events());
}

#[cfg(test)]
mod tests {
    use crucible_compat::{Confidence, DetectOn, DetectRule, FieldRule, KeyMatch, Source};
    use crucible_otlp::TraceRequest;
    use crucible_records::{ApiKeyId, Lsn, ProjectId, Timestamp, WalEpoch};
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::{
        AnyValue, InstrumentationScope, KeyValue, any_value,
    };
    use opentelemetry_proto::tonic::trace::v1::span::Event;
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use prost::Message;

    use super::*;
    use crate::mapper::{Hints, SpanMapper, TableMapper};
    use crate::test_support::record;
    use crate::{IngestFrame, McpProjector};

    fn kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value.to_owned())),
            }),
            key_strindex: 0,
        }
    }

    fn event(name: &str, attributes: Vec<KeyValue>) -> Event {
        Event {
            time_unix_nano: 0,
            name: name.to_owned(),
            attributes,
            dropped_attributes_count: 0,
        }
    }

    /// A one-span request body.
    fn body(scope: &str, attributes: Vec<KeyValue>, events: Vec<Event>) -> Vec<u8> {
        let span = Span {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            attributes,
            events,
            ..Span::default()
        };
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: None,
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: scope.to_owned(),
                        ..InstrumentationScope::default()
                    }),
                    spans: vec![span],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
        .encode_to_vec()
    }

    fn frame(otlp: &[u8]) -> IngestFrame<'_> {
        IngestFrame {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            lsn: Lsn::new(1),
            received_at: Timestamp::from_micros(0),
            project_id: ProjectId::from_bytes([2; 16]),
            api_key_id: ApiKeyId::from_bytes([3; 16]),
            compat_dialect: 0,
            sdk_name: "",
            sdk_version: "",
            rejected: &[],
            otlp,
        }
    }

    /// A table with what dialect 0's has none of: prefix keys, event sources and detection rules.
    static TEST_TABLE: DialectTable = DialectTable {
        dialect: 5,
        detect: &[
            DetectRule {
                on: DetectOn::ScopeName(KeyMatch::Exact("lib.x")),
                confidence: Confidence(5),
            },
            DetectRule {
                on: DetectOn::SpanKey(KeyMatch::Prefix("x.")),
                confidence: Confidence(7),
            },
        ],
        fields: &[
            FieldRule {
                target: Target::ModelRequested,
                sources: &[
                    Source {
                        location: Location::Span,
                        key: KeyMatch::Prefix("x.model."),
                        decoder: Decoder::String,
                    },
                    Source {
                        location: Location::Event("x.choice"),
                        key: KeyMatch::Exact("model"),
                        decoder: Decoder::String,
                    },
                ],
            },
            FieldRule {
                target: Target::Provider,
                sources: &[Source {
                    location: Location::Event("x.choice"),
                    key: KeyMatch::Exact("provider"),
                    decoder: Decoder::String,
                }],
            },
        ],
        observation_types: &[],
    };

    fn mapped(otlp: &[u8]) -> SpanDraft {
        let request = TraceRequest::scan(otlp, None).unwrap();
        let span = request.spans().next().unwrap();
        let f = frame(otlp);
        let ctx = MapCtx {
            frame: &f,
            normalizer_version: 1,
        };
        let mut draft = SpanDraft {
            record: record(),
            level: None,
        };
        TableMapper::new(&TEST_TABLE).map(&ctx, &span, &mut draft);
        draft
    }

    #[test]
    fn prefix_and_event_sources_follow_their_priority() {
        let choice = || {
            event(
                "x.choice",
                vec![kv("model", "from event"), kv("provider", "p")],
            )
        };
        // The span source outranks the event source; the first matching key in wire order wins.
        let both = mapped(&body(
            "",
            vec![kv("x.model.a", "first"), kv("x.model.b", "second")],
            vec![event("other", vec![kv("model", "ignored")]), choice()],
        ));
        assert_eq!(&*both.record.core.model_requested, "first");
        assert_eq!(&*both.record.core.provider, "p");
        assert_eq!(
            &both.record.payload.attributes[..],
            br#"{"x.model.b":"second"}"#
        );
        // Without the span key, the event supplies the value; events are never consumed.
        let event_only = mapped(&body("", Vec::new(), vec![choice()]));
        assert_eq!(&*event_only.record.core.model_requested, "from event");
        let events: serde_json::Value =
            serde_json::from_slice(&event_only.record.payload.events).unwrap();
        assert_eq!(events[0]["attributes"]["model"], "from event");
    }

    #[test]
    fn detection_takes_the_most_confident_matching_rule() {
        let detect = |otlp: &[u8]| {
            let request = TraceRequest::scan(otlp, None).unwrap();
            let span = request.spans().next().unwrap();
            TableMapper::new(&TEST_TABLE).detect(
                span.resource(),
                span.scope(),
                &span,
                &Hints { dialect: 0 },
            )
        };
        assert_eq!(
            detect(&body("lib.x", vec![kv("x.k", "v")], Vec::new())),
            Some(Confidence(7))
        );
        assert_eq!(
            detect(&body("lib.x", Vec::new(), Vec::new())),
            Some(Confidence(5))
        );
        assert_eq!(
            detect(&body("lib.y", vec![kv("y.k", "v")], Vec::new())),
            None
        );
        assert_eq!(TableMapper::new(&TEST_TABLE).dialect(), 5);
    }

    #[test]
    fn the_projector_is_a_stub() {
        let otlp = body("", vec![kv("mcp.method.name", "tools/call")], Vec::new());
        let request = TraceRequest::scan(&otlp, None).unwrap();
        let span = request.spans().next().unwrap();
        let draft = SpanDraft {
            record: record(),
            level: None,
        };
        let native = TableMapper::new(&crucible_compat::DIALECT_0);
        assert!(native.project(&draft, &span).is_none());
        assert!(native.inventory(&draft, &span).is_empty());
    }
}
