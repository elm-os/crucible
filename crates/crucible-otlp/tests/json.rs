//! OTLP/JSON transcoding against prost and its serde rendering as the oracle.

mod common;

use common::{generate, oracle};
use crucible_otlp::{JsonError, MAX_BODY_BYTES, TraceRequest, transcode_json};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message;

const TRACE_ID: &str = "5b8efff798038103d269b633813fc60c";
const SPAN_ID: &str = "eee19b7ec3c1b174";

fn with_spans(spans: &str) -> String {
    format!(r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[{spans}]}}]}}]}}"#)
}

fn with_value(value: &str) -> String {
    with_spans(&format!(
        r#"{{"traceId":"{TRACE_ID}","spanId":"{SPAN_ID}","attributes":[{{"key":"k","value":{value}}}]}}"#
    ))
}

/// Transcodes and scans `json`, returning every span as the views show it.
fn spans(json: &str) -> Result<Vec<oracle::Span>, String> {
    let bytes = transcode_json(json.as_bytes()).map_err(|e| e.to_string())?;
    let scanned = TraceRequest::scan(&bytes, None).map_err(|e| e.to_string())?;
    Ok(oracle::observe(&scanned))
}

/// The value of attribute `k` of the only span.
fn value(json_value: &str) -> Result<oracle::Val, String> {
    let spans = spans(&with_value(json_value))?;
    let attribute = spans.first().and_then(|s| s.attributes.first()).cloned();
    attribute
        .map(|(_, v)| v)
        .ok_or_else(|| "no attribute".to_owned())
}

/// Whatever transcodes is valid protobuf that prost decodes, and a successful scan of it reads
/// exactly as prost decodes it.
fn output_is_valid(json: &[u8]) -> Result<(), TestCaseError> {
    let Ok(bytes) = transcode_json(json) else {
        return Ok(());
    };
    let decoded = ExportTraceServiceRequest::decode(bytes.as_slice());
    let Ok(decoded) = decoded else {
        return Err(TestCaseError::fail("transcoded bytes that prost refuses"));
    };
    if let Ok(scanned) = TraceRequest::scan(&bytes, None) {
        prop_assert_eq!(oracle::observe(&scanned), oracle::expect(&decoded, None));
    }
    Ok(())
}

proptest! {
    /// The OTLP/JSON rendering of a random request transcodes to protobuf that decodes to the same
    /// request and scans to the same values as prost's own encoding.
    #[test]
    fn proto_equals_json(request in generate::request(true)) {
        let json = serde_json::to_vec(&request).unwrap();
        let bytes = transcode_json(&json).unwrap();
        prop_assert_eq!(&ExportTraceServiceRequest::decode(bytes.as_slice()).unwrap(), &request);
        let from_json = oracle::observe(&TraceRequest::scan(&bytes, None).unwrap());
        let native = request.encode_to_vec();
        let from_proto = oracle::observe(&TraceRequest::scan(&native, None).unwrap());
        prop_assert_eq!(&from_json, &from_proto);
        prop_assert_eq!(from_json, oracle::expect(&request, None));
    }

    /// Arbitrary bytes never panic the transcoder.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in vec(any::<u8>(), 0..256)) {
        output_is_valid(&bytes)?;
    }

    /// Rendered requests with bytes overwritten, inserted or removed never panic the transcoder,
    /// and whatever still transcodes is valid protobuf.
    #[test]
    fn mutated_json_never_panics(
        request in generate::request(true),
        edits in vec((any::<prop::sample::Index>(), any::<u8>(), 0..3u8), 1..5),
    ) {
        let mut json = serde_json::to_vec(&request).unwrap();
        for (at, byte, op) in edits {
            if json.is_empty() {
                json.push(byte);
                continue;
            }
            let i = at.index(json.len());
            match op {
                0 => json[i] = byte,
                1 => json.insert(i, byte),
                _ => json.truncate(i),
            }
        }
        output_is_valid(&json)?;
    }
}

#[test]
fn ids_are_hex_in_either_case() {
    let upper = with_spans(&format!(
        r#"{{"traceId":"{}","spanId":"{}","parentSpanId":"{}"}}"#,
        TRACE_ID.to_uppercase(),
        SPAN_ID.to_uppercase(),
        SPAN_ID
    ));
    let span = &spans(&upper).unwrap()[0];
    assert_eq!(span.trace_id.map(|b| format!("{b:02x}")).concat(), TRACE_ID);
    assert_eq!(span.span_id.map(|b| format!("{b:02x}")).concat(), SPAN_ID);
    assert_eq!(span.parent_span_id, Some(span.span_id));
    // An empty id is absent, so the span is rejected through partial success, not refused.
    let empty = with_spans(r#"{"traceId":"","spanId":""}"#);
    let bytes = transcode_json(empty.as_bytes()).unwrap();
    let scanned = TraceRequest::scan(&bytes, None).unwrap();
    assert_eq!(scanned.partial_success().unwrap().rejected_spans(), 1);
    for bad in ["abc", "zz", "0x5b8e"] {
        let json = with_spans(&format!(r#"{{"traceId":"{bad}"}}"#));
        assert!(matches!(
            transcode_json(json.as_bytes()),
            Err(JsonError::Invalid(_))
        ));
    }
    // A well-formed hex string of the wrong length transcodes, and the scan refuses it.
    let short = with_spans(r#"{"traceId":"abcd"}"#);
    let bytes = transcode_json(short.as_bytes()).unwrap();
    assert!(TraceRequest::scan(&bytes, None).is_err());
}

#[test]
fn integers_are_numbers_or_decimal_strings() {
    assert_eq!(value(r#"{"intValue":"-42"}"#), Ok(oracle::Val::Int(-42)));
    assert_eq!(value(r#"{"intValue":-42}"#), Ok(oracle::Val::Int(-42)));
    assert_eq!(
        value(r#"{"intValue":"9223372036854775807"}"#),
        Ok(oracle::Val::Int(i64::MAX))
    );
    let times = with_spans(&format!(
        r#"{{"traceId":"{TRACE_ID}","spanId":"{SPAN_ID}","startTimeUnixNano":"1794907800000123456","endTimeUnixNano":1794907800000999999}}"#
    ));
    let span = &spans(&times).unwrap()[0];
    assert_eq!(span.start_us, 1_794_907_800_000_123);
    assert_eq!(span.end_us, 1_794_907_800_000_999);
    for bad in [
        r#"{"intValue":"9223372036854775808"}"#,
        r#"{"intValue":1.5}"#,
        r#"{"intValue":"12a"}"#,
        r#"{"intValue":true}"#,
    ] {
        assert!(value(bad).is_err(), "{bad}");
    }
    let negative_time = with_spans(r#"{"startTimeUnixNano":"-1"}"#);
    assert!(transcode_json(negative_time.as_bytes()).is_err());
    let wide_count = with_spans(r#"{"droppedAttributesCount":4294967296}"#);
    assert!(transcode_json(wide_count.as_bytes()).is_err());
}

#[test]
fn enums_are_integers() {
    let span = |kind: &str| {
        with_spans(&format!(
            r#"{{"traceId":"{TRACE_ID}","spanId":"{SPAN_ID}","kind":{kind},"status":{{"code":2}}}}"#
        ))
    };
    let parsed = &spans(&span("3")).unwrap()[0];
    assert_eq!((parsed.kind, parsed.status_code), (3, 2));
    // Out-of-range values transcode faithfully and read as unspecified.
    assert_eq!(spans(&span("-1")).unwrap()[0].kind, 0);
    assert!(transcode_json(span(r#""SPAN_KIND_SERVER""#).as_bytes()).is_err());
    assert!(transcode_json(span("2147483648").as_bytes()).is_err());
}

#[test]
fn bytes_are_base64_with_either_alphabet() {
    let bytes = |b: &[u8]| Ok(oracle::Val::Bytes(b.to_vec()));
    assert_eq!(value(r#"{"bytesValue":"aGk="}"#), bytes(b"hi"));
    assert_eq!(value(r#"{"bytesValue":"aGk"}"#), bytes(b"hi"));
    assert_eq!(value(r#"{"bytesValue":"+/8="}"#), bytes(&[0xfb, 0xff]));
    assert_eq!(value(r#"{"bytesValue":"-_8"}"#), bytes(&[0xfb, 0xff]));
    assert_eq!(value(r#"{"bytesValue":""}"#), bytes(b""));
    assert!(value(r#"{"bytesValue":"a*k="}"#).is_err());
}

#[test]
fn doubles_include_the_special_strings() {
    let double = |d: f64| Ok(oracle::Val::Double(d.to_bits()));
    assert_eq!(value(r#"{"doubleValue":1.5}"#), double(1.5));
    assert_eq!(value(r#"{"doubleValue":3}"#), double(3.0));
    assert_eq!(value(r#"{"doubleValue":-0.0}"#), double(-0.0));
    assert_eq!(value(r#"{"doubleValue":"2.5"}"#), double(2.5));
    assert_eq!(
        value(r#"{"doubleValue":"Infinity"}"#),
        double(f64::INFINITY)
    );
    assert_eq!(
        value(r#"{"doubleValue":"-Infinity"}"#),
        double(f64::NEG_INFINITY)
    );
    let Ok(oracle::Val::Double(bits)) = value(r#"{"doubleValue":"NaN"}"#) else {
        panic!("NaN did not transcode");
    };
    assert!(f64::from_bits(bits).is_nan());
    // The shortest decimal that rounds to a double transcodes to exactly that double.
    assert_eq!(
        value(r#"{"doubleValue":2.2250738585072014e-308}"#),
        double(f64::MIN_POSITIVE)
    );
}

#[test]
fn nulls_are_absent_and_unknown_keys_are_ignored() {
    let json = with_spans(&format!(
        r#"{{"traceId":"{TRACE_ID}","spanId":"{SPAN_ID}","name":null,"status":null,"events":null,
            "unknownKey":{{"nested":[1,2,{{"x":null}}]}},"attributes":[{{"key":"k","value":null}},
            {{"key":"e","value":{{}}}},{{"key":"u","value":{{"futureValue":7}}}}]}}"#
    ));
    let span = &spans(&json).unwrap()[0];
    assert_eq!(span.name, "");
    assert_eq!((span.status_code, span.status_message.as_str()), (0, ""));
    let empty = oracle::Val::Empty;
    assert_eq!(
        span.attributes,
        [
            ("k".to_owned(), empty.clone()),
            ("e".to_owned(), empty.clone()),
            ("u".to_owned(), empty)
        ]
    );
    // An empty object is an empty request.
    assert_eq!(transcode_json(b"{}").unwrap(), Vec::<u8>::new());
    assert_eq!(
        transcode_json(b" {\"resourceSpans\":[]} ").unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn malformed_json_is_an_error() {
    for bad in [
        "",
        "[]",
        "null",
        "\"x\"",
        "{",
        "{} {}",
        r#"{"resourceSpans":{}}"#,
        r#"{"resourceSpans":[1]}"#,
        r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"name":5}]}]}]}"#,
        r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"attributes":[{"value":{"boolValue":"t"}}]}]}]}]}"#,
        r#"{"resourceSpans":[{"resource":[]}]}"#,
    ] {
        assert!(
            matches!(transcode_json(bad.as_bytes()), Err(JsonError::Invalid(_))),
            "{bad}"
        );
    }
    // Invalid UTF-8 inside a string.
    let mut bytes = br#"{"resourceSpans":[{"schemaUrl":"#.to_vec();
    bytes.extend_from_slice(&[b'"', 0xff, b'"', b'}', b']', b'}']);
    assert!(transcode_json(&bytes).is_err());
}

#[test]
fn the_canonical_cap_applies_to_the_output() {
    let big = "a".repeat(MAX_BODY_BYTES);
    let json = format!(r#"{{"resourceSpans":[{{"schemaUrl":"{big}"}}]}}"#);
    match transcode_json(json.as_bytes()) {
        Err(JsonError::BodyTooLarge { len }) => assert!(len > MAX_BODY_BYTES),
        other => panic!("expected BodyTooLarge, got {other:?}"),
    }
}

#[test]
fn the_deepest_json_stays_within_the_scan_nesting_limit() {
    // Key-value lists nest four JSON containers per three protobuf messages, the worst ratio, and
    // arrays three per two. Every depth the JSON parser accepts must also scan.
    for (open, close) in [
        (r#"{"kvlistValue":{"values":[{"key":"k","value":"#, "}]}}"),
        (r#"{"arrayValue":{"values":["#, "]}}"),
    ] {
        let mut accepted = 0;
        for depth in 0..60 {
            let value = format!(
                "{}{}{}",
                open.repeat(depth),
                r#"{"stringValue":"leaf"}"#,
                close.repeat(depth)
            );
            let Ok(bytes) = transcode_json(with_value(&value).as_bytes()) else {
                break;
            };
            assert!(
                TraceRequest::scan(&bytes, None).is_ok(),
                "depth {depth} transcodes but does not scan"
            );
            accepted = depth;
        }
        assert!(
            accepted > 20 && accepted < 59,
            "parser depth limit not reached"
        );
    }
}
