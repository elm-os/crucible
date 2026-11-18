//! Dialect 0 mapping through the production mapper, and the stage A rules around it: observation types (contract
//! 10.4), consumed and residual attributes, provenance, the GenAI fields, times (contract 1.2), ids, the rejected
//! bitmap and the dialect choice (contract 10.2).

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::double::Double;
use common::{
    BASE_NANOS, RECEIVED_AT, bitmap, ctx, frame, int, kv, kvlist, request, request_with, run, s,
    span, span_with, strings, value,
};
use crucible_normalize::{NormalizeError, normalize, normalize_with};
use crucible_records::{
    ApiKeyId, IoShape, Level, Lsn, ObservationType, ProjectId, SourceFormat, SpanId, SpanKind,
    StatusCode, Timestamp, TraceId, WalEpoch,
};
use opentelemetry_proto::tonic::common::v1::{ArrayValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::span::{Event, Link};
use opentelemetry_proto::tonic::trace::v1::{Span, Status};
use prost::Message;

fn one(attributes: Vec<KeyValue>) -> crucible_records::SpanRecord {
    run(&request(vec![span_with(1, 1, None, attributes)]))
        .spans
        .remove(0)
}

fn kind_of(attributes: Vec<KeyValue>) -> ObservationType {
    one(attributes).core.observation_type
}

fn residual(r: &crucible_records::SpanRecord) -> &str {
    core::str::from_utf8(&r.payload.attributes).unwrap()
}

#[test]
fn operation_names_map_to_observation_types() {
    use ObservationType as T;
    for (name, expected) in [
        ("chat", T::Generation),
        ("text_completion", T::Generation),
        ("generate_content", T::Generation),
        ("embeddings", T::Embedding),
        ("execute_tool", T::Tool),
        ("create_agent", T::Agent),
        ("invoke_agent", T::Agent),
        // Operations the contract does not map, and unknown or differently spelled values, fall through.
        ("invoke_workflow", T::Span),
        ("retrieval", T::Span),
        ("Chat", T::Span),
        ("", T::Span),
    ] {
        assert_eq!(
            kind_of(vec![s("gen_ai.operation.name", name)]),
            expected,
            "{name}"
        );
    }
    assert_eq!(kind_of(vec![int("gen_ai.operation.name", 1)]), T::Span);
    assert_eq!(kind_of(Vec::new()), T::Span);
}

#[test]
fn observation_type_rules_apply_in_priority_order() {
    use ObservationType as T;
    // Only tools/call yields a type from the MCP method; other methods fall through to the next rule.
    assert_eq!(kind_of(vec![s("mcp.method.name", "tools/call")]), T::Tool);
    assert_eq!(
        kind_of(vec![
            s("mcp.method.name", "prompts/get"),
            s("gen_ai.operation.name", "chat")
        ]),
        T::Generation
    );
    // The MCP rule outranks the operation name, whatever the wire order.
    assert_eq!(
        kind_of(vec![
            s("gen_ai.operation.name", "chat"),
            s("mcp.method.name", "tools/call")
        ]),
        T::Tool
    );
    // The explicit type outranks both.
    assert_eq!(
        kind_of(vec![
            s("mcp.method.name", "tools/call"),
            s("gen_ai.operation.name", "chat"),
            s("crucible.observation.type", "guardrail"),
        ]),
        T::Guardrail
    );
    for t in ObservationType::ALL {
        assert_eq!(
            kind_of(vec![s("crucible.observation.type", t.as_str())]),
            *t
        );
    }
    // An unknown explicit value yields nothing.
    assert_eq!(
        kind_of(vec![
            s("crucible.observation.type", "GENERATION"),
            s("gen_ai.operation.name", "embeddings")
        ]),
        T::Embedding
    );
}

#[test]
fn only_the_explicit_type_key_is_consumed() {
    let r = one(vec![
        s("crucible.observation.type", "agent"),
        s("mcp.method.name", "tools/call"),
        s("gen_ai.operation.name", "chat"),
    ]);
    assert_eq!(
        residual(&r),
        r#"{"mcp.method.name":"tools/call","gen_ai.operation.name":"chat"}"#
    );
    // A value that yields no type stays.
    let r = one(vec![s("crucible.observation.type", "nope")]);
    assert_eq!(residual(&r), r#"{"crucible.observation.type":"nope"}"#);
}

#[test]
fn provenance_comes_from_the_resource_scope_and_envelope() {
    let req = request_with(
        vec![
            s("service.name", "checkout"),
            s("service.version", "2.1.0"),
            s("telemetry.sdk.name", "opentelemetry"),
            s("telemetry.sdk.language", "rust"),
            s("telemetry.sdk.version", "0.31.0"),
            s("host.name", "box-1"),
            int("service.name", 5),
        ],
        vec![span(1, 1, None)],
    );
    let body = req.encode_to_vec();
    let mut f = frame(&body, &[]);
    (f.sdk_name, f.sdk_version) = ("crucible-sdk", "0.1.0");
    let r = &normalize(&f, &ctx()).unwrap().spans[0];
    let c = &r.core;
    assert_eq!(
        (&*c.service_name, &*c.service_version),
        ("checkout", "2.1.0")
    );
    assert_eq!(
        (
            &*c.telemetry_sdk_name,
            &*c.telemetry_sdk_language,
            &*c.telemetry_sdk_version
        ),
        ("opentelemetry", "rust", "0.31.0")
    );
    assert_eq!((&*c.scope_name, &*c.scope_version), ("scope.name", "1.0.0"));
    assert_eq!((&*c.sdk_name, &*c.sdk_version), ("crucible-sdk", "0.1.0"));
    // Consumed keys leave the residual; the losing second `service.name` (wrong type) stays.
    assert_eq!(
        core::str::from_utf8(&r.payload.resource_attributes).unwrap(),
        r#"{"host.name":"box-1","service.name":5}"#
    );
}

#[test]
fn genai_fields_map_and_leave_the_residual() {
    let r = one(vec![
        s("gen_ai.request.model", "model-a"),
        s("gen_ai.response.model", "model-a-2026"),
        s("gen_ai.provider.name", "provider-x"),
        s("gen_ai.prompt.name", "greeting"),
        strings("gen_ai.response.finish_reasons", &["stop", "length"]),
        s("gen_ai.input.messages", r#"[{"role":"user","parts":[]}]"#),
        kv(
            "gen_ai.output.messages",
            any_value::Value::ArrayValue(ArrayValue {
                values: vec![value(any_value::Value::StringValue("hi".to_owned()))],
            }),
        ),
        s("gen_ai.system_instructions", r#"[{"type":"text"}]"#),
        s("gen_ai.tool.definitions", "[]"),
        s("user.id", "user-7"),
        s("gen_ai.agent.name", "planner"),
    ]);
    let (c, p) = (&r.core, &r.payload);
    assert_eq!(
        (&*c.model_requested, &*c.model_response, &*c.provider),
        ("model-a", "model-a-2026", "provider-x")
    );
    assert_eq!(&*c.prompt_name, "greeting");
    assert_eq!(c.finish_reasons, vec!["stop".into(), "length".into()]);
    assert_eq!(&*c.user_id, "user-7");
    assert_eq!(c.io_shape, IoShape::GenaiMessages);
    // A string body is kept verbatim; a structured one is rendered as JSON.
    assert_eq!(&p.input[..], br#"[{"role":"user","parts":[]}]"#);
    assert_eq!(&p.output[..], br#"["hi"]"#);
    assert_eq!(&p.system_instructions[..], br#"[{"type":"text"}]"#);
    assert_eq!(&p.tool_definitions[..], b"[]");
    assert_eq!(&*c.input_preview, r#"[{"role":"user","parts":[]}]"#);
    // The agent name has no field, so it stays.
    assert_eq!(residual(&r), r#"{"gen_ai.agent.name":"planner"}"#);
}

#[test]
fn the_first_good_candidate_wins_and_the_rest_stay() {
    let r = one(vec![
        s("gen_ai.request.model", ""),
        int("gen_ai.request.model", 3),
        s("gen_ai.request.model", "first"),
        s("gen_ai.request.model", "second"),
        kv(
            "gen_ai.response.finish_reasons",
            any_value::Value::ArrayValue(ArrayValue {
                values: vec![
                    value(any_value::Value::StringValue("stop".to_owned())),
                    value(any_value::Value::IntValue(1)),
                ],
            }),
        ),
    ]);
    assert_eq!(&*r.core.model_requested, "first");
    assert!(r.core.finish_reasons.is_empty());
    assert_eq!(
        residual(&r),
        r#"{"gen_ai.request.model":"","gen_ai.request.model":3,"gen_ai.request.model":"second","gen_ai.response.finish_reasons":["stop",1]}"#
    );
    // A single string is a one-item list.
    let r = one(vec![s("gen_ai.response.finish_reasons", "stop")]);
    assert_eq!(r.core.finish_reasons, vec!["stop".into()]);
}

#[test]
fn io_shape_follows_the_body_sources() {
    assert_eq!(one(Vec::new()).core.io_shape, IoShape::Raw);
    assert_eq!(
        one(vec![s("gen_ai.output.messages", "[]")]).core.io_shape,
        IoShape::GenaiMessages
    );
    // Input and output take text too, labelled with the shape of their source.
    let text = one(vec![s("gen_ai.input.messages", "hello")]);
    assert_eq!(&text.payload.input[..], b"hello");
    assert_eq!(text.core.io_shape, IoShape::GenaiMessages);
    assert_eq!(
        one(vec![s("gen_ai.system_instructions", "[]")])
            .core
            .io_shape,
        IoShape::Raw
    );
}

#[test]
fn json_bodies_take_only_json() {
    let r = one(vec![
        s("gen_ai.system_instructions", "be brief"),
        s("gen_ai.tool.definitions", "{\"a\":"),
    ]);
    assert!(r.payload.system_instructions.is_empty() && r.payload.tool_definitions.is_empty());
    assert_eq!(
        residual(&r),
        r#"{"gen_ai.system_instructions":"be brief","gen_ai.tool.definitions":"{\"a\":"}"#
    );
    // JSON text is kept verbatim; a scalar value is rendered as JSON.
    let r = one(vec![
        s("gen_ai.system_instructions", " \"be brief\" "),
        kv("gen_ai.tool.definitions", any_value::Value::BoolValue(true)),
    ]);
    assert_eq!(&r.payload.system_instructions[..], b" \"be brief\" ");
    assert_eq!(&r.payload.tool_definitions[..], b"true");
    assert!(r.payload.attributes.is_empty());
}

#[test]
fn residual_json_renders_every_value_type() {
    let r = one(vec![
        s("str", "a\"b\n"),
        kv("bool", any_value::Value::BoolValue(false)),
        int("int", i64::MIN),
        kv("double", any_value::Value::DoubleValue(1.5)),
        kv("nan", any_value::Value::DoubleValue(f64::NAN)),
        kv("bytes", any_value::Value::BytesValue(vec![0xde, 0xad])),
        kv("profile", any_value::Value::StringValueStrindex(3)),
        KeyValue {
            key: "unset".to_owned(),
            value: None,
            key_strindex: 0,
        },
        strings("list", &["x"]),
        kvlist("map", vec![int("n", 1), s("n", "dup")]),
        s("dup", "1"),
        s("dup", "2"),
    ]);
    assert_eq!(
        residual(&r),
        concat!(
            r#"{"str":"a\"b\n","bool":false,"int":-9223372036854775808,"double":1.5,"nan":"NaN","#,
            r#""bytes":"dead","profile":null,"unset":null,"list":["x"],"map":{"n":1,"n":"dup"},"#,
            r#""dup":"1","dup":"2"}"#
        )
    );
}

#[test]
fn events_and_links_are_stored_as_json_lists() {
    let mut sp = span(1, 1, None);
    sp.events = vec![Event {
        time_unix_nano: BASE_NANOS + 1_500,
        name: "exception".to_owned(),
        attributes: vec![s("exception.type", "Oops")],
        dropped_attributes_count: 0,
    }];
    sp.links = vec![Link {
        trace_id: vec![0xab; 16],
        span_id: vec![0xcd; 8],
        trace_state: "k=v".to_owned(),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        flags: 0,
    }];
    sp.trace_state = "vendor=1".to_owned();
    let r = &run(&request(vec![sp])).spans[0];
    assert_eq!(
        core::str::from_utf8(&r.payload.events).unwrap(),
        r#"[{"time":"2026-11-17T09:30:00.000001Z","name":"exception","attributes":{"exception.type":"Oops"}}]"#
    );
    assert_eq!(
        core::str::from_utf8(&r.payload.links).unwrap(),
        concat!(
            r#"[{"trace_id":"abababababababababababababababab","span_id":"cdcdcdcdcdcdcdcd","#,
            r#""attributes":{}}]"#
        )
    );
    assert_eq!(&r.payload.trace_state[..], b"vendor=1");
    // A span without them stores nothing.
    let bare = &run(&request(vec![span(1, 2, None)])).spans[0];
    assert!(bare.payload.events.is_empty() && bare.payload.links.is_empty());
    assert!(bare.payload.attributes.is_empty() && bare.payload.trace_state.is_empty());
}

#[test]
fn level_follows_the_status() {
    let with_status = |code| {
        let mut sp = span(1, 1, None);
        sp.status = Some(Status {
            message: "boom".to_owned(),
            code,
        });
        run(&request(vec![sp])).spans.remove(0).core
    };
    let error = with_status(2);
    assert_eq!(
        (error.status_code, error.level),
        (StatusCode::Error, Level::Error)
    );
    assert_eq!(&*error.status_message, "boom");
    assert_eq!(with_status(1).level, Level::Default);
    // Out-of-range codes read as unset.
    assert_eq!(with_status(7).status_code, StatusCode::Unset);
}

fn timed(start: u64, end: u64) -> crucible_records::SpanCore {
    let sp = Span {
        start_time_unix_nano: start,
        end_time_unix_nano: end,
        ..span(1, 1, None)
    };
    run(&request(vec![sp])).spans.remove(0).core
}

#[test]
fn times_are_clamped_and_skew_is_flagged() {
    let micros = |nanos: u64| Timestamp::from_unix_nanos(nanos);
    let ok = timed(BASE_NANOS + 1_999, BASE_NANOS + 5_000_000);
    assert_eq!(
        (ok.start_time, ok.end_time),
        (micros(BASE_NANOS + 1_999), micros(BASE_NANOS + 5_000_000))
    );
    assert_eq!(ok.duration_us(), 4_999);
    assert!(!ok.clock_skew);
    // An end before the start is raised to the start, without a skew flag.
    let backwards = timed(BASE_NANOS, BASE_NANOS - 1_000_000);
    assert_eq!(backwards.end_time, backwards.start_time);
    assert!(!backwards.clock_skew);
    // Times at or past 2100 are clamped and flagged.
    let late = timed(u64::MAX, u64::MAX);
    assert_eq!(
        late.start_time.micros(),
        Timestamp::MAX_EXCLUSIVE.micros() - 1
    );
    assert!(late.clock_skew && late.end_time == late.start_time);
    let late_end = timed(BASE_NANOS, u64::MAX);
    assert!(late_end.clock_skew);
    // More than 24 h ahead of received_at is flagged; exactly 24 h is not.
    let day = 86_400_000_000_000u64;
    let received = u64::try_from(RECEIVED_AT.micros()).unwrap() * 1_000;
    assert!(!timed(received + day, received + day).clock_skew);
    assert!(timed(received + day + 1_000, received + day + 1_000).clock_skew);
}

#[test]
fn ids_kind_and_frame_values() {
    let sp = Span {
        kind: 3,
        ..span(1, 2, Some(1))
    };
    let root = Span {
        kind: 99,
        ..span(1, 1, None)
    };
    let body = request(vec![sp, root]);
    let out = run(&body);
    let (child, root) = (&out.spans[0].core, &out.spans[1].core);
    assert_eq!(child.trace_id, TraceId::from_bytes([1; 16]));
    assert_eq!(child.span_id, SpanId::from_bytes([2; 8]));
    assert_eq!(child.parent_span_id, SpanId::from_bytes([1; 8]));
    assert!(!child.is_root && root.is_root && root.parent_span_id == SpanId::ZERO);
    assert_eq!(
        (child.kind, root.kind),
        (SpanKind::Client, SpanKind::Unspecified)
    );
    assert_eq!(&*child.name, "span 2");
    assert_eq!(child.project_id, ProjectId::from_bytes([0x22; 16]));
    assert_eq!(child.api_key_id, ApiKeyId::from_bytes([0x33; 16]));
    assert_eq!(child.ingestion.wal_epoch, WalEpoch::from_bytes([0x11; 16]));
    assert_eq!(
        (child.ingestion.lsn, child.ingestion.batch_seq),
        (Lsn::new(7), Lsn::ZERO)
    );
    assert_eq!(child.ingestion.received_at, RECEIVED_AT);
    assert_eq!(out.lsn, Lsn::new(7));
    assert_eq!(
        (child.source_format, child.compat_dialect),
        (SourceFormat::Otel, 0)
    );
    assert_eq!(&*child.environment, "default");
    let sizes: Vec<u32> = crucible_otlp::TraceRequest::scan(&body, None)
        .unwrap()
        .spans()
        .map(|s| s.encoded_len() as u32)
        .collect();
    assert_eq!(sizes, [child.event_bytes, root.event_bytes]);
}

#[test]
fn rejected_and_idless_spans_are_skipped_and_counted() {
    let no_trace = Span {
        trace_id: Vec::new(),
        ..span(1, 3, None)
    };
    let zero_span = Span {
        span_id: vec![0; 8],
        ..span(1, 4, None)
    };
    let body = request(vec![
        span(1, 1, None),
        span(1, 2, None),
        no_trace,
        zero_span,
        span(1, 5, None),
    ]);
    let out = normalize(&frame(&body, &bitmap(&[1, 3, 40])), &ctx()).unwrap();
    let ids: Vec<SpanId> = out.spans.iter().map(|r| r.core.span_id).collect();
    assert_eq!(
        ids,
        [SpanId::from_bytes([1; 8]), SpanId::from_bytes([5; 8])]
    );
    assert_eq!(
        (
            out.summary.spans,
            out.summary.rejected_before_ack,
            out.summary.missing_ids
        ),
        (5, 2, 1)
    );
}

#[test]
fn a_body_that_does_not_scan_fails_the_frame() {
    let err = normalize(&frame(&[0x0a, 0x05, 0x01], &[]), &ctx()).unwrap_err();
    assert!(matches!(err, NormalizeError::Scan(_)));
    // An empty body is an empty request.
    assert!(run(&[]).spans.is_empty());
}

#[test]
fn a_hint_without_a_mapper_falls_back_to_dialect_0() {
    let body = request(vec![span(1, 1, None)]);
    for hint in 1..=7 {
        let mut f = frame(&body, &[]);
        f.compat_dialect = hint;
        let r = &normalize(&f, &ctx()).unwrap().spans[0];
        assert_eq!(
            (r.core.compat_dialect, r.core.source_format),
            (0, SourceFormat::Otel)
        );
    }
}

#[test]
fn the_dialect_is_the_hint_else_the_most_confident_else_0() {
    let body = request(vec![span(1, 1, None)]);
    let mapped = |mappers: &[&dyn crucible_normalize::SpanMapper], hint: u8| {
        let mut f = frame(&body, &[]);
        f.compat_dialect = hint;
        let r = normalize_with(mappers, &f, &ctx()).unwrap().spans.remove(0);
        (
            r.core.compat_dialect,
            r.core.source_format,
            r.core.prompt_version.to_string(),
        )
    };
    let native = Double::NATIVE;
    let (a, b, c) = (
        Double {
            id: 3,
            confidence: Some(5),
        },
        Double {
            id: 2,
            confidence: Some(5),
        },
        Double {
            id: 6,
            confidence: Some(9),
        },
    );
    // The hint wins over detection.
    assert_eq!(
        mapped(&[&native, &a, &c], 3),
        (3, SourceFormat::Compat, "double 3".into())
    );
    // Highest confidence, ties to the lowest id.
    assert_eq!(mapped(&[&native, &a, &b], 0).0, 2);
    assert_eq!(mapped(&[&native, &a, &b, &c], 7).0, 6);
    // Nothing detects: dialect 0.
    assert_eq!(
        mapped(&[&native], 0),
        (0, SourceFormat::Otel, "double 0".into())
    );
    // No mapper applies: the span keeps only what stage A fills.
    assert_eq!(mapped(&[], 0), (0, SourceFormat::Otel, String::new()));
}
