use crucible_compat::{
    Confidence, DetectOn, DetectRule, FieldRule, KeyMatch, Location, Scope, Source,
};
use crucible_otlp::TraceRequest;
use crucible_records::{ApiKeyId, Lsn, ProjectId, Timestamp, WalEpoch};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
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

fn event(name: &str, time_unix_nano: u64, attributes: Vec<KeyValue>) -> Event {
    Event {
        time_unix_nano,
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

const fn source(location: Location, key: KeyMatch, decoder: Decoder) -> Source {
    Source {
        location,
        key,
        path: &[],
        decoder,
        scope: Scope::Any,
    }
}

/// A table with what the production tables have none of: a prefix key that feeds a field taking one value, and
/// sources that read event attributes.
static TEST_TABLE: DialectTable = DialectTable {
    dialect: 5,
    encoding: ValueEncoding::Native,
    detect: &[
        DetectRule {
            on: DetectOn::ScopeName(KeyMatch::Exact("lib.x")),
            confidence: Confidence(40),
        },
        DetectRule {
            on: DetectOn::SpanKey(KeyMatch::Prefix("x.")),
            confidence: Confidence(70),
        },
    ],
    fields: &[
        FieldRule {
            target: Target::ModelRequested,
            sources: &[
                source(
                    Location::Span,
                    KeyMatch::Prefix("x.model."),
                    Decoder::String,
                ),
                source(
                    Location::Event("x.choice"),
                    KeyMatch::Exact("model"),
                    Decoder::String,
                ),
            ],
        },
        FieldRule {
            target: Target::Provider,
            sources: &[source(
                Location::Event("x.choice"),
                KeyMatch::Exact("provider"),
                Decoder::String,
            )],
        },
        FieldRule {
            target: Target::CompletionStartTime,
            sources: &[source(
                Location::EventTime("x.first"),
                KeyMatch::Exact(crucible_compat::EVENT_TIME_KEY),
                Decoder::Timestamp,
            )],
        },
    ],
    observation_types: &[],
    media: &[],
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
            0,
            vec![kv("model", "from event"), kv("provider", "p")],
        )
    };
    // The span source outranks the event source; the first matching key in wire order wins.
    let both = mapped(&body(
        "",
        vec![kv("x.model.a", "first"), kv("x.model.b", "second")],
        vec![event("other", 0, vec![kv("model", "ignored")]), choice()],
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
fn an_event_time_source_takes_the_first_event_of_its_name() {
    let draft = mapped(&body(
        "",
        Vec::new(),
        vec![
            event("other", 1_000, Vec::new()),
            event("x.first", 5_000, Vec::new()),
            event("x.first", 9_000, Vec::new()),
        ],
    ));
    assert_eq!(
        draft.record.core.completion_start_time,
        Some(Timestamp::from_micros(5))
    );
}

#[test]
fn detection_takes_the_most_confident_matching_rule_and_the_hint_counts_as_60() {
    let detect = |otlp: &[u8], hint: u8| {
        let request = TraceRequest::scan(otlp, None).unwrap();
        let span = request.spans().next().unwrap();
        TableMapper::new(&TEST_TABLE).detect(
            span.resource(),
            span.scope(),
            &span,
            &Hints { dialect: hint },
        )
    };
    let keyed = body("lib.x", vec![kv("x.k", "v")], Vec::new());
    let scoped = body("lib.x", Vec::new(), Vec::new());
    let other = body("lib.y", vec![kv("y.k", "v")], Vec::new());
    assert_eq!(detect(&keyed, 0), Some(Confidence(70)));
    assert_eq!(detect(&keyed, 5), Some(Confidence(70)));
    assert_eq!(detect(&scoped, 0), Some(Confidence(40)));
    assert_eq!(detect(&scoped, 5), Some(Confidence::HINT));
    assert_eq!(detect(&other, 0), None);
    assert_eq!(detect(&other, 5), Some(Confidence::HINT));
    assert_eq!(detect(&other, 4), None);
    assert_eq!(TableMapper::new(&TEST_TABLE).dialect(), 5);
}

#[test]
fn the_projector_yields_an_operation_only_for_the_method_key() {
    let draft = SpanDraft {
        record: record(),
        level: None,
    };
    let native = TableMapper::new(&crucible_compat::DIALECT_0);
    let otlp = body("", vec![kv("mcp.method.name", "tools/call")], Vec::new());
    let request = TraceRequest::scan(&otlp, None).unwrap();
    let span = request.spans().next().unwrap();
    let op = native.project(&draft, &span).unwrap();
    assert_eq!(&*op.record.method, "tools/call");
    assert_eq!(op.record.span_id, draft.record.core.span_id);
    assert!(native.inventory(&draft, &span).is_empty());

    let otlp = body("", vec![kv("gen_ai.tool.name", "echo")], Vec::new());
    let request = TraceRequest::scan(&otlp, None).unwrap();
    let span = request.spans().next().unwrap();
    assert!(native.project(&draft, &span).is_none());
}

fn str_scalar(s: &str) -> Scalar<'_> {
    Scalar::Str(Cow::Borrowed(s))
}

#[test]
fn offsets_round_half_to_even_and_clamp_to_the_end() {
    let (start, end) = (
        Timestamp::from_micros(1_000_000),
        Timestamp::from_micros(2_000_000),
    );
    let at = |unit, scalar| offset(start, end, unit, scalar).map(Timestamp::micros);
    assert_eq!(at(TimeUnit::Millis, Scalar::Int(100)), Some(1_100_000));
    assert_eq!(at(TimeUnit::Seconds, Scalar::Double(0.25)), Some(1_250_000));
    // 0.5 and 1.5 microseconds round to the even neighbour.
    assert_eq!(
        at(TimeUnit::Millis, Scalar::Double(0.0005)),
        Some(1_000_000)
    );
    assert_eq!(
        at(TimeUnit::Millis, Scalar::Double(0.0015)),
        Some(1_000_002)
    );
    assert_eq!(at(TimeUnit::Seconds, Scalar::Int(5)), Some(2_000_000));
    assert_eq!(
        at(TimeUnit::Seconds, Scalar::Double(f64::MAX)),
        Some(2_000_000)
    );
    assert_eq!(at(TimeUnit::Seconds, Scalar::Int(-1)), None);
    assert_eq!(at(TimeUnit::Seconds, Scalar::Double(-0.1)), None);
    assert_eq!(at(TimeUnit::Seconds, Scalar::Double(f64::NAN)), None);
    assert_eq!(at(TimeUnit::Seconds, Scalar::Double(f64::INFINITY)), None);
    assert_eq!(at(TimeUnit::Seconds, str_scalar("1")), None);
    // A span without a duration is not clamped.
    assert_eq!(
        offset(start, start, TimeUnit::Seconds, Scalar::Int(1)).map(Timestamp::micros),
        Some(2_000_000)
    );
}

#[test]
fn tags_are_trimmed_split_and_deduplicated() {
    let of = |text: &'static str, split: bool| {
        let value = AnyValueView::String(text);
        tags(&Val::View(&value), split, false)
            .map(|tags| tags.into_iter().map(Cow::into_owned).collect::<Vec<_>>())
    };
    assert_eq!(
        of("a, b,,a ", true),
        Some(vec!["a".to_owned(), "b".to_owned()])
    );
    assert_eq!(of("a, b", false), Some(vec!["a, b".to_owned()]));
    assert_eq!(
        of(r#"["x"," y ","x"]"#, false),
        Some(vec!["x".to_owned(), "y".to_owned()])
    );
    // An array with an element that is not a string is one plain tag, not a list.
    assert_eq!(of("[1]", false), Some(vec!["[1]".to_owned()]));
    assert_eq!(of(" , ", true), None);
    assert_eq!(of("", false), None);
    let quoted = AnyValueView::String(r#""a,b""#);
    assert_eq!(
        tags(&Val::View(&quoted), true, true).map(|t| t.len()),
        Some(2)
    );
}

#[test]
fn levels_match_names_without_case_and_numbers_exactly() {
    static MAP: crucible_compat::LevelMap = crucible_compat::LevelMap {
        names: &[("warn", Level::Warning)],
        numbers: &[(30, Level::Warning)],
    };
    assert_eq!(level(&MAP, str_scalar(" WARN\t")), Some(Level::Warning));
    assert_eq!(level(&MAP, str_scalar("warning")), None);
    assert_eq!(level(&MAP, Scalar::Int(30)), Some(Level::Warning));
    assert_eq!(level(&MAP, Scalar::Int(31)), None);
    assert_eq!(level(&MAP, Scalar::Double(30.0)), None);
}

#[test]
fn instants_read_rfc_3339_a_quoted_instant_or_unix_nanoseconds() {
    let expected = Some(Timestamp::from_micros(1_794_907_800_000_000));
    assert_eq!(instant(str_scalar("2026-11-17T09:30:00Z")), expected);
    assert_eq!(instant(str_scalar("\"2026-11-17T09:30:00Z\"")), expected);
    assert_eq!(instant(Scalar::Int(1_794_907_800_000_000_000)), expected);
    assert_eq!(instant(Scalar::Int(-1)), None);
    assert_eq!(instant(str_scalar("yesterday")), None);
    assert_eq!(instant(Scalar::Bool(true)), None);
}

#[test]
fn a_preview_replaces_media_tokens_and_keeps_other_text() {
    let closed = MediaToken {
        prefix: "@@@m:",
        suffix: "@@@",
    };
    let open = MediaToken {
        prefix: "att://",
        suffix: "",
    };
    let preview = |body: &str| media_preview(body.as_bytes(), &[closed, open]);
    assert_eq!(
        preview("see @@@m:type=image/png|id=abc@@@ and att://x/y\" end").as_deref(),
        Some("see [media] and [media]\" end")
    );
    assert_eq!(preview("att://only").as_deref(), Some("[media]"));
    // No token: the caps cut the preview from the body as usual.
    assert_eq!(preview("plain text"), None);
    // An opening delimiter without its closing one is ordinary text.
    assert_eq!(preview("@@@m:never closed"), None);
    assert_eq!(
        preview("@@@m:open att://a b").as_deref(),
        Some("@@@m:open [media] b")
    );
    // A token that starts beyond the preview does not matter, and the preview keeps its cap.
    let far = format!("{}@@@m:x@@@", "a".repeat(caps::PREVIEW_BYTES));
    assert_eq!(preview(&far), None);
    let long = format!("@@@m:x@@@{}", "é".repeat(caps::PREVIEW_BYTES));
    let cut = preview(&long).unwrap();
    assert!(cut.starts_with("[media]é") && cut.len() <= caps::PREVIEW_BYTES);
}

#[test]
fn keyed_candidates_merge_by_rank_and_keep_the_cap() {
    let keyed = |key: &'static str, value: u64, rank: u16| Keyed {
        key: Cow::Borrowed(key),
        value,
        rank,
        origin: Origin::None,
    };
    let winners = merge_keyed(
        vec![
            keyed("b", 1, 5),
            keyed("a", 2, 9),
            keyed("b", 3, 2),
            keyed("b", 4, 2),
        ],
        8,
    );
    let pairs: Vec<(&str, u64)> = winners.iter().map(|w| (&*w.key, w.value)).collect();
    assert_eq!(pairs, [("a", 2), ("b", 3)]);
    // Beyond the cap the highest ranks go first, then the greatest names.
    let capped = merge_keyed(
        vec![
            keyed("z", 1, 1),
            keyed("y", 2, 7),
            keyed("x", 3, 7),
            keyed("w", 4, 3),
        ],
        3,
    );
    let keys: Vec<&str> = capped.iter().map(|w| &*w.key).collect();
    assert_eq!(keys, ["w", "x", "z"]);
}

#[test]
fn a_tool_call_needs_a_name_and_keeps_its_arguments_as_sent() {
    static SHAPE: ToolCallShape = ToolCallShape {
        id: &["id"],
        name: &["function", "name"],
        arguments: &["function", "arguments"],
    };
    let list = Node::parse(
        r#"[{"id":"c1","function":{"name":"f","arguments":"{\"a\":1}"}},
            {"id":"c2","function":{"arguments":{}}},
            {"function":{"name":"g","arguments":{"b":2}}},
            "not a call"]"#,
    )
    .unwrap();
    let calls = Node::Arr(tool_calls(&list, &SHAPE));
    assert_eq!(
        String::from_utf8(calls.render()).unwrap(),
        r#"[{"id":"c1","name":"f","arguments":"{\"a\":1}"},{"name":"g","arguments":{"b":2}}]"#
    );
}
