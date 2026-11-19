//! The dialect 0 source keys of contract 10.6: every row of its table, the priority order of each field's sources,
//! which attributes are consumed, the map fields, the request-local forward-fill through the native keys, and the
//! identity of the protobuf and JSON encodings.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{
    BASE_NANOS, ctx, frame, int, kv, kvlist, request, request_with, run, s, span, span_with,
    strings,
};
use crucible_compat::{DIALECT_0, KeyMatch, Target};
use crucible_normalize::{NORMALIZER_VERSION, normalize};
use crucible_otlp::transcode_json;
use crucible_records::{
    CostSource, IoShape, Level, NanoUsd, SpanRecord, StatusCode, Timestamp, TracePublic, caps,
};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::Status;
use proptest::collection::{btree_map, vec};
use proptest::prelude::*;
use prost::Message;

fn one(attributes: Vec<KeyValue>) -> SpanRecord {
    with_resource(Vec::new(), attributes)
}

fn with_resource(resource: Vec<KeyValue>, attributes: Vec<KeyValue>) -> SpanRecord {
    let body = request_with(resource, vec![span_with(1, 1, None, attributes)]).encode_to_vec();
    run(&body).spans.remove(0)
}

/// The provider key that feeds the `service_tier` model parameter: the one exact source of `model_parameters`.
/// Its name lives in the compat table only.
fn service_tier_key() -> &'static str {
    DIALECT_0
        .fields
        .iter()
        .filter(|rule| rule.target == Target::ModelParameters)
        .flat_map(|rule| rule.sources)
        .find_map(|source| match source.key {
            KeyMatch::Exact(key) => Some(key),
            KeyMatch::Prefix(_) => None,
        })
        .unwrap()
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap()
}

fn flag(key: &str, value: bool) -> KeyValue {
    kv(key, any_value::Value::BoolValue(value))
}

fn double(key: &str, value: f64) -> KeyValue {
    kv(key, any_value::Value::DoubleValue(value))
}

fn strs(items: &[Box<str>]) -> Vec<&str> {
    items.iter().map(|s| &**s).collect()
}

#[test]
fn records_carry_normalizer_version_5() {
    assert_eq!(NORMALIZER_VERSION, 5);
    assert_eq!(one(Vec::new()).core.normalizer_version, 5);
}

#[test]
fn the_trace_name_key_fills_the_trace_name() {
    let r = one(vec![s("crucible.trace.name", "checkout"), s("other", "x")]);
    assert_eq!(&*r.core.trace_name, "checkout");
    assert_eq!(text(&r.payload.attributes), r#"{"other":"x"}"#);
    assert_eq!(&*one(Vec::new()).core.trace_name, "");
}

#[test]
fn session_sources_apply_in_priority_order() {
    let all = || {
        vec![
            s("gen_ai.conversation.id", "conversation"),
            s("session.id", "otel"),
            s("crucible.session.id", "native"),
        ]
    };
    let r = one(all());
    assert_eq!(&*r.core.session_id, "native");
    // Only the winning attribute is consumed.
    assert_eq!(
        text(&r.payload.attributes),
        r#"{"gen_ai.conversation.id":"conversation","session.id":"otel"}"#
    );
    assert_eq!(&*one(all()[..2].to_vec()).core.session_id, "otel");
    assert_eq!(&*one(all()[..1].to_vec()).core.session_id, "conversation");
    // An empty or non-string value is no candidate.
    let r = one(vec![
        s("crucible.session.id", ""),
        int("session.id", 5),
        s("gen_ai.conversation.id", "c"),
    ]);
    assert_eq!(&*r.core.session_id, "c");
}

#[test]
fn user_sources_apply_in_priority_order() {
    let r = one(vec![s("user.id", "otel"), s("crucible.user.id", "native")]);
    assert_eq!(&*r.core.user_id, "native");
    assert_eq!(text(&r.payload.attributes), r#"{"user.id":"otel"}"#);
    assert_eq!(&*one(vec![s("user.id", "otel")]).core.user_id, "otel");
}

#[test]
fn environment_sources_apply_in_priority_order_and_default() {
    let resource = || {
        vec![
            s("deployment.environment", "old"),
            s("deployment.environment.name", "new"),
        ]
    };
    let r = with_resource(resource(), vec![s("crucible.environment", "native")]);
    assert_eq!(&*r.core.environment, "native");
    assert_eq!(text(&r.payload.attributes), "");
    assert_eq!(
        text(&r.payload.resource_attributes),
        r#"{"deployment.environment":"old","deployment.environment.name":"new"}"#
    );
    let r = with_resource(resource(), Vec::new());
    assert_eq!(&*r.core.environment, "new");
    assert_eq!(
        text(&r.payload.resource_attributes),
        r#"{"deployment.environment":"old"}"#
    );
    let r = with_resource(resource()[..1].to_vec(), Vec::new());
    assert_eq!(&*r.core.environment, "old");
    // The span key is read on the span, the resource keys on the resource.
    let r = with_resource(
        vec![s("crucible.environment", "x")],
        vec![s("deployment.environment.name", "y")],
    );
    assert_eq!(&*r.core.environment, "default");
}

#[test]
fn a_span_without_an_environment_gets_the_default() {
    assert_eq!(&*one(Vec::new()).core.environment, "default");
    assert_eq!(
        &*one(vec![s("crucible.environment", "")]).core.environment,
        "default"
    );
    assert_eq!(
        &*one(vec![s("crucible.environment", "staging")])
            .core
            .environment,
        "staging"
    );
    // The cap applies.
    let long = "e".repeat(caps::ENVIRONMENT_BYTES + 10);
    assert_eq!(
        one(vec![s("crucible.environment", &long)])
            .core
            .environment
            .len(),
        caps::ENVIRONMENT_BYTES
    );
}

#[test]
fn release_reads_its_key_then_the_service_version() {
    let resource = || vec![s("service.version", "1.4.2")];
    let r = with_resource(resource(), vec![s("crucible.release", "2026-11-17")]);
    assert_eq!(&*r.core.release, "2026-11-17");
    assert_eq!(&*r.core.service_version, "1.4.2");
    let r = with_resource(resource(), Vec::new());
    assert_eq!(&*r.core.release, "1.4.2");
    assert_eq!(&*r.core.service_version, "1.4.2");
    // `service_version` consumes the resource key either way.
    assert_eq!(text(&r.payload.resource_attributes), "");
    assert_eq!(&*one(Vec::new()).core.release, "");
}

#[test]
fn the_version_key_fills_the_version() {
    let r = one(vec![s("crucible.version", "v7")]);
    assert_eq!(&*r.core.version, "v7");
    assert_eq!(text(&r.payload.attributes), "");
    // `service.version` is not a source of `version`.
    let r = with_resource(vec![s("service.version", "1")], Vec::new());
    assert_eq!(&*r.core.version, "");
}

#[test]
fn tags_and_trace_tags_are_capped_string_lists() {
    let r = one(vec![
        strings("crucible.tags", &["a", "b"]),
        strings("crucible.trace.tags", &["t"]),
    ]);
    assert_eq!(strs(&r.core.tags), ["a", "b"]);
    assert_eq!(strs(&r.core.trace_tags), ["t"]);
    assert_eq!(text(&r.payload.attributes), "");
    // One string is a list of one.
    assert_eq!(
        strs(&one(vec![s("crucible.tags", "solo")]).core.tags),
        ["solo"]
    );
    // An array with another element type does not decode and stays in the residual.
    let mixed = kv(
        "crucible.tags",
        any_value::Value::ArrayValue(opentelemetry_proto::tonic::common::v1::ArrayValue {
            values: vec![
                common::value(any_value::Value::StringValue("a".into())),
                common::value(any_value::Value::IntValue(1)),
            ],
        }),
    );
    let r = one(vec![mixed]);
    assert!(r.core.tags.is_empty());
    assert_eq!(text(&r.payload.attributes), r#"{"crucible.tags":["a",1]}"#);
    // 64 items of 256 bytes each.
    let many: Vec<String> = (0..80)
        .map(|i| format!("{i}-{}", "x".repeat(300)))
        .collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let r = one(vec![strings("crucible.tags", &refs)]);
    assert_eq!(r.core.tags.len(), caps::TAGS_MAX_ITEMS);
    assert!(r.core.tags.iter().all(|t| t.len() == caps::TAG_BYTES));
}

#[test]
fn metadata_is_flattened_from_its_prefix() {
    let r = one(vec![
        s("crucible.metadata.plan", "pro"),
        int("crucible.metadata.attempt", 3),
        flag("crucible.metadata.beta", true),
        kvlist("crucible.metadata.nested", vec![s("k", "v")]),
        // No member name, an unset value and an empty string: not read.
        s("crucible.metadata.", "nameless"),
        s("crucible.metadata.empty", ""),
        s("crucible.metadatax", "other key"),
    ]);
    let p = &r.payload;
    // Members of one source in wire order.
    assert_eq!(
        text(&p.metadata),
        r#"{"plan":"pro","attempt":3,"beta":true,"nested":{"k":"v"}}"#
    );
    assert_eq!(
        strs(&p.metadata_keys),
        ["plan", "attempt", "beta", "nested"]
    );
    assert_eq!(
        strs(&p.metadata_values),
        ["pro", "3", "true", r#"{"k":"v"}"#]
    );
    assert_eq!(
        strs(&p.metadata_kv),
        ["plan=pro", "attempt=3", "beta=true", r#"nested={"k":"v"}"#]
    );
    assert_eq!(
        text(&p.attributes),
        r#"{"crucible.metadata.":"nameless","crucible.metadata.empty":"","crucible.metadatax":"other key"}"#
    );
    assert!(p.trace_metadata.is_empty());
}

#[test]
fn a_repeated_metadata_key_keeps_its_first_occurrence() {
    let r = one(vec![
        s("crucible.metadata.k", "first"),
        s("crucible.metadata.k", "second"),
    ]);
    assert_eq!(text(&r.payload.metadata), r#"{"k":"first"}"#);
    assert_eq!(
        text(&r.payload.attributes),
        r#"{"crucible.metadata.k":"second"}"#
    );
}

#[test]
fn metadata_keeps_256_keys_and_cuts_kv_items() {
    let mut attributes = vec![s("crucible.metadata.a", &"x".repeat(2_000))];
    attributes.extend((0..300).map(|i| s(&format!("crucible.metadata.k{i:03}"), "v")));
    let r = one(attributes);
    let p = &r.payload;
    assert_eq!(p.metadata_keys.len(), caps::METADATA_MAX_KEYS);
    assert_eq!(p.metadata_values.len(), caps::METADATA_MAX_KEYS);
    assert_eq!(p.metadata_kv.len(), caps::METADATA_MAX_KEYS);
    // The first 256 members in wire order are kept.
    assert_eq!(&*p.metadata_keys[0], "a");
    assert_eq!(p.metadata_values[0].len(), 2_000);
    assert_eq!(p.metadata_kv[0].len(), caps::METADATA_KV_ITEM_BYTES);
    assert_eq!(&*p.metadata_keys[255], "k254");
    // The members beyond the cap stay in the residual attributes.
    let residual: serde_json::Value = serde_json::from_slice(&p.attributes).unwrap();
    let residual = residual.as_object().unwrap();
    assert_eq!(residual.len(), 45);
    assert!(residual.contains_key("crucible.metadata.k255"));
    assert!(residual.contains_key("crucible.metadata.k299"));
}

#[test]
fn the_level_key_sets_the_level() {
    for (name, level) in [
        ("debug", Level::Debug),
        ("default", Level::Default),
        ("warning", Level::Warning),
        ("error", Level::Error),
    ] {
        let r = one(vec![s("crucible.level", name)]);
        assert_eq!(r.core.level, level, "{name}");
        assert_eq!(text(&r.payload.attributes), "");
    }
    // An unknown name does not decode: the status decides, and the attribute stays.
    let r = one(vec![s("crucible.level", "WARN")]);
    assert_eq!(r.core.level, Level::Default);
    assert_eq!(text(&r.payload.attributes), r#"{"crucible.level":"WARN"}"#);
    // An explicit level outranks the status.
    let failing = opentelemetry_proto::tonic::trace::v1::Span {
        status: Some(Status {
            message: String::new(),
            code: 2,
        }),
        ..span_with(1, 1, None, vec![s("crucible.level", "warning")])
    };
    let r = run(&request(vec![failing])).spans.remove(0);
    assert_eq!(r.core.status_code, StatusCode::Error);
    assert_eq!(r.core.level, Level::Warning);
}

#[test]
fn trace_level_keys_fill_tags_metadata_and_the_public_flag() {
    let r = one(vec![
        strings("crucible.trace.tags", &["x", "y"]),
        s("crucible.trace.metadata.tenant", "acme"),
        int("crucible.trace.metadata.shard", 4),
        flag("crucible.trace.public", true),
        s("crucible.metadata.own", "1"),
    ]);
    assert_eq!(strs(&r.core.trace_tags), ["x", "y"]);
    assert_eq!(
        text(&r.payload.trace_metadata),
        r#"{"tenant":"acme","shard":4}"#
    );
    assert_eq!(text(&r.payload.metadata), r#"{"own":"1"}"#);
    assert_eq!(r.core.trace_public, TracePublic::Public);
    assert_eq!(text(&r.payload.attributes), "");
    assert_eq!(
        one(vec![flag("crucible.trace.public", false)])
            .core
            .trace_public,
        TracePublic::Private
    );
    // A boolean decodes, and so do the strings `true` and `false`; nothing else does.
    for (value, public) in [
        ("true", TracePublic::Public),
        ("false", TracePublic::Private),
    ] {
        let r = one(vec![s("crucible.trace.public", value)]);
        assert_eq!(r.core.trace_public, public);
        assert_eq!(text(&r.payload.attributes), "");
    }
    let r = one(vec![s("crucible.trace.public", "True")]);
    assert_eq!(r.core.trace_public, TracePublic::Unset);
    assert_eq!(
        text(&r.payload.attributes),
        r#"{"crucible.trace.public":"True"}"#
    );
}

#[test]
fn input_and_output_keys_fill_spans_without_messages() {
    let r = one(vec![
        s("crucible.input", "plain question"),
        kvlist("crucible.output", vec![s("answer", "42")]),
    ]);
    assert_eq!(text(&r.payload.input), "plain question");
    assert_eq!(text(&r.payload.output), r#"{"answer":"42"}"#);
    assert_eq!(r.core.io_shape, IoShape::Raw);
    assert_eq!(&*r.core.input_preview, "plain question");
    assert_eq!(text(&r.payload.attributes), "");
}

#[test]
fn an_explicit_body_outranks_messages() {
    // The `crucible.*` keys are the first layer (contract 10.3), so an explicit body wins over the message keys;
    // the losing messages stay in the residual.
    let r = one(vec![
        s("crucible.input", "explicit"),
        s("gen_ai.input.messages", r#"[{"role":"user"}]"#),
        s("gen_ai.output.messages", r#"[{"role":"assistant"}]"#),
    ]);
    assert_eq!(text(&r.payload.input), "explicit");
    assert_eq!(r.core.io_shape, IoShape::Raw);
    // The side without an explicit body keeps its messages.
    assert_eq!(text(&r.payload.output), r#"[{"role":"assistant"}]"#);
    assert_eq!(
        text(&r.payload.attributes),
        r#"{"gen_ai.input.messages":"[{\"role\":\"user\"}]"}"#
    );
}

#[test]
fn prompt_reference_keys_fill_name_and_version() {
    let r = one(vec![
        s("gen_ai.prompt.name", "semconv"),
        s("crucible.prompt.name", "greeting"),
        s("crucible.prompt.version", "12"),
    ]);
    assert_eq!(&*r.core.prompt_name, "greeting");
    assert_eq!(&*r.core.prompt_version, "12");
    assert_eq!(
        text(&r.payload.attributes),
        r#"{"gen_ai.prompt.name":"semconv"}"#
    );
    assert_eq!(
        &*one(vec![s("gen_ai.prompt.name", "semconv")])
            .core
            .prompt_name,
        "semconv"
    );
    let long = "9".repeat(100);
    assert_eq!(
        one(vec![s("crucible.prompt.version", &long)])
            .core
            .prompt_version
            .len(),
        caps::PROMPT_VERSION_BYTES
    );
}

#[test]
fn provided_cost_is_read_as_decimal_text_or_a_double() {
    let r = one(vec![
        int("gen_ai.usage.input_tokens", 10),
        int("gen_ai.usage.output_tokens", 5),
        s("crucible.cost.input", "0.000001"),
        double("crucible.cost.output", 0.5),
        s("crucible.cost.total", "0.500001"),
    ]);
    let nanos = |key: &str| {
        r.payload
            .cost_provided
            .iter()
            .find(|(k, _)| &**k == key)
            .map(|(_, c)| c.nanos())
    };
    assert_eq!(nanos("input"), Some(1_000));
    assert_eq!(nanos("output"), Some(500_000_000));
    assert_eq!(nanos("total"), Some(500_001_000));
    assert_eq!(r.core.cost_source, CostSource::Provided);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(500_001_000));
    let cost_of = |key: &str| {
        r.core
            .usage
            .iter()
            .find(|e| &*e.key == key)
            .map(|e| e.cost.nanos())
    };
    assert_eq!(cost_of("input"), Some(1_000));
    assert_eq!(cost_of("output"), Some(500_000_000));
    assert_eq!(text(&r.payload.attributes), "");
}

#[test]
fn provided_cost_without_a_total_sums_its_parts_and_clamps() {
    let r = one(vec![
        s("crucible.cost.input", "0.25"),
        s("crucible.cost.output", "-3"),
        double("crucible.cost.cache_read", f64::NAN),
        int("crucible.cost.reasoning", 2),
        // Half a nano-USD rounds to even.
        s("crucible.cost.a", "0.0000000005"),
        s("crucible.cost.b", "0.0000000015"),
        // Text that is not a decimal does not decode and stays.
        s("crucible.cost.bad", "12 USD"),
        flag("crucible.cost.flag", true),
    ]);
    let costs: Vec<(&str, i64)> = r
        .payload
        .cost_provided
        .iter()
        .map(|(k, c)| (&**k, c.nanos()))
        .collect();
    assert_eq!(
        costs,
        [
            ("a", 0),
            ("b", 2),
            ("cache_read", 0),
            ("input", 250_000_000),
            ("output", 0),
            ("reasoning", 2_000_000_000),
        ]
    );
    assert_eq!(r.core.cost_source, CostSource::Provided);
    assert_eq!(r.core.total_cost_nusd.nanos(), 2_250_000_002);
    assert_eq!(
        text(&r.payload.attributes),
        r#"{"crucible.cost.bad":"12 USD","crucible.cost.flag":true}"#
    );
}

#[test]
fn completion_start_time_is_an_rfc_3339_string_or_unix_nanoseconds() {
    let expected = Timestamp::from_micros((BASE_NANOS / 1_000) as i64 + 125_000);
    let r = one(vec![s(
        "crucible.completion_start_time",
        "2026-11-17T09:30:00.125Z",
    )]);
    assert_eq!(r.core.completion_start_time, Some(expected));
    assert_eq!(text(&r.payload.attributes), "");
    // Nanoseconds truncate to microseconds.
    let nanos = i64::try_from(BASE_NANOS).unwrap() + 125_000_999;
    let r = one(vec![int("crucible.completion_start_time", nanos)]);
    assert_eq!(r.core.completion_start_time, Some(expected));
    // Other text, a negative integer and an instant outside the valid range do not decode.
    for bad in [
        s("crucible.completion_start_time", "yesterday"),
        int("crucible.completion_start_time", -1),
        s("crucible.completion_start_time", "2200-01-01T00:00:00Z"),
        double("crucible.completion_start_time", 1.5),
    ] {
        let r = one(vec![bad]);
        assert_eq!(r.core.completion_start_time, None);
        assert!(text(&r.payload.attributes).contains("crucible.completion_start_time"));
    }
}

#[test]
fn model_parameters_come_from_three_sources_in_priority_order() {
    let r = one(vec![
        s("gen_ai.request.model", "gpt-4o"),
        double("gen_ai.request.temperature", 0.5),
        int("gen_ai.request.max_tokens", 256),
        strings("gen_ai.request.stop_sequences", &["END"]),
        int("crucible.model_parameters.max_tokens", 100),
        s("crucible.model_parameters.service_tier", "flex"),
        s(service_tier_key(), "priority"),
    ]);
    // The model stays `model_requested`; the explicit keys win per member; members in source order, then wire
    // order.
    assert_eq!(&*r.core.model_requested, "gpt-4o");
    assert_eq!(
        text(&r.payload.model_parameters),
        r#"{"max_tokens":100,"service_tier":"flex","temperature":0.5,"stop_sequences":["END"]}"#
    );
    assert_eq!(
        text(&r.payload.attributes),
        format!(
            r#"{{"gen_ai.request.max_tokens":256,"{}":"priority"}}"#,
            service_tier_key()
        )
    );
    // Without the explicit member, the service-tier key supplies it.
    let r = one(vec![
        s(service_tier_key(), "priority"),
        double("gen_ai.request.top_p", 0.9),
    ]);
    assert_eq!(
        text(&r.payload.model_parameters),
        r#"{"top_p":0.9,"service_tier":"priority"}"#
    );
    assert_eq!(text(&r.payload.attributes), "");
    // A model key that does not decode as a model is still not a parameter.
    let r = one(vec![int("gen_ai.request.model", 4)]);
    assert!(r.payload.model_parameters.is_empty());
    assert_eq!(text(&r.payload.attributes), r#"{"gen_ai.request.model":4}"#);
}

#[test]
fn trace_level_values_from_native_keys_forward_fill_within_the_request() {
    let root = span_with(
        1,
        1,
        None,
        vec![
            s("crucible.trace.name", "checkout"),
            s("crucible.session.id", "s1"),
            s("crucible.user.id", "u1"),
            s("crucible.environment", "staging"),
            s("crucible.release", "r9"),
            s("crucible.version", "v2"),
            strings("crucible.trace.tags", &["vip"]),
            s("crucible.trace.metadata.tenant", "acme"),
            flag("crucible.trace.public", true),
            // Not trace-level: stays on its own span.
            strings("crucible.tags", &["own"]),
            s("crucible.metadata.k", "v"),
            s("crucible.level", "debug"),
        ],
    );
    let body = request(vec![span(1, 2, Some(1)), root, span(2, 3, None)]);
    let out = run(&body);
    let (child, root, other) = (&out.spans[0], &out.spans[1], &out.spans[2]);
    for r in [child, root] {
        let c = &r.core;
        assert_eq!(
            (
                &*c.trace_name,
                &*c.session_id,
                &*c.user_id,
                &*c.environment,
                &*c.release,
                &*c.version
            ),
            ("checkout", "s1", "u1", "staging", "r9", "v2")
        );
        assert_eq!(strs(&c.trace_tags), ["vip"]);
        assert_eq!(text(&r.payload.trace_metadata), r#"{"tenant":"acme"}"#);
        assert_eq!(c.trace_public, TracePublic::Public);
    }
    assert!(child.core.tags.is_empty() && child.payload.metadata.is_empty());
    assert_eq!(child.core.level, Level::Default);
    assert_eq!(strs(&root.core.tags), ["own"]);
    assert_eq!(root.core.level, Level::Debug);
    // Another trace of the request takes nothing.
    assert_eq!(&*other.core.trace_name, "");
    assert_eq!(&*other.core.environment, "default");
    assert!(other.payload.trace_metadata.is_empty());
    assert_eq!(other.core.trace_public, TracePublic::Unset);
}

/// One request that carries every key of contract 10.6.
fn every_key() -> opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest {
    request_with(
        vec![
            s("service.name", "svc"),
            s("service.version", "1.4.2"),
            s("deployment.environment.name", "prod"),
        ],
        vec![
            span_with(
                1,
                1,
                None,
                vec![
                    s("crucible.trace.name", "checkout"),
                    s("crucible.session.id", "s1"),
                    s("crucible.user.id", "u1"),
                    s("crucible.version", "v2"),
                    strings("crucible.tags", &["a", "b"]),
                    s("crucible.metadata.plan", "pro"),
                    double("crucible.metadata.ratio", 0.25),
                    s("crucible.level", "warning"),
                    strings("crucible.trace.tags", &["vip"]),
                    int("crucible.trace.metadata.shard", 4),
                    flag("crucible.trace.public", false),
                    s("crucible.input", r#"{"q":"hi"}"#),
                    s("crucible.output", "hello"),
                    s("crucible.prompt.name", "greeting"),
                    s("crucible.prompt.version", "3"),
                    s("crucible.cost.total", "0.0125"),
                    double("crucible.cost.input", 0.01),
                    s("crucible.completion_start_time", "2026-11-17T09:30:00.125Z"),
                    double("crucible.model_parameters.temperature", 0.5),
                    int("gen_ai.request.max_tokens", 64),
                    s(service_tier_key(), "flex"),
                ],
            ),
            span(1, 2, Some(1)),
        ],
    )
}

#[test]
fn protobuf_and_json_agree_on_every_source_key() {
    let request = every_key();
    let proto = request.encode_to_vec();
    let json = transcode_json(&serde_json::to_vec(&request).unwrap()).unwrap();
    let ctx = ctx();
    let mut from_proto = normalize(&frame(&proto, &[]), &ctx).unwrap();
    let mut from_json = normalize(&frame(&json, &[]), &ctx).unwrap();
    // `event_bytes` is the span's size in its own canonical body (see `proto_json.rs`).
    for r in from_proto.spans.iter_mut().chain(&mut from_json.spans) {
        r.core.event_bytes = 0;
    }
    assert_eq!(from_proto, from_json);
    let root = &from_proto.spans[0];
    assert_eq!(&*root.core.release, "1.4.2");
    assert_eq!(&*root.core.environment, "prod");
    assert_eq!(root.core.level, Level::Warning);
    assert_eq!(root.core.trace_public, TracePublic::Private);
    assert_eq!(root.core.total_cost_nusd.nanos(), 12_500_000);
    assert_eq!(
        text(&root.payload.model_parameters),
        r#"{"temperature":0.5,"max_tokens":64,"service_tier":"flex"}"#
    );
    // Every key was consumed, and the child took the trace-level values.
    assert_eq!(text(&root.payload.attributes), "");
    assert_eq!(text(&root.payload.resource_attributes), "");
    assert_eq!(&*from_proto.spans[1].core.session_id, "s1");
    // Mapping is idempotent: the same frame gives the same records.
    let mut again = normalize(&frame(&proto, &[]), &ctx).unwrap();
    again.spans.iter_mut().for_each(|r| r.core.event_bytes = 0);
    assert_eq!(again, from_proto);
}

fn leaf() -> impl Strategy<Value = any_value::Value> {
    use any_value::Value as V;
    prop_oneof![
        "[ -~]{0,12}".prop_map(V::StringValue),
        any::<bool>().prop_map(V::BoolValue),
        any::<i64>().prop_map(V::IntValue),
        any::<u64>().prop_map(|bits| V::DoubleValue(f64::from_bits(bits))),
        vec(any::<u8>(), 0..4).prop_map(V::BytesValue),
    ]
}

/// The exact keys of contract 10.6 that dialect 0 reads on a span.
const EXACT_KEYS: [&str; 17] = [
    "crucible.trace.name",
    "crucible.session.id",
    "crucible.user.id",
    "crucible.environment",
    "crucible.release",
    "crucible.version",
    "crucible.tags",
    "crucible.level",
    "crucible.trace.tags",
    "crucible.trace.public",
    "crucible.input",
    "crucible.output",
    "crucible.prompt.name",
    "crucible.prompt.version",
    "crucible.cost.total",
    "crucible.completion_start_time",
    "crucible.model_parameters.temperature",
];

proptest! {
    /// Any value under any source key maps without a panic, and each attribute is either consumed or kept in the
    /// residual object, never both lost: a key absent from the residual filled its field.
    #[test]
    fn any_value_under_a_source_key_maps_or_stays_residual(
        pairs in vec((proptest::sample::select(EXACT_KEYS.to_vec()), leaf()), 0..6),
    ) {
        let attributes: Vec<KeyValue> = pairs.iter().map(|(k, v)| kv(k, v.clone())).collect();
        let r = one(attributes);
        let residual: Vec<(String, serde_json::Value)> = if r.payload.attributes.is_empty() {
            Vec::new()
        } else {
            let object: serde_json::Value = serde_json::from_slice(&r.payload.attributes).unwrap();
            object.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        let (c, p) = (&r.core, &r.payload);
        for (key, _) in &pairs {
            let kept = residual.iter().any(|(k, _)| k == key);
            let filled = match *key {
                "crucible.trace.name" => !c.trace_name.is_empty(),
                "crucible.session.id" => !c.session_id.is_empty(),
                "crucible.user.id" => !c.user_id.is_empty(),
                "crucible.environment" => &*c.environment != "default",
                "crucible.release" => !c.release.is_empty(),
                "crucible.version" => !c.version.is_empty(),
                "crucible.tags" => !c.tags.is_empty(),
                "crucible.level" => true,
                "crucible.trace.tags" => !c.trace_tags.is_empty(),
                "crucible.trace.public" => c.trace_public != TracePublic::Unset,
                "crucible.input" => !p.input.is_empty(),
                "crucible.output" => !p.output.is_empty(),
                "crucible.prompt.name" => !c.prompt_name.is_empty(),
                "crucible.prompt.version" => !c.prompt_version.is_empty(),
                "crucible.cost.total" => !p.cost_provided.is_empty(),
                "crucible.completion_start_time" => c.completion_start_time.is_some(),
                _ => !p.model_parameters.is_empty(),
            };
            prop_assert!(kept || filled, "{key} was neither kept nor mapped");
        }
    }

    /// String metadata round-trips: the stored object, the key and value lists and the `key=value` items all
    /// hold exactly the members sent, in wire order.
    #[test]
    fn string_metadata_round_trips(
        members in btree_map("[a-z][a-z0-9_.]{0,8}", "[ -~]{1,20}", 0..12),
        reversed in any::<bool>(),
    ) {
        let mut attributes: Vec<KeyValue> = members
            .iter()
            .map(|(k, v)| s(&format!("crucible.metadata.{k}"), v))
            .collect();
        if reversed {
            attributes.reverse();
        }
        let r = one(attributes);
        let p = &r.payload;
        let mut members: Vec<(String, String)> = members.into_iter().collect();
        if reversed {
            members.reverse();
        }
        let keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
        let values: Vec<&str> = members.iter().map(|(_, v)| v.as_str()).collect();
        prop_assert_eq!(strs(&p.metadata_keys), keys);
        prop_assert_eq!(strs(&p.metadata_values), values);
        let kv_items: Vec<String> = members.iter().map(|(k, v)| format!("{k}={v}")).collect();
        prop_assert_eq!(strs(&p.metadata_kv), kv_items.iter().map(String::as_str).collect::<Vec<_>>());
        if members.is_empty() {
            prop_assert!(p.metadata.is_empty());
        } else {
            let object: serde_json::Value = serde_json::from_slice(&p.metadata).unwrap();
            let mut stored: Vec<(String, String)> = object
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect();
            stored.sort();
            members.sort();
            prop_assert_eq!(stored, members);
        }
        prop_assert!(p.attributes.is_empty());
    }
}
