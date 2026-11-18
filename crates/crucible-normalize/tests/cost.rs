//! Usage and cost (contracts 3.1, 3.4 and 10.5): provided costs as sent, and otherwise usage priced from the price
//! tables of the context.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::double::Double;
use common::{BASE_NANOS, ctx, frame, int, kv, prices, request, run, s, span_with};
use crucible_normalize::{NormCtx, NormalizedFrame, PriceSnapshot, PriceTable, normalize_with};
use crucible_records::{CostSource, NanoUsd, PayloadCaps, SpanRecord, Timestamp, UsageEntry, caps};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};

fn one(attributes: Vec<KeyValue>) -> SpanRecord {
    one_with(&ctx(), attributes)
}

fn one_with(ctx: &NormCtx<'_>, attributes: Vec<KeyValue>) -> SpanRecord {
    let body = request(vec![span_with(1, 1, None, attributes)]);
    let out: NormalizedFrame = normalize_with(&[&Double::NATIVE], &frame(&body, &[]), ctx).unwrap();
    out.spans.into_iter().next().unwrap()
}

/// The start of the test spans.
const START: Timestamp = Timestamp::from_unix_nanos(BASE_NANOS);

/// The ids recorded for the bundled row of `model` and its tier named `tier`.
fn ids(
    model: &str,
    tier: &str,
) -> (
    Option<crucible_records::Uuid>,
    Option<crucible_records::Uuid>,
) {
    let row = prices().find(model, START).unwrap();
    let tier = row.tiers.iter().find(|t| &*t.name == tier).unwrap();
    (Some(row.id), Some(tier.id))
}

fn price_ids(
    r: &SpanRecord,
) -> (
    Option<crucible_records::Uuid>,
    Option<crucible_records::Uuid>,
) {
    (r.core.price_model_id, r.core.price_tier_id)
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

#[test]
fn usage_without_a_provided_cost_is_calculated() {
    let r = one(vec![
        s("t.model_response", "gpt-4o"),
        int("t.usage.input", 1_000),
        int("t.usage.cache_read", 400),
        int("t.usage.output", 500),
    ]);
    assert_eq!(r.core.cost_source, CostSource::Calculated);
    // 600 uncached input tokens at 2.50 USD per million, 400 cache reads at 1.25 and 500 output tokens at 10.
    assert_eq!(
        usage(&r),
        [
            ("cache_read".to_owned(), 400, 500_000),
            ("input".to_owned(), 1_000, 1_500_000),
            ("output".to_owned(), 500, 5_000_000),
            ("total".to_owned(), 1_500, 0),
        ]
    );
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(7_000_000));
    assert_eq!(price_ids(&r), ids("gpt-4o", "Standard"));
}

#[test]
fn native_spans_are_priced() {
    let body = request(vec![span_with(
        1,
        1,
        None,
        vec![
            s("gen_ai.request.model", "gpt-4o"),
            int("gen_ai.usage.input_tokens", 1_000),
            int("gen_ai.usage.cache_read.input_tokens", 400),
            int("gen_ai.usage.output_tokens", 500),
        ],
    )]);
    let r = &run(&body).spans[0];
    assert_eq!(r.core.cost_source, CostSource::Calculated);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(7_000_000));
}

#[test]
fn a_provided_cost_wins_over_the_price_table() {
    let r = one(vec![
        s("t.model_response", "gpt-4o"),
        int("t.usage.input", 1_000),
        s("t.cost.input", "0.5"),
    ]);
    assert_eq!(r.core.cost_source, CostSource::Provided);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(500_000_000));
    assert_eq!(
        usage(&r),
        [
            ("input".to_owned(), 1_000, 500_000_000),
            ("total".to_owned(), 1_000, 0)
        ]
    );
    assert_eq!(price_ids(&r), (None, None));
}

#[test]
fn without_a_matching_row_or_usage_there_is_no_cost() {
    for attributes in [
        vec![
            s("t.model_response", "model-x"),
            int("t.usage.input", 1_000),
        ],
        vec![s("t.model_response", "gpt-4o")],
        vec![int("t.usage.input", 1_000)],
    ] {
        let r = one(attributes);
        assert_eq!(r.core.cost_source, CostSource::None);
        assert_eq!(r.core.total_cost_nusd, NanoUsd::ZERO);
        assert!(r.core.usage.iter().all(|e| e.cost == NanoUsd::ZERO));
        assert_eq!(price_ids(&r), (None, None));
    }
}

#[test]
fn the_response_model_is_priced_and_the_requested_model_only_without_one() {
    let input = |r: &SpanRecord| usage(r).first().map(|e| e.2);
    // The dated row prices input at 5 USD per million; the undated one at 2.50.
    let both = one(vec![
        s("t.model_requested", "gpt-4o"),
        s("t.model_response", "gpt-4o-2024-05-13"),
        int("t.usage.input", 1_000),
    ]);
    assert_eq!(input(&both), Some(5_000_000));
    let requested = one(vec![
        s("t.model_requested", "gpt-4o"),
        int("t.usage.input", 1_000),
    ]);
    assert_eq!(input(&requested), Some(2_500_000));
    // An unknown response model is not priced as the requested one.
    let unknown = one(vec![
        s("t.model_requested", "gpt-4o"),
        s("t.model_response", "model-x"),
        int("t.usage.input", 1_000),
    ]);
    assert_eq!(unknown.core.cost_source, CostSource::None);
}

#[test]
fn a_model_parameter_selects_its_tier() {
    let r = one(vec![
        s("t.model_response", "gpt-4o"),
        s(
            "t.model_parameters",
            r#"{"service_tier":"priority","temperature":0.2}"#,
        ),
        int("t.usage.input", 1_000),
    ]);
    assert_eq!(r.core.total_cost_nusd, NanoUsd::from_nanos(4_250_000));
    assert_eq!(price_ids(&r), ids("gpt-4o", "Fast mode"));
}

#[test]
fn a_usage_threshold_selects_its_tier() {
    let at = |input: i64| {
        one(vec![
            s("t.model_response", "gemini-2.5-pro"),
            int("t.usage.input", input),
        ])
    };
    assert_eq!(price_ids(&at(200_000)), ids("gemini-2.5-pro", "Standard"));
    assert_eq!(
        price_ids(&at(200_001)),
        ids("gemini-2.5-pro", "Large Context")
    );
    // 200,001 input tokens at 2.50 USD per million.
    assert_eq!(
        at(200_001).core.total_cost_nusd,
        NanoUsd::from_nanos(500_002_500)
    );
}

#[test]
fn project_rows_win_and_rows_apply_from_their_start() {
    let bundled = PriceTable::from_json(
        1,
        r#"[
            {"name": "m", "match": "m", "tiers": [{"name": "S", "priority": 1, "prices": {"input": 1000}}]},
            {"name": "m", "match": "m", "start": "2026-11-17", "tiers": [{"name": "S", "priority": 1, "prices": {"input": 2000}}]},
            {"name": "m", "match": "m", "start": "2026-11-18", "tiers": [{"name": "S", "priority": 1, "prices": {"input": 4000}}]}
        ]"#,
    )
    .unwrap();
    let project = PriceTable::from_json(
        2,
        r#"[{"name": "p", "match": "p|m", "start": "2026-11-18", "tiers": [{"name": "S", "priority": 1, "prices": {"input": 8000}}]},
            {"name": "p", "match": "p", "tiers": [{"name": "S", "priority": 1, "prices": {"input": 16000}}]}]"#,
    )
    .unwrap();
    let mut ctx = NormCtx::new(&bundled, PayloadCaps::DEFAULT);
    assert_eq!(ctx.price_table_version, 1);
    let cost = |ctx: &NormCtx<'_>, model: &str| {
        let r = one_with(
            ctx,
            vec![s("t.model_response", model), int("t.usage.input", 10)],
        );
        r.core.total_cost_nusd.nanos()
    };
    // The span starts on 2026-11-17: the row of that day applies, not the next day's.
    assert_eq!(cost(&ctx, "m"), 20);
    assert_eq!(cost(&ctx, "p"), 0);
    ctx.prices = PriceSnapshot {
        project: Some(&project),
        bundled: &bundled,
    };
    // The project row for "m" starts later, so the bundled row still applies; "p" has a project row.
    assert_eq!(cost(&ctx, "m"), 20);
    assert_eq!(cost(&ctx, "p"), 160);
}
