//! The writers (contract 12.8): both are total over any record, a `crucible-jsonl` line has exactly the keys
//! `schema`, `span` and `facts`, an `otlp-json` line is one resource-spans document holding one span, and the
//! attribute rules hold.

// Test code throughout: lets the helpers use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use bytes::Bytes;
use crucible_auth::test_support::scope;
use crucible_export::{Written, write_jsonl, write_otlp};
use crucible_plan::{FactTable, Value};
use crucible_query::{ExportColumn, ExportFact, ExportMcp, ExportRecord};
use crucible_records::{
    ApiKeyId, CostSource, Ingestion, IoShape, Level, Lsn, McpSide, NanoUsd, ObservationType,
    PayloadField, PayloadFields, ProjectId, ProjectScope, SourceFormat, SpanCore, SpanId, SpanKind,
    SpanPayload, SpanRecord, StatusCode, Timestamp, TraceId, TracePublic, Truncation, UsageEntry,
    Uuid, WalEpoch,
};
use crucible_schema::{
    Column, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol, McpOpsCol,
    Ty,
};
use proptest::prelude::*;
use serde_json::{Value as J, json};

fn project() -> ProjectScope {
    scope(ProjectId::from_bytes([0x19; 16])).unwrap()
}

/// A small deterministic stream of values from a seed.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A value for a column type, sometimes one that does not fit it (the writer must still answer).
fn value_of(ty: &Ty, s: &mut Seeded) -> Value {
    let n = s.next();
    if n.is_multiple_of(23) {
        return Value::String("misfit".to_owned());
    }
    match ty {
        Ty::Uuid => Value::Uuid(if n.is_multiple_of(3) {
            Uuid::NIL
        } else {
            Uuid::from_bytes(n.to_le_bytes().repeat(2).try_into().unwrap())
        }),
        Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64 => Value::UInt(n % 1_000),
        Ty::Int32 | Ty::Int64 => Value::Int(n as i64),
        Ty::Float32 | Ty::Float64 => Value::Float(if n.is_multiple_of(5) {
            f64::NAN
        } else {
            n as f64 / 7.0
        }),
        Ty::String | Ty::LowCardinalityString => match n % 4 {
            0 => Value::String(String::new()),
            1 => Value::String("{\"k\":[1,2]}".to_owned()),
            2 => Value::Bytes(vec![0xff, 0xfe, b'"']),
            _ => Value::String("text \"quoted\"\n".to_owned()),
        },
        Ty::FixedString(len) => Value::Bytes(vec![
            if n.is_multiple_of(2) { 0 } else { n as u8 };
            usize::from(*len)
        ]),
        Ty::Date => Value::Date(Timestamp::from_micros((n % 4_000_000_000_000_000) as i64).date()),
        Ty::DateTime | Ty::DateTime64 => {
            Value::Time(Timestamp::from_micros((n % 4_000_000_000_000_000) as i64))
        }
        Ty::Enum8(kind) => Value::Enum(if n.is_multiple_of(7) {
            99
        } else {
            kind.variants()[n as usize % kind.variants().len()].1
        }),
        Ty::Nullable(inner) => {
            if n.is_multiple_of(2) {
                Value::Null
            } else {
                value_of(inner, s)
            }
        }
        Ty::Array(inner) => Value::Array((0..n % 3).map(|_| value_of(inner, s)).collect()),
        Ty::Tuple2(a, b) => Value::Tuple2(Box::new(value_of(a, s)), Box::new(value_of(b, s))),
        Ty::Saf(_, inner) => value_of(inner, s),
    }
}

fn columns<C: Column>(seed: u64) -> Vec<ExportColumn> {
    let mut s = Seeded(seed | 1);
    C::ALL
        .iter()
        .map(|c| {
            let column = c.column_ref();
            ExportColumn {
                name: column.name(),
                ty: *column.ty(),
                value: value_of(column.ty(), &mut s),
            }
        })
        .collect()
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "\\PC{0,10}",
        "[\\x00-\\x1f\"\\\\a]{0,4}"
    ]
}

fn body() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(Vec::new()),
        prop::collection::vec(any::<u8>(), 0..40),
        Just(br#"[{"role":"user","parts":[{"type":"text","content":"hi"}]}]"#.to_vec()),
        Just(b"{\n  \"messages\": [ {\"role\": \"user\", \"content\": \"x\"} ],\n \"a\" : 1.0 }".to_vec()),
        Just(br#"{"gen_ai.operation.name":"chat","k":1,"k":"dup","n":null,"f":2.5,"l":[true,{"z":"q"}]}"#.to_vec()),
        Just(br#"[{"time":"2026-11-17T09:30:00.000001Z","name":"e","attributes":{"a":1}}]"#.to_vec()),
        "\\PC{0,20}".prop_map(String::into_bytes),
    ]
}

fn pick<T: Copy>(all: &[T], n: u8) -> T {
    all[usize::from(n) % all.len()]
}

prop_compose! {
    fn core_strategy()(
        ids in (any::<[u8; 16]>(), any::<[u8; 8]>(), any::<[u8; 8]>()),
        strings in prop::collection::vec(text(), 25),
        lists in prop::collection::vec(prop::collection::vec(text(), 0..3), 4),
        enums in prop::collection::vec(any::<u8>(), 11),
        numbers in prop::collection::vec(any::<u64>(), 9),
        times in (0i64..4_102_444_800_000_000, 0i64..10_000_000, prop::option::of(0i64..4_102_444_800_000_000)),
        flags in prop::collection::vec(any::<bool>(), 5),
        usage in prop::collection::vec(("[a-z_]{1,8}", any::<u64>(), any::<i64>()), 0..4),
        digests in (prop::option::of(any::<[u8; 32]>()), prop::option::of(any::<[u8; 32]>())),
    ) -> SpanCore {
        let s = |i: usize| -> Box<str> { strings[i].as_str().into() };
        let l = |i: usize| -> Vec<Box<str>> { lists[i].iter().map(|x| x.as_str().into()).collect() };
        SpanCore {
            project_id: project().project_id(),
            trace_id: TraceId::from_bytes(ids.0),
            span_id: SpanId::from_bytes(ids.1),
            parent_span_id: SpanId::from_bytes(if flags[0] { [0; 8] } else { ids.2 }),
            is_root: flags[0],
            is_app_root: flags[1],
            kind: pick(SpanKind::ALL, enums[0]),
            name: s(0),
            start_time: Timestamp::from_micros(times.0),
            end_time: Timestamp::from_micros(times.0.saturating_add(times.1)),
            completion_start_time: times.2.map(Timestamp::from_micros),
            status_code: pick(StatusCode::ALL, enums[1]),
            status_message: s(1),
            service_name: s(2), service_version: s(3), scope_name: s(4), scope_version: s(5),
            telemetry_sdk_name: s(6), telemetry_sdk_language: s(7), telemetry_sdk_version: s(8),
            sdk_name: s(9), sdk_version: s(10),
            observation_type: pick(ObservationType::ALL, enums[2]),
            level: pick(Level::ALL, enums[3]),
            environment: s(11), release: s(12), version: s(13), trace_name: s(14), user_id: s(15),
            session_id: s(16),
            trace_tags: l(0),
            trace_public: pick(TracePublic::ALL, enums[4]),
            tags: l(1),
            model_requested: s(17), model_response: s(18), provider: s(19),
            finish_reasons: l(2),
            usage: usage.iter().map(|(k, u, c)| UsageEntry { key: k.as_str().into(), units: *u, cost: NanoUsd::from_nanos(*c) }).collect(),
            input_tokens: numbers[0], output_tokens: numbers[1], total_tokens: numbers[2],
            total_cost_nusd: NanoUsd::from_nanos(numbers[3] as i64),
            cost_source: pick(CostSource::ALL, enums[5]),
            price_model_id: flags[2].then(|| Uuid::from_bytes(ids.0)),
            price_tier_id: None,
            usage_aggregate: flags[3],
            prompt_name: s(20), prompt_version: s(21),
            tool_names: l(3),
            tool_call_count: numbers[4] as u32,
            mcp_side: pick(McpSide::ALL, enums[6]),
            io_shape: pick(IoShape::ALL, enums[7]),
            input_preview: s(22), output_preview: s(23),
            input_bytes: numbers[5], output_bytes: numbers[6],
            input_truncation: pick(Truncation::ALL, enums[8]),
            output_truncation: pick(Truncation::ALL, enums[9]),
            input_sha256: digests.0, output_sha256: digests.1,
            source_format: pick(SourceFormat::ALL, enums[10]),
            compat_dialect: enums[10] % 8,
            normalizer_version: 1,
            clock_skew: flags[4],
            ingestion: Ingestion {
                wal_epoch: WalEpoch::from_bytes(ids.0),
                batch_seq: Lsn::new(numbers[7]),
                lsn: Lsn::new(numbers[8]),
                received_at: Timestamp::from_micros(times.0),
            },
            api_key_id: ApiKeyId::from_bytes(ids.0),
            row_digest: numbers[7],
            event_bytes: numbers[8] as u32,
        }
    }
}

prop_compose! {
    fn payload_strategy()(
        bodies in prop::collection::vec(body(), 13),
        metadata in prop::collection::vec(("[a-z.]{1,6}", prop_oneof![text(), Just("12".to_owned()), Just("{\"x\":true}".to_owned())]), 0..3),
        usage in prop::collection::vec(("[a-z_]{1,8}", any::<u64>()), 0..3),
        cost in prop::collection::vec(("[a-z_]{1,8}", 0i64..i64::MAX), 0..3),
        truncated in any::<u16>(),
    ) -> SpanPayload {
        let b = |i: usize| Bytes::from(bodies[i].clone());
        let mut truncated_fields = PayloadFields::EMPTY;
        for (i, field) in PayloadField::ALL.into_iter().enumerate() {
            if truncated & (1 << i) != 0 && truncated.is_multiple_of(3) {
                truncated_fields.insert(field);
            }
        }
        SpanPayload {
            input: b(0), output: b(1), system_instructions: b(2), metadata: Bytes::new(),
            metadata_keys: metadata.iter().map(|(k, _)| k.as_str().into()).collect(),
            metadata_values: metadata.iter().map(|(_, v)| v.as_str().into()).collect(),
            metadata_kv: metadata.iter().map(|(k, v)| format!("{k}={v}").into()).collect(),
            trace_metadata: b(3), attributes: b(4), resource_attributes: b(5), events: b(6), links: b(7),
            tool_definitions: b(8), tool_calls: b(9), model_parameters: b(10),
            usage_provided: usage.iter().map(|(k, u)| (k.as_str().into(), *u)).collect(),
            cost_provided: cost.iter().map(|(k, c)| (k.as_str().into(), NanoUsd::from_nanos(*c))).collect(),
            trace_state: b(11),
            truncated_fields,
        }
    }
}

fn facts(seed: u64) -> Vec<ExportFact> {
    let skip = ["target_day", "wal_epoch", "batch_seq", "lsn", "received_at"];
    let of = |table: FactTable, mut cols: Vec<ExportColumn>| {
        cols.retain(|c| !skip.contains(&c.name));
        ExportFact {
            table,
            columns: cols,
        }
    };
    vec![
        of(FactTable::Scores, columns::<FactScoresCol>(seed)),
        of(FactTable::Tags, columns::<FactTagsCol>(seed + 1)),
        of(FactTable::Flags, columns::<FactFlagsCol>(seed + 2)),
        of(
            FactTable::Corrections,
            columns::<FactCorrectionsCol>(seed + 3),
        ),
    ]
}

prop_compose! {
    fn record_strategy()(
        core in core_strategy(),
        payload in payload_strategy(),
        seed in any::<u64>(),
        mcp in any::<bool>(),
        stored in any::<bool>(),
    ) -> ExportRecord {
        ExportRecord {
            span: SpanRecord { core, payload },
            payload_stored: stored,
            mcp: mcp.then(|| ExportMcp { op: columns::<McpOpsCol>(seed), content: Some(columns::<McpContentCol>(seed ^ 5)) }),
            facts: if seed.is_multiple_of(2) { facts(seed) } else { Vec::new() },
        }
    }
}

fn one_line(out: &[u8]) -> J {
    assert_eq!(out.last(), Some(&b'\n'), "a line ends with a newline");
    assert_eq!(
        out.iter().filter(|b| **b == b'\n').count(),
        1,
        "exactly one line"
    );
    serde_json::from_slice(out).expect("valid JSON")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn both_writers_are_total(record in record_strategy(), keep in any::<bool>()) {
        let mut out = Vec::new();
        write_jsonl(project(), &record, &mut out);
        let line = one_line(&out);
        let keys: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
        prop_assert_eq!(keys, ["facts", "schema", "span"]);
        prop_assert_eq!(&line["schema"], "crucible.span/1");
        prop_assert_eq!(&line["span"]["trace_id"], &J::from(record.span.core.trace_id.to_string()));
        prop_assert_eq!(&line["span"]["project_id"], &J::from(project().project_id().to_string()));
        prop_assert_eq!(line["facts"].as_array().unwrap().len(), record.facts.len());
        prop_assert_eq!(line["span"].get("mcp").is_some(), record.mcp.is_some());

        let mut out = Vec::new();
        let (core, payload) = (&record.span.core, &record.span.payload);
        let cut = !payload.truncated_fields.is_empty()
            || core.input_truncation == Truncation::CapExceeded
            || core.output_truncation == Truncation::CapExceeded;
        match write_otlp(project(), &record, keep, &mut out) {
            Written::Skipped => {
                prop_assert!(cut && !keep);
                prop_assert!(out.is_empty());
            }
            Written::Line => {
                prop_assert!(!cut || keep);
                let doc = one_line(&out);
                let resources = doc["resourceSpans"].as_array().unwrap();
                prop_assert_eq!(resources.len(), 1);
                let scopes = resources[0]["scopeSpans"].as_array().unwrap();
                prop_assert_eq!(scopes.len(), 1);
                let spans = scopes[0]["spans"].as_array().unwrap();
                prop_assert_eq!(spans.len(), 1);
                prop_assert_eq!(&spans[0]["traceId"], &J::from(core.trace_id.to_string()));
                // Every attribute key appears once unless the residual itself repeats it.
                let keys: Vec<&str> = spans[0]["attributes"].as_array().unwrap().iter().map(|a| a["key"].as_str().unwrap()).collect();
                prop_assert!(keys.iter().filter(|k| **k == "crucible.observation.type").count() == 1);
            }
        }
    }
}

/// A generation as an OTel GenAI application sends it.
fn generation() -> ExportRecord {
    let core = SpanCore {
        project_id: project().project_id(),
        trace_id: TraceId::parse("c3a1e2f4b5d6478990a1b2c3d4e5f601").unwrap(),
        span_id: SpanId::parse("0d1e2f3a4b5c6d7e").unwrap(),
        parent_span_id: SpanId::parse("53995c3f42cd8ad8").unwrap(),
        is_root: false,
        is_app_root: false,
        kind: SpanKind::Client,
        name: "chat acme/large-1".into(),
        start_time: Timestamp::parse_rfc3339("2026-11-17T09:30:00.050000Z").unwrap(),
        end_time: Timestamp::parse_rfc3339("2026-11-17T09:30:00.850000Z").unwrap(),
        completion_start_time: Some(
            Timestamp::parse_rfc3339("2026-11-17T09:30:00.420000Z").unwrap(),
        ),
        status_code: StatusCode::Unset,
        status_message: "".into(),
        service_name: "support-app".into(),
        service_version: "1.4.0".into(),
        scope_name: "opentelemetry.instrumentation.genai".into(),
        scope_version: "0.9.0".into(),
        telemetry_sdk_name: "opentelemetry".into(),
        telemetry_sdk_language: "python".into(),
        telemetry_sdk_version: "1.38.0".into(),
        sdk_name: "".into(),
        sdk_version: "".into(),
        observation_type: ObservationType::Generation,
        level: Level::Default,
        environment: "production".into(),
        release: "1.4.0".into(),
        version: "".into(),
        trace_name: "".into(),
        user_id: "user-42".into(),
        session_id: "cr-session-1".into(),
        trace_tags: Vec::new(),
        trace_public: TracePublic::Unset,
        tags: Vec::new(),
        model_requested: "acme/large-1".into(),
        model_response: "acme/large-1".into(),
        provider: "acme".into(),
        finish_reasons: vec!["tool_call".into()],
        usage: vec![
            UsageEntry {
                key: "input".into(),
                units: 27,
                cost: NanoUsd::from_nanos(67_500),
            },
            UsageEntry {
                key: "output".into(),
                units: 11,
                cost: NanoUsd::from_nanos(110_000),
            },
            UsageEntry {
                key: "total".into(),
                units: 38,
                cost: NanoUsd::from_nanos(0),
            },
        ],
        input_tokens: 27,
        output_tokens: 11,
        total_tokens: 38,
        total_cost_nusd: NanoUsd::from_nanos(177_500),
        cost_source: CostSource::Calculated,
        price_model_id: None,
        price_tier_id: None,
        usage_aggregate: false,
        prompt_name: "".into(),
        prompt_version: "".into(),
        tool_names: vec!["lookup_order".into()],
        tool_call_count: 1,
        mcp_side: McpSide::None,
        io_shape: IoShape::GenaiMessages,
        input_preview: "".into(),
        output_preview: "".into(),
        input_bytes: 79,
        output_bytes: 155,
        input_truncation: Truncation::None,
        output_truncation: Truncation::None,
        input_sha256: None,
        output_sha256: None,
        source_format: SourceFormat::Otel,
        compat_dialect: 0,
        normalizer_version: 1,
        clock_skew: false,
        ingestion: Ingestion {
            wal_epoch: WalEpoch::from_bytes([0x10; 16]),
            batch_seq: Lsn::new(4104),
            lsn: Lsn::new(4106),
            received_at: Timestamp::parse_rfc3339("2026-11-17T09:30:01.200000Z").unwrap(),
        },
        api_key_id: ApiKeyId::from_bytes([0x11; 16]),
        row_digest: 0x5a17_c0de_0000_03e6,
        event_bytes: 1134,
    };
    let payload = SpanPayload {
        input: Bytes::from_static(br#"[{"role":"user","parts":[{"type":"text","content":"Where is order ORD-104?"}]}]"#),
        output: Bytes::from_static(br#"[{"role":"assistant","parts":[{"type":"tool_call","id":"call-cr-1","name":"lookup_order","arguments":{"order_id":"ORD-104"}}],"finish_reason":"tool_call"}]"#),
        system_instructions: Bytes::from_static(br#"[{"type":"text","content":"Help the customer."}]"#),
        attributes: Bytes::from_static(br#"{"gen_ai.operation.name":"chat","rightmodeler.family":"support","server.address":"api.acme.test"}"#),
        resource_attributes: Bytes::from_static(br#"{"host.name":"support-1"}"#),
        model_parameters: Bytes::from_static(br#"{"temperature":0}"#),
        usage_provided: vec![("input".into(), 27), ("output".into(), 11)],
        ..SpanPayload::default()
    };
    ExportRecord {
        span: SpanRecord { core, payload },
        payload_stored: true,
        mcp: None,
        facts: Vec::new(),
    }
}

#[test]
fn a_generation_has_the_shape_of_the_crucible_span_schema() {
    let mut out = Vec::new();
    write_jsonl(project(), &generation(), &mut out);
    let line = one_line(&out);
    let span = &line["span"];
    let expected = json!({
        "trace_id": "c3a1e2f4b5d6478990a1b2c3d4e5f601",
        "span_id": "0d1e2f3a4b5c6d7e",
        "parent_span_id": "53995c3f42cd8ad8",
        "kind": "client",
        "observation_type": "generation",
        "start_time": "2026-11-17T09:30:00.050000Z",
        "duration_us": 800000,
        "completion_start_time": "2026-11-17T09:30:00.420000Z",
        "status_code": "unset",
        "model_requested": "acme/large-1",
        "finish_reasons": ["tool_call"],
        "usage": {"input": 27, "output": 11, "total": 38},
        "cost_usd": [0.0000675, 0.00011, 0.0],
        "total_cost_usd": 0.0001775,
        "cost_source": "calculated",
        "price_model_id": null,
        "io_shape": "genai_messages",
        "row_digest": "5a17c0de000003e6",
        "input": [{"role": "user", "parts": [{"type": "text", "content": "Where is order ORD-104?"}]}],
        "system_instructions": [{"type": "text", "content": "Help the customer."}],
        "metadata": {},
        "attributes": {"gen_ai.operation.name": "chat", "rightmodeler.family": "support", "server.address": "api.acme.test"},
        "events": "",
        "model_parameters": {"temperature": 0},
        "usage_provided": {"input": 27, "output": 11},
        "cost_provided_usd": {},
        "truncated_fields": [],
        "payload_stored": true,
    });
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&span[key], value, "{key}");
    }
    assert!(span.get("mcp").is_none());
    assert!(span.get("saturated").is_none());
}

fn attributes(doc: &J) -> Vec<(String, J)> {
    doc["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["key"].as_str().unwrap().to_owned(), a["value"].clone()))
        .collect()
}

fn attribute<'a>(attrs: &'a [(String, J)], key: &str) -> Option<&'a J> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

#[test]
fn a_generation_goes_out_under_the_genai_keys_with_its_residual_once() {
    let mut out = Vec::new();
    assert_eq!(
        write_otlp(project(), &generation(), false, &mut out),
        Written::Line
    );
    let doc = one_line(&out);
    let attrs = attributes(&doc);
    let string = |key: &str| attribute(&attrs, key).map(|v| v["stringValue"].clone());
    assert_eq!(
        string("gen_ai.request.model"),
        Some(J::from("acme/large-1"))
    );
    assert_eq!(string("gen_ai.provider.name"), Some(J::from("acme")));
    let input: J =
        serde_json::from_str(string("gen_ai.input.messages").unwrap().as_str().unwrap()).unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(
        attribute(&attrs, "gen_ai.usage.input_tokens"),
        Some(&json!({"intValue": "27"}))
    );
    assert_eq!(
        attribute(&attrs, "gen_ai.request.temperature"),
        Some(&json!({"intValue": "0"}))
    );
    assert_eq!(string("crucible.session.id"), Some(J::from("cr-session-1")));
    assert_eq!(
        string("crucible.completion_start_time"),
        Some(J::from("2026-11-17T09:30:00.420000Z"))
    );
    // The residual already names the operation: it goes out once, as sent.
    assert_eq!(
        attrs
            .iter()
            .filter(|(k, _)| k == "gen_ai.operation.name")
            .count(),
        1
    );
    assert_eq!(string("rightmodeler.family"), Some(J::from("support")));
    assert!(attribute(&attrs, "crucible.io_shape").is_none());
    let resource = &doc["resourceSpans"][0]["resource"]["attributes"];
    assert!(
        resource
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "host.name")
    );
    let span = &doc["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["startTimeUnixNano"], "1794907800050000000");
    assert_eq!(span["kind"], 3);
}

#[test]
fn a_raw_value_is_wrapped_in_one_message_and_a_tool_keeps_its_arguments() {
    let mut record = generation();
    record.span.core.io_shape = IoShape::Raw;
    record.span.payload.input = Bytes::from_static(b"what is the time");
    record.span.payload.output =
        Bytes::from_static(br#"{"messages":[{"role":"assistant","content":"noon"}]}"#);
    let mut out = Vec::new();
    write_otlp(project(), &record, false, &mut out);
    let attrs = attributes(&one_line(&out));
    let messages = |key: &str| -> J {
        serde_json::from_str(
            attribute(&attrs, key).unwrap()["stringValue"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        messages("gen_ai.input.messages"),
        json!([{"role": "user", "parts": [{"type": "text", "content": "what is the time"}]}])
    );
    assert_eq!(
        messages("gen_ai.output.messages"),
        json!([{"role": "assistant", "content": "noon"}])
    );
    assert_eq!(
        attribute(&attrs, "crucible.io_shape"),
        Some(&json!({"stringValue": "raw"}))
    );

    record.span.core.observation_type = ObservationType::Tool;
    record.span.payload.input = Bytes::from_static(br#"{"order_id":"ORD-104"}"#);
    let mut out = Vec::new();
    write_otlp(project(), &record, false, &mut out);
    let attrs = attributes(&one_line(&out));
    assert!(attribute(&attrs, "gen_ai.input.messages").is_none());
    assert_eq!(
        attribute(&attrs, "gen_ai.tool.call.arguments"),
        Some(&json!({"stringValue": "{\"order_id\":\"ORD-104\"}"}))
    );
    assert_eq!(
        attribute(&attrs, "gen_ai.tool.name"),
        Some(&json!({"stringValue": "lookup_order"}))
    );
    // The residual names the operation `chat`, which goes out as sent; the type goes out as `crucible.*`.
    assert_eq!(
        attribute(&attrs, "crucible.observation.type"),
        Some(&json!({"stringValue": "tool"}))
    );
}

#[test]
fn a_cut_span_is_skipped_unless_kept() {
    let mut record = generation();
    record.span.core.output_truncation = Truncation::CapExceeded;
    record
        .span
        .payload
        .truncated_fields
        .insert(PayloadField::Output);
    record
        .span
        .payload
        .truncated_fields
        .insert(PayloadField::Attributes);
    record.span.payload.attributes = Bytes::from_static(b"{\"cut");
    let mut out = Vec::new();
    assert_eq!(
        write_otlp(project(), &record, false, &mut out),
        Written::Skipped
    );
    assert!(out.is_empty());
    assert_eq!(
        write_otlp(project(), &record, true, &mut out),
        Written::Line
    );
    let attrs = attributes(&one_line(&out));
    assert_eq!(
        attribute(&attrs, "crucible.truncated_fields"),
        Some(&json!({"arrayValue": {"values": [{"stringValue": "attributes"}]}}))
    );
    assert!(attribute(&attrs, "rightmodeler.family").is_none());

    // In crucible-jsonl the cut fields are opaque text and carry their markers.
    let mut out = Vec::new();
    write_jsonl(project(), &record, &mut out);
    let span = one_line(&out)["span"].clone();
    assert_eq!(span["attributes"], "{\"cut");
    assert!(span["output"].is_string());
    assert_eq!(span["truncated_fields"], json!(["output", "attributes"]));
    assert_eq!(span["output_truncation"], "cap_exceeded");
}
