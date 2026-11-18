//! Usage and cost from provided values (contracts 3.1 and 3.4). Milestone 1 has no price table, so without a
//! provided cost the source is `none` and every cost is 0.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::double::Double;
use common::{ctx, frame, int, kv, request, run, s, span_with};
use crucible_normalize::{NormalizedFrame, normalize_with};
use crucible_records::{CostSource, NanoUsd, SpanRecord, UsageEntry, caps};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};

fn one(attributes: Vec<KeyValue>) -> SpanRecord {
    let body = request(vec![span_with(1, 1, None, attributes)]);
    let out: NormalizedFrame =
        normalize_with(&[&Double::NATIVE], &frame(&body, &[]), &ctx()).unwrap();
    out.spans.into_iter().next().unwrap()
}

fn usage(r: &SpanRecord) -> Vec<(String, u64, i64)> {
    r.core
        .usage
        .iter()
        .map(|e| (e.key.to_string(), e.units, e.cost.nanos()))
        .collect()
}

#[test]
fn a_provided_total_wins() {
    let r = one(vec![
        int("t.usage.input", 100),
        int("t.usage.output", 50),
        s("t.cost.input", "0.5"),
        s("t.cost.output", "0.25"),
        s("t.cost.total", "1"),
    ]);
    assert_eq!(r.core.cost_source, CostSource::Provided);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(1_000_000_000));
    assert_eq!(
        usage(&r),
        [
            ("input".to_owned(), 100, 500_000_000),
            ("output".to_owned(), 50, 250_000_000),
            ("total".to_owned(), 150, 1_000_000_000),
        ]
    );
}

#[test]
fn without_a_total_the_parts_are_summed() {
    let r = one(vec![
        s("t.cost.input", "0.5"),
        kv("t.cost.output", any_value::Value::DoubleValue(0.25)),
        s("t.cost.other", "0.000000001"),
    ]);
    assert_eq!(r.core.cost_source, CostSource::Provided);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(750_000_001));
    // No usage was provided, so there is none, and no cost key without usage enters `usage`.
    assert!(r.core.usage.is_empty());
    assert_eq!(r.payload.cost_provided.len(), 3);
}

#[test]
fn negative_and_nan_costs_count_as_zero() {
    let r = one(vec![
        int("t.cost_nanos.input", -5),
        kv("t.cost.output", any_value::Value::DoubleValue(f64::NAN)),
        s("t.cost.cache_read", "-1.5"),
    ]);
    assert_eq!(r.core.cost_source, CostSource::Provided);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::ZERO);
    assert!(
        r.payload
            .cost_provided
            .iter()
            .all(|(_, c)| *c == NanoUsd::ZERO)
    );
}

#[test]
fn sums_saturate() {
    let r = one(vec![
        int("t.cost_nanos.input", i64::MAX),
        int("t.cost_nanos.output", i64::MAX),
    ]);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::MAX);
}

#[test]
fn without_a_provided_cost_there_is_no_cost() {
    let r = one(vec![int("t.usage.input", 7)]);
    assert_eq!(r.core.cost_source, CostSource::None);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::ZERO);
    assert_eq!(
        usage(&r),
        [("input".to_owned(), 7, 0), ("total".to_owned(), 7, 0)]
    );
    assert_eq!((r.core.price_model_id, r.core.price_tier_id), (None, None));
}

#[test]
fn a_provided_total_usage_is_kept_and_scalars_follow_usage() {
    let r = one(vec![
        int("t.usage.input", 100),
        int("t.usage.output", 50),
        int("t.usage.total", 7),
        int("t.usage.reasoning", 3),
    ]);
    assert_eq!(
        (
            r.core.input_tokens,
            r.core.output_tokens,
            r.core.total_tokens
        ),
        (100, 50, 7)
    );
    let keys: Vec<String> = r.core.usage.iter().map(|e| e.key.to_string()).collect();
    assert_eq!(keys, ["input", "output", "reasoning", "total"]);
}

#[test]
fn a_repeated_usage_key_keeps_its_first_value() {
    let r = one(vec![int("t.usage.input", 1), int("t.usage.input", 2)]);
    assert_eq!(r.payload.usage_provided, vec![("input".into(), 1)]);
    assert_eq!(r.core.input_tokens, 1);
}

#[test]
fn usage_keeps_at_most_32_keys_in_byte_order() {
    let attributes: Vec<KeyValue> = (0..40)
        .map(|i| int(&format!("t.usage.k{i:02}"), i))
        .collect();
    let r = one(attributes);
    assert_eq!(r.core.usage.len(), caps::USAGE_MAX_KEYS);
    assert_eq!(&*r.core.usage[0].key, "k00");
    assert_eq!(&*r.core.usage[31].key, "k31");
    // The provided map keeps everything the client sent.
    assert_eq!(r.payload.usage_provided.len(), 40);
}

#[test]
fn native_token_usage_maps_to_normalized_keys() {
    let body = request(vec![span_with(
        1,
        1,
        None,
        vec![
            int("gen_ai.usage.input_tokens", 120),
            int("gen_ai.usage.output_tokens", 30),
            int("gen_ai.usage.cache_read.input_tokens", 100),
            int("gen_ai.usage.cache_creation.input_tokens", 20),
            int("gen_ai.usage.reasoning.output_tokens", 10),
        ],
    )]);
    let r = &run(&body).spans[0];
    let provided: Vec<(String, u64)> = r
        .payload
        .usage_provided
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    assert_eq!(
        provided,
        [
            (UsageEntry::CACHE_READ.to_owned(), 100),
            (UsageEntry::CACHE_WRITE.to_owned(), 20),
            (UsageEntry::INPUT.to_owned(), 120),
            (UsageEntry::OUTPUT.to_owned(), 30),
            (UsageEntry::REASONING.to_owned(), 10),
        ]
    );
    assert_eq!(
        (
            r.core.input_tokens,
            r.core.output_tokens,
            r.core.total_tokens
        ),
        (120, 30, 150)
    );
    assert_eq!(r.core.cost_source, CostSource::None);
    // Every token key was consumed.
    assert!(r.payload.attributes.is_empty());
}

#[test]
fn a_negative_token_count_is_not_usage_and_stays_residual() {
    let body = request(vec![span_with(
        1,
        1,
        None,
        vec![int("gen_ai.usage.input_tokens", -1)],
    )]);
    let r = &run(&body).spans[0];
    assert!(r.payload.usage_provided.is_empty() && r.core.usage.is_empty());
    assert_eq!(
        &r.payload.attributes[..],
        br#"{"gen_ai.usage.input_tokens":-1}"#
    );
}
