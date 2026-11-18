//! The scanner against prost as the oracle, plus the contract's caps and id rules.

mod common;

use common::{generate, oracle};
use crucible_otlp::{
    AnyValueView, MAX_BODY_BYTES, MAX_SPANS, Rejection, ScanError, TraceRequest, WireError,
};
use crucible_records::Timestamp;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::{Span, Status};
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message;

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A length-delimited field.
fn field(number: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(u64::from((number << 3) | 2), &mut out);
    varint(payload.len() as u64, &mut out);
    out.extend_from_slice(payload);
    out
}

/// One request holding the given encoded spans in one scope.
fn request_of(spans: &[Vec<u8>]) -> Vec<u8> {
    let scope: Vec<u8> = spans.iter().flat_map(|s| field(2, s)).collect();
    field(1, &field(2, &scope))
}

fn valid_span() -> Span {
    Span {
        trace_id: vec![1; 16],
        span_id: vec![2; 8],
        name: "work".to_owned(),
        ..Span::default()
    }
}

fn malformed(bytes: &[u8]) -> Option<WireError> {
    match TraceRequest::scan(bytes, None) {
        Err(ScanError::Malformed(e)) => Some(e),
        _ => None,
    }
}

/// The scan refuses whatever prost refuses, and what it accepts reads exactly as prost decodes it.
fn agrees_with_prost(bytes: &[u8]) -> Result<(), TestCaseError> {
    let decoded = ExportTraceServiceRequest::decode(bytes);
    if let Ok(scanned) = TraceRequest::scan(bytes, None) {
        let Ok(decoded) = decoded else {
            return Err(TestCaseError::fail(
                "the scan accepted a body prost refuses",
            ));
        };
        prop_assert_eq!(oracle::observe(&scanned), oracle::expect(&decoded, None));
    }
    Ok(())
}

proptest! {
    /// Random requests encoded by prost scan to the values prost decodes, including rejections.
    #[test]
    fn prost_encodings_scan_to_the_same_values(
        request in generate::request(false),
        cutoff_ns in proptest::option::of(any::<u64>()),
    ) {
        let bytes = request.encode_to_vec();
        let min_start = cutoff_ns.map(Timestamp::from_unix_nanos);
        let scanned = TraceRequest::scan(&bytes, min_start).unwrap();
        let expected = oracle::expect(&request, min_start.map(Timestamp::micros));
        prop_assert_eq!(oracle::observe(&scanned), expected.clone());
        prop_assert_eq!(scanned.span_count() as usize, expected.len());
        let rejected = expected.iter().filter(|s| s.rejection.is_some()).count();
        let reported = scanned.partial_success().map_or(0, |p| p.rejected_spans());
        prop_assert_eq!(reported as usize, rejected);
        let lens: Vec<usize> = scanned.spans().map(|s| s.encoded_len()).collect();
        let prost_lens: Vec<usize> = request
            .resource_spans
            .iter()
            .flat_map(|rs| &rs.scope_spans)
            .flat_map(|ss| &ss.spans)
            .map(Message::encoded_len)
            .collect();
        prop_assert_eq!(lens, prost_lens);
    }

    /// Arbitrary bytes never panic the scan, and it agrees with prost.
    #[test]
    fn arbitrary_bytes_agree_with_prost(bytes in vec(any::<u8>(), 0..512)) {
        agrees_with_prost(&bytes)?;
    }

    /// Valid encodings with bytes overwritten, inserted or removed: no panic, agreement with prost.
    #[test]
    fn mutated_encodings_agree_with_prost(
        request in generate::request(false),
        edits in vec((any::<prop::sample::Index>(), any::<u8>(), 0..3u8), 1..5),
    ) {
        let mut bytes = request.encode_to_vec();
        for (at, byte, op) in edits {
            if bytes.is_empty() {
                bytes.push(byte);
                continue;
            }
            let i = at.index(bytes.len());
            match op {
                0 => bytes[i] = byte,
                1 => bytes.insert(i, byte),
                _ => bytes.truncate(i),
            }
        }
        agrees_with_prost(&bytes)?;
    }
}

#[test]
fn body_cap_is_the_canonical_limit() {
    let over = vec![0u8; MAX_BODY_BYTES + 1];
    assert_eq!(
        TraceRequest::scan(&over, None).unwrap_err(),
        ScanError::BodyTooLarge {
            len: MAX_BODY_BYTES + 1
        }
    );
    // Exactly at the cap: one unknown length-delimited field filling the body.
    let mut body = vec![(15 << 3) | 2];
    let payload = MAX_BODY_BYTES - 1 - 4;
    varint(payload as u64, &mut body);
    assert_eq!(body.len(), 5);
    body.resize(MAX_BODY_BYTES, 0);
    let scanned = TraceRequest::scan(&body, None).unwrap();
    assert_eq!(scanned.span_count(), 0);
}

#[test]
fn span_cap_is_fifty_thousand() {
    let span = valid_span().encode_to_vec();
    let spans = vec![span; MAX_SPANS as usize];
    let body = request_of(&spans);
    let scanned = TraceRequest::scan(&body, None).unwrap();
    assert_eq!(scanned.span_count(), MAX_SPANS);
    assert_eq!(scanned.spans().count(), MAX_SPANS as usize);
    let mut more = spans;
    more.push(valid_span().encode_to_vec());
    assert_eq!(
        TraceRequest::scan(&request_of(&more), None).unwrap_err(),
        ScanError::TooManySpans
    );
}

#[test]
fn missing_zero_and_old_spans_are_partial_success() {
    let with = |trace_id: Vec<u8>, span_id: Vec<u8>, start: u64| {
        Span {
            trace_id,
            span_id,
            start_time_unix_nano: start,
            ..valid_span()
        }
        .encode_to_vec()
    };
    let spans = [
        with(vec![], vec![2; 8], 5_000),
        with(vec![0; 16], vec![2; 8], 5_000),
        with(vec![1; 16], vec![], 5_000),
        with(vec![1; 16], vec![0; 8], 5_000),
        with(vec![1; 16], vec![2; 8], 1_000),
        with(vec![1; 16], vec![2; 8], 5_000),
    ];
    let body = request_of(&spans);
    let cutoff = Some(Timestamp::from_micros(2));
    let scanned = TraceRequest::scan(&body, cutoff).unwrap();
    let rejections: Vec<_> = scanned.spans().map(|s| s.rejection()).collect();
    assert_eq!(
        rejections,
        [
            Some(Rejection::MissingTraceId),
            Some(Rejection::MissingTraceId),
            Some(Rejection::MissingSpanId),
            Some(Rejection::MissingSpanId),
            Some(Rejection::TooOld),
            None,
        ]
    );
    let partial = scanned.partial_success().unwrap();
    assert_eq!(partial.rejected_spans(), 5);
    assert_eq!(
        partial.to_string(),
        "rejected 5 of 6 spans: 2 with a missing or all-zero trace_id, 2 with a missing or \
         all-zero span_id, 1 older than the accepted event age"
    );
    // Without a cutoff (import keys, post-ack readers) only the id rules apply.
    let scanned = TraceRequest::scan(&body, None).unwrap();
    assert_eq!(scanned.partial_success().unwrap().rejected_spans(), 4);
    let all_good = request_of(&[valid_span().encode_to_vec()]);
    assert_eq!(
        TraceRequest::scan(&all_good, None)
            .unwrap()
            .partial_success(),
        None
    );
}

#[test]
fn ids_of_the_wrong_length_are_malformed() {
    let span = |trace: usize, span: usize, parent: usize| {
        Span {
            trace_id: vec![1; trace],
            span_id: vec![2; span],
            parent_span_id: vec![3; parent],
            ..valid_span()
        }
        .encode_to_vec()
    };
    assert!(TraceRequest::scan(&request_of(&[span(16, 8, 8)]), None).is_ok());
    assert!(TraceRequest::scan(&request_of(&[span(16, 8, 0)]), None).is_ok());
    let id_length = |field, len| {
        Some(WireError::IdLength {
            message: "Span",
            field,
            len,
        })
    };
    assert_eq!(malformed(&request_of(&[span(15, 8, 0)])), id_length(1, 15));
    assert_eq!(malformed(&request_of(&[span(16, 9, 0)])), id_length(2, 9));
    assert_eq!(malformed(&request_of(&[span(16, 8, 3)])), id_length(4, 3));
    let link = opentelemetry_proto::tonic::trace::v1::span::Link {
        trace_id: vec![1; 5],
        ..Default::default()
    };
    let with_link = Span {
        links: vec![link],
        ..valid_span()
    }
    .encode_to_vec();
    assert_eq!(
        malformed(&request_of(&[with_link])),
        Some(WireError::IdLength {
            message: "Span.Link",
            field: 1,
            len: 5
        })
    );
}

#[test]
fn singular_message_fields_must_not_repeat() {
    let status = |code| {
        Span {
            status: Some(Status {
                message: String::new(),
                code,
            }),
            ..valid_span()
        }
        .encode_to_vec()
    };
    // Concatenated encodings of one message type merge into one message in protobuf.
    let twice = [status(1), status(2)].concat();
    assert_eq!(
        malformed(&request_of(&[twice])),
        Some(WireError::Duplicate {
            message: "Span",
            field: 15
        })
    );
    let kv = |v: &str| field(2, &field(1, v.as_bytes()));
    let doubled_value = [field(1, b"k"), kv("a"), kv("b")].concat();
    let span = [valid_span().encode_to_vec(), field(9, &doubled_value)].concat();
    assert_eq!(
        malformed(&request_of(&[span])),
        Some(WireError::Duplicate {
            message: "KeyValue",
            field: 2
        })
    );
    let scope_spans = [field(1, b""), field(1, b"")].concat();
    assert_eq!(
        malformed(&field(1, &field(2, &scope_spans))),
        Some(WireError::Duplicate {
            message: "ScopeSpans",
            field: 1
        })
    );
    let resource_spans = [field(1, b""), field(1, b"")].concat();
    assert_eq!(
        malformed(&field(1, &resource_spans)),
        Some(WireError::Duplicate {
            message: "ResourceSpans",
            field: 1
        })
    );
}

#[test]
fn any_value_oneof_takes_the_last_field() {
    let array = |item: &str| field(5, &field(1, &field(1, item.as_bytes())));
    let attribute = |value: Vec<u8>| {
        let kv = [field(1, b"k"), field(2, &value)].concat();
        let span = [valid_span().encode_to_vec(), field(9, &kv)].concat();
        request_of(&[span])
    };
    // A string replaces an array, and a later array replaces the string.
    let body = attribute([array("a"), field(1, b"s"), array("b")].concat());
    let scanned = TraceRequest::scan(&body, None).unwrap();
    let observed = oracle::observe(&scanned);
    assert_eq!(
        observed[0].attributes,
        [(
            "k".to_owned(),
            oracle::Val::Array(vec![oracle::Val::Str("b".to_owned())])
        )]
    );
    let decoded = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
    assert_eq!(observed, oracle::expect(&decoded, None));
    // Two arrays in a row would merge, so they are refused.
    assert_eq!(
        malformed(&attribute([array("a"), array("b")].concat())),
        Some(WireError::Duplicate {
            message: "AnyValue",
            field: 5
        })
    );
}

#[test]
fn scalars_take_the_last_occurrence() {
    let span = [
        valid_span().encode_to_vec(),
        field(5, b"renamed"),
        field(1, &[7; 16]),
    ]
    .concat();
    let body = request_of(&[span]);
    let scanned = TraceRequest::scan(&body, None).unwrap();
    let view = scanned.spans().next().unwrap();
    assert_eq!(view.name(), "renamed");
    assert_eq!(view.trace_id().to_bytes(), [7; 16]);
}

#[test]
fn unknown_fields_of_every_wire_type_are_skipped() {
    // Field 99 as varint, fixed64, length-delimited, group (with a field inside) and fixed32.
    let mut unknown = Vec::new();
    varint(99 << 3, &mut unknown);
    varint(300, &mut unknown);
    varint((99 << 3) | 1, &mut unknown);
    unknown.extend_from_slice(&[0; 8]);
    unknown.extend(field(99, b"junk"));
    varint((99 << 3) | 3, &mut unknown);
    varint(1 << 3, &mut unknown);
    varint(1, &mut unknown);
    varint((99 << 3) | 4, &mut unknown);
    varint((99 << 3) | 5, &mut unknown);
    unknown.extend_from_slice(&[0; 4]);

    let attribute = KeyValue {
        key: "k".to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(7)),
        }),
        key_strindex: 0,
    };
    let span = Span {
        attributes: vec![attribute.clone()],
        status: Some(Status {
            message: "m".to_owned(),
            code: 2,
        }),
        ..valid_span()
    };
    let plain = request_of(&[span.encode_to_vec()]);
    let kv = [attribute.encode_to_vec(), unknown.clone()].concat();
    let noisy_span = [
        Span {
            attributes: vec![],
            ..span.clone()
        }
        .encode_to_vec(),
        unknown.clone(),
        field(9, &kv),
    ]
    .concat();
    let noisy = [request_of(&[noisy_span]), unknown].concat();
    let a = oracle::observe(&TraceRequest::scan(&plain, None).unwrap());
    let b = oracle::observe(&TraceRequest::scan(&noisy, None).unwrap());
    assert_eq!(a, b);
}

#[test]
fn wire_errors_are_typed() {
    let mut name_as_varint = valid_span().encode_to_vec();
    varint(5 << 3, &mut name_as_varint);
    varint(1, &mut name_as_varint);
    assert_eq!(
        malformed(&request_of(&[name_as_varint])),
        Some(WireError::WireType {
            message: "Span",
            field: 5
        })
    );
    let bad_utf8 = [valid_span().encode_to_vec(), field(5, &[0xff, 0xfe])].concat();
    assert_eq!(
        malformed(&request_of(&[bad_utf8])),
        Some(WireError::Utf8 {
            message: "Span",
            field: 5
        })
    );
    let body = request_of(&[valid_span().encode_to_vec()]);
    assert_eq!(
        malformed(&body[..body.len() - 1]),
        Some(WireError::Truncated)
    );
    assert_eq!(malformed(&[0x00]), Some(WireError::Key));
    assert_eq!(malformed(&[0x0c]), Some(WireError::EndGroup));
}

#[test]
fn the_nesting_limit_agrees_with_prost() {
    let nested = |depth: usize| {
        let mut value = AnyValue {
            value: Some(any_value::Value::StringValue("leaf".to_owned())),
        };
        for _ in 0..depth {
            value = AnyValue {
                value: Some(any_value::Value::ArrayValue(ArrayValue {
                    values: vec![value],
                })),
            };
        }
        let span = Span {
            attributes: vec![KeyValue {
                key: "deep".to_owned(),
                value: Some(value),
                key_strindex: 0,
            }],
            ..valid_span()
        };
        request_of(&[span.encode_to_vec()])
    };
    let mut outcomes = Vec::new();
    for depth in 40..56 {
        let body = nested(depth);
        let prost_ok = ExportTraceServiceRequest::decode(body.as_slice()).is_ok();
        let scan = TraceRequest::scan(&body, None);
        assert_eq!(scan.is_ok(), prost_ok, "depth {depth}");
        if let Err(e) = scan {
            assert_eq!(e, ScanError::Malformed(WireError::TooDeep));
        }
        outcomes.push(prost_ok);
    }
    assert!(outcomes.contains(&true) && outcomes.contains(&false));
    // Views reach the innermost value of the deepest accepted body.
    let deepest = (40..56).filter(|d| outcomes[d - 40]).max().unwrap();
    let body = nested(deepest);
    let scanned = TraceRequest::scan(&body, None).unwrap();
    let span = scanned.spans().next().unwrap();
    let (_, mut value) = span.attributes().next().unwrap();
    let mut levels = 0;
    while let AnyValueView::Array(mut values) = value {
        value = values.next().unwrap();
        levels += 1;
    }
    assert_eq!(levels, deepest);
    assert!(matches!(value, AnyValueView::String("leaf")));
}
