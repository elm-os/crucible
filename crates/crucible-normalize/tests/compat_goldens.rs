//! The compat goldens (contract 10): every fixture of `crucible-compat` maps to the fields it expects.
//!
//! A fixture is one OTLP/JSON export request with the request path and headers it arrives with, and the fields
//! each of its spans must get. Its values are invented; its shape is the one an SDK sends. For each fixture:
//!
//! 1. the request is encoded as protobuf, and the JSON text is transcoded to canonical protobuf as ingest does;
//! 2. both encodings are normalized, and give the same records (`event_bytes`, the size of a span in its own
//!    canonical body, is the one field that may differ);
//! 3. every expected field of every span holds, on the finished records (after the request-local forward-fill,
//!    usage, cost and caps);
//! 4. the residual span attributes are exactly the expected keys;
//! 5. normalizing again gives the same records.
//!
//! The dialect hint and the SDK identity of the frame come from the request as contract 12.1 derives them: the
//! route of the path (or a routing header) gives the hint, and a compat route's SDK headers give `sdk_name` and
//! `sdk_version`. No key, path or header of a dialect is named here: they come from the fixtures and from
//! `crucible_compat::ROUTES`.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

mod common;

use std::collections::BTreeMap;

use common::{ctx, frame};
use crucible_compat::{FIXTURES, Fixture, ROUTES, TABLES};
use crucible_normalize::{NormalizedFrame, normalize};
use crucible_otlp::transcode_json;
use crucible_records::{SpanRecord, Timestamp};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use serde_json::Value;

/// One fixture, ready to normalize.
struct Case {
    id: String,
    hint: u8,
    sdk_name: String,
    sdk_version: String,
    proto: Vec<u8>,
    json: Vec<u8>,
    expect: serde_json::Map<String, Value>,
}

fn case(fixture: &Fixture) -> Case {
    let id = format!("d{}/{}", fixture.dialect, fixture.id);
    let doc: Value = serde_json::from_str(fixture.json).unwrap_or_else(|e| panic!("{id}: {e}"));
    let path = doc["request"]["path"].as_str().expect("a request path");
    let headers = doc["request"]["headers"].as_object().expect("headers");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.as_str())
    };
    let on_route = ROUTES.iter().find(|r| r.path == path);
    let by_header = ROUTES
        .iter()
        .find(|r| r.hint_header.is_some_and(|h| header(h).is_some()));
    let hint = on_route.or(by_header).map_or(0, |r| r.dialect);
    let of = |name: Option<&str>| name.and_then(header).unwrap_or_default().to_owned();
    let (sdk_name, sdk_version) = on_route.map_or((String::new(), String::new()), |r| {
        (of(r.sdk_name_header), of(r.sdk_version_header))
    });
    let otlp = serde_json::to_vec(&doc["otlp"]).unwrap();
    let request: ExportTraceServiceRequest =
        serde_json::from_slice(&otlp).unwrap_or_else(|e| panic!("{id}: {e}"));
    Case {
        hint,
        sdk_name,
        sdk_version,
        proto: request.encode_to_vec(),
        json: transcode_json(&otlp).unwrap_or_else(|e| panic!("{id}: {e:?}")),
        expect: doc["expect"].as_object().expect("expect").clone(),
        id,
    }
}

fn run(case: &Case, body: &[u8]) -> NormalizedFrame {
    let mut f = frame(body, &[]);
    f.compat_dialect = case.hint;
    f.sdk_name = &case.sdk_name;
    f.sdk_version = &case.sdk_version;
    let mut out = normalize(&f, &ctx()).unwrap();
    for span in &mut out.spans {
        span.core.event_bytes = 0;
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap()
}

/// A stored JSON field as a value; `null` when the field is absent.
fn stored(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(bytes).unwrap()
    }
}

fn strings(items: &[Box<str>]) -> Value {
    Value::Array(items.iter().map(|s| Value::from(&**s)).collect())
}

/// The keys of a residual attribute object, sorted.
fn keys(bytes: &[u8]) -> Vec<String> {
    match stored(bytes) {
        Value::Null => Vec::new(),
        Value::Object(members) => members.keys().cloned().collect(),
        other => panic!("residual attributes are not an object: {other}"),
    }
}

fn sorted(expected: &Value) -> Vec<String> {
    let mut keys: Vec<String> = expected
        .as_array()
        .expect("a list of keys")
        .iter()
        .map(|k| k.as_str().expect("a key").to_owned())
        .collect();
    keys.sort();
    keys
}

/// What the record holds for one expected field, in the form the fixture states it.
fn actual(field: &str, expected: &Value, r: &SpanRecord) -> Value {
    let (c, p) = (&r.core, &r.payload);
    match field {
        "compat_dialect" => c.compat_dialect.into(),
        "source_format" => c.source_format.as_str().into(),
        "observation_type" => c.observation_type.as_str().into(),
        "level" => c.level.as_str().into(),
        "status_message" => (&*c.status_message).into(),
        "is_root" => c.is_root.into(),
        "is_app_root" => c.is_app_root.into(),
        "usage_aggregate" => c.usage_aggregate.into(),
        "model_requested" => (&*c.model_requested).into(),
        "model_response" => (&*c.model_response).into(),
        "provider" => (&*c.provider).into(),
        "prompt_name" => (&*c.prompt_name).into(),
        "prompt_version" => (&*c.prompt_version).into(),
        "trace_name" => (&*c.trace_name).into(),
        "user_id" => (&*c.user_id).into(),
        "session_id" => (&*c.session_id).into(),
        "version" => (&*c.version).into(),
        "environment" => (&*c.environment).into(),
        "release" => (&*c.release).into(),
        "service_name" => (&*c.service_name).into(),
        "telemetry_sdk_name" => (&*c.telemetry_sdk_name).into(),
        "telemetry_sdk_language" => (&*c.telemetry_sdk_language).into(),
        "sdk_name" => (&*c.sdk_name).into(),
        "sdk_version" => (&*c.sdk_version).into(),
        "input_preview" => (&*c.input_preview).into(),
        "trace_tags" => strings(&c.trace_tags),
        "tags" => strings(&c.tags),
        "tool_names" => strings(&c.tool_names),
        "finish_reasons" => strings(&c.finish_reasons),
        "trace_public" => c.trace_public.as_str().into(),
        "io_shape" => c.io_shape.as_str().into(),
        "cost_source" => c.cost_source.as_str().into(),
        "total_tokens" => c.total_tokens.into(),
        "total_cost_nusd" => c.total_cost_nusd.nanos().into(),
        "tool_call_count" => c.tool_call_count.into(),
        // Bodies verbatim, or compared as JSON.
        "input" => text(&p.input).into(),
        "output" => text(&p.output).into(),
        "input_json" => stored(&p.input),
        "output_json" => stored(&p.output),
        "metadata" => stored(&p.metadata),
        "trace_metadata" => stored(&p.trace_metadata),
        "model_parameters" => stored(&p.model_parameters),
        "tool_calls" => stored(&p.tool_calls),
        "tool_definitions" => stored(&p.tool_definitions),
        "system_instructions" => stored(&p.system_instructions),
        "events" => stored(&p.events).as_array().map_or(0, Vec::len).into(),
        "usage_provided" => Value::Object(
            p.usage_provided
                .iter()
                .map(|(k, v)| (k.to_string(), Value::from(*v)))
                .collect(),
        ),
        "cost_provided" => Value::Object(
            p.cost_provided
                .iter()
                .map(|(k, v)| (k.to_string(), Value::from(v.nanos())))
                .collect(),
        ),
        "completion_start_time" => {
            // Compared as instants: the fixture states RFC 3339 text.
            let stated = Timestamp::parse_rfc3339(expected.as_str().expect("a time")).unwrap();
            if c.completion_start_time == Some(stated) {
                expected.clone()
            } else {
                format!("{:?}", c.completion_start_time).into()
            }
        }
        "completion_start_offset_us" => c
            .completion_start_time
            .map(|t| t.micros() - c.start_time.micros())
            .into(),
        "residual" => keys(&p.attributes).into(),
        "resource_residual" => keys(&p.resource_attributes).into(),
        other => panic!("the runner does not know the expected field `{other}`"),
    }
}

#[test]
fn every_fixture_maps_to_its_expected_fields() {
    let mut spans = 0;
    for fixture in FIXTURES {
        let case = case(fixture);
        let from_proto = run(&case, &case.proto);
        let from_json = run(&case, &case.json);
        assert_eq!(
            from_proto, from_json,
            "{}: protobuf and JSON differ",
            case.id
        );
        assert_eq!(
            from_proto,
            run(&case, &case.proto),
            "{}: not idempotent",
            case.id
        );
        assert_eq!(
            from_proto.spans.len(),
            case.expect.len(),
            "{}: every span has expectations",
            case.id
        );
        let by_id: BTreeMap<String, &SpanRecord> = from_proto
            .spans
            .iter()
            .map(|r| (hex(r.core.span_id.as_bytes()), r))
            .collect();
        for (span_id, fields) in &case.expect {
            let context = format!("{} span {span_id}", case.id);
            let record = by_id
                .get(span_id)
                .unwrap_or_else(|| panic!("{context}: no record"));
            let fields = fields.as_object().expect("fields");
            assert!(
                fields.contains_key("residual"),
                "{context}: the residual keys are always stated"
            );
            for (field, expected) in fields {
                let expected_value = match field.as_str() {
                    "residual" | "resource_residual" => sorted(expected).into(),
                    _ => expected.clone(),
                };
                assert_eq!(
                    actual(field, expected, record),
                    expected_value,
                    "{context}: {field}"
                );
            }
            spans += 1;
        }
    }
    assert!(spans >= FIXTURES.len());
}

/// The fixture set is what the tables were written against: every dialect with a table of its own has at least
/// three fixtures, ids are unique, every fixture states that its values are invented and where its shape is
/// documented, and its directory names a registered dialect.
#[test]
fn the_fixture_set_covers_every_dialect_with_invented_values() {
    for (i, fixture) in FIXTURES.iter().enumerate() {
        assert!(usize::from(fixture.dialect) < TABLES.len() && fixture.dialect != 0);
        assert!(
            FIXTURES[..i]
                .iter()
                .all(|f| (f.dialect, f.id) != (fixture.dialect, fixture.id)),
            "{} is listed twice",
            fixture.id
        );
        let doc: Value = serde_json::from_str(fixture.json).unwrap();
        let provenance = &doc["provenance"];
        assert_eq!(provenance["values"], "invented", "{}", fixture.id);
        assert!(provenance["shape"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(provenance["license_of_the_shape_source"].is_string());
    }
    for dialect in 1..TABLES.len() as u8 {
        let count = FIXTURES.iter().filter(|f| f.dialect == dialect).count();
        assert!(count >= 3, "dialect {dialect} has {count} fixtures");
    }
}

/// Every fixture's first chosen dialect is reachable: the set as a whole exercises each dialect's detection,
/// including spans that stay native on a plain path.
#[test]
fn the_fixtures_choose_every_dialect() {
    let mut chosen = std::collections::BTreeSet::new();
    for fixture in FIXTURES {
        let case = case(fixture);
        chosen.extend(
            run(&case, &case.proto)
                .spans
                .iter()
                .map(|r| r.core.compat_dialect),
        );
    }
    assert_eq!(chosen.len(), TABLES.len(), "{chosen:?}");
}
