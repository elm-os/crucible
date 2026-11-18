//! The request-local forward-fill of trace-level attributes (contract 3.3), through the test-double mapper (which
//! has sources for every trace-level attribute) and through the native table (`user.id`).

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::double::Double;
use common::{bitmap, ctx, frame, kv, request, run, s, span, span_with, strings};
use crucible_normalize::{NormalizedFrame, normalize_with};
use crucible_records::TracePublic;
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::Span;
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

fn run_double(body: &[u8], rejected: &[u8]) -> NormalizedFrame {
    normalize_with(&[&Double::NATIVE], &frame(body, rejected), &ctx()).unwrap()
}

fn names(out: &NormalizedFrame) -> Vec<&str> {
    out.spans.iter().map(|r| &*r.core.trace_name).collect()
}

#[test]
fn the_root_fills_spans_before_and_after_it() {
    let body = request(vec![
        span(1, 2, Some(1)),
        span_with(1, 1, None, vec![s("t.trace_name", "root")]),
        span(1, 3, Some(2)),
    ]);
    assert_eq!(names(&run_double(&body, &[])), ["root", "root", "root"]);
}

#[test]
fn without_a_root_the_earliest_span_with_a_value_fills() {
    let body = request(vec![
        span(1, 2, Some(1)),
        span_with(1, 3, Some(1), vec![s("t.trace_name", "first")]),
        span_with(1, 4, Some(1), vec![s("t.trace_name", "second")]),
        span(1, 5, Some(1)),
    ]);
    assert_eq!(
        names(&run_double(&body, &[])),
        ["first", "first", "second", "first"]
    );
}

#[test]
fn the_first_root_with_a_value_fills() {
    let body = request(vec![
        span_with(1, 2, Some(1), vec![s("t.trace_name", "child")]),
        span(1, 1, None),
        span_with(1, 9, None, vec![s("t.trace_name", "second root")]),
        span(1, 3, Some(1)),
    ]);
    assert_eq!(
        names(&run_double(&body, &[])),
        ["child", "second root", "second root", "second root"]
    );
}

#[test]
fn each_attribute_fills_on_its_own() {
    let body = request(vec![
        span_with(1, 1, None, vec![s("t.trace_name", "root")]),
        span_with(1, 2, Some(1), vec![s("t.user_id", "child user")]),
        span(1, 3, Some(1)),
    ]);
    let out = run_double(&body, &[]);
    for r in &out.spans {
        assert_eq!(
            (&*r.core.trace_name, &*r.core.user_id),
            ("root", "child user")
        );
    }
}

fn all_trace_level() -> Vec<KeyValue> {
    vec![
        s("t.trace_name", "name"),
        s("t.user_id", "user"),
        s("t.session_id", "session"),
        strings("t.trace_tags", &["a", "b"]),
        s("t.trace_metadata", r#"{"k":1}"#),
        kv("t.trace_public", any_value::Value::BoolValue(true)),
        s("t.environment", "prod"),
        s("t.release", "r1"),
        s("t.version", "v1"),
    ]
}

#[test]
fn all_nine_attributes_fill_and_the_default_environment_comes_after() {
    let body = request(vec![
        span(1, 2, Some(1)),
        span_with(1, 1, None, all_trace_level()),
        span(2, 3, None),
    ]);
    let out = run_double(&body, &[]);
    let (child, other) = (&out.spans[0], &out.spans[2]);
    let c = &child.core;
    assert_eq!(
        (&*c.trace_name, &*c.user_id, &*c.session_id),
        ("name", "user", "session")
    );
    assert_eq!(c.trace_tags, vec!["a".into(), "b".into()]);
    assert_eq!(&child.payload.trace_metadata[..], br#"{"k":1}"#);
    assert_eq!(c.trace_public, TracePublic::Public);
    assert_eq!(
        (&*c.environment, &*c.release, &*c.version),
        ("prod", "r1", "v1")
    );
    // Another trace in the same request is untouched and takes the default environment.
    let o = &other.core;
    assert_eq!(
        (&*o.trace_name, &*o.user_id, &*o.environment),
        ("", "", "default")
    );
    assert_eq!(o.trace_public, TracePublic::Unset);
    assert!(o.trace_tags.is_empty() && other.payload.trace_metadata.is_empty());
}

#[test]
fn a_span_rejected_before_the_ack_never_fills() {
    let body = request(vec![
        span_with(1, 1, None, vec![s("t.trace_name", "rejected root")]),
        span(1, 2, Some(1)),
        span(1, 3, Some(1)),
    ]);
    let out = run_double(&body, &bitmap(&[0]));
    assert_eq!(out.summary.rejected_before_ack, 1);
    assert_eq!(names(&out), ["", ""]);
}

#[test]
fn nothing_is_backfilled_into_an_earlier_request() {
    let first = request(vec![span(1, 2, Some(1))]);
    let second = request(vec![span_with(
        1,
        1,
        None,
        vec![s("t.trace_name", "late root")],
    )]);
    let before = run_double(&first, &[]);
    assert_eq!(names(&before), [""]);
    assert_eq!(names(&run_double(&second, &[])), ["late root"]);
    // The earlier request normalizes exactly as before: no state carried the later value back.
    assert_eq!(run_double(&first, &[]), before);
}

#[test]
fn the_native_table_fills_user_id() {
    let body = request(vec![
        span(1, 2, Some(1)),
        span_with(1, 1, None, vec![s("user.id", "u-1")]),
        span(1, 3, Some(1)),
        span(2, 4, None),
    ]);
    let users: Vec<String> = run(&body)
        .spans
        .iter()
        .map(|r| r.core.user_id.to_string())
        .collect();
    assert_eq!(users, ["u-1", "u-1", "u-1", ""]);
}

/// One span of the property below: (trace, is_root, trace_name value, release value, rejected).
type Shape = (u8, bool, Option<u8>, Option<u8>, bool);

/// Contract 3.3 applied directly: the span's own value, else the first non-rejected root of its trace with a
/// value, else the first non-rejected span of its trace with a value, else empty.
fn expected(shapes: &[Shape], pick: fn(&Shape) -> Option<u8>) -> Vec<String> {
    let donor = |trace: u8| {
        let candidates = || shapes.iter().filter(|s| s.0 == trace && !s.4);
        candidates()
            .find(|s| s.1 && pick(s).is_some())
            .or_else(|| candidates().find(|s| pick(s).is_some()))
            .and_then(pick)
    };
    shapes
        .iter()
        .filter(|s| !s.4)
        .map(|s| {
            pick(s)
                .or_else(|| donor(s.0))
                .map_or_else(String::new, |v| format!("v{v}"))
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    #[test]
    fn the_fill_follows_the_rule(
        shapes in vec((0u8..3, any::<bool>(), option::of(0u8..3), option::of(0u8..3), prop::bool::weighted(0.15)), 1..12),
    ) {
        let spans: Vec<Span> = shapes
            .iter()
            .enumerate()
            .map(|(i, &(t, root, name, release, _))| {
                let mut attributes = Vec::new();
                if let Some(v) = name {
                    attributes.push(s("t.trace_name", &format!("v{v}")));
                }
                if let Some(v) = release {
                    attributes.push(s("t.release", &format!("v{v}")));
                }
                let parent = (!root).then_some(250);
                span_with(t + 1, i as u8 + 1, parent, attributes)
            })
            .collect();
        let rejected: Vec<u32> = shapes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.4)
            .map(|(i, _)| i as u32)
            .collect();
        let out = run_double(&request(spans), &bitmap(&rejected));
        let got = |f: fn(&crucible_records::SpanRecord) -> String| out.spans.iter().map(f).collect::<Vec<_>>();
        prop_assert_eq!(got(|r| r.core.trace_name.to_string()), expected(&shapes, |s| s.2));
        prop_assert_eq!(got(|r| r.core.release.to_string()), expected(&shapes, |s| s.3));
    }
}
