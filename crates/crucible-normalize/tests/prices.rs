//! The bundled price table: a corpus of model names with the row each must match (or none), and tier choices and
//! prices for usage at the boundaries of conditional tiers.
//!
//! Usage here is Crucible usage: `input` holds its cache reads and writes, and `output` its reasoning (contract
//! 10.5).

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

use crucible_normalize::{ModelPrice, PriceTable};
use crucible_records::{NanoUsd, Timestamp, UsageEntry};
use serde::Deserialize;

/// 2026-11-17T09:30:00Z.
const AT: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

fn bundled() -> &'static PriceTable {
    PriceTable::bundled().unwrap()
}

fn row(model: &str) -> &'static ModelPrice {
    bundled()
        .find(model, AT)
        .unwrap_or_else(|| panic!("no row matches {model}"))
}

fn usage(pairs: &[(&str, u64)]) -> Vec<UsageEntry> {
    let mut usage: Vec<UsageEntry> = pairs
        .iter()
        .map(|&(key, units)| UsageEntry {
            key: key.into(),
            units,
            cost: NanoUsd::ZERO,
        })
        .collect();
    usage.sort_by(|a, b| a.key.cmp(&b.key));
    usage
}

#[derive(Deserialize)]
struct Case {
    model: String,
    row: Option<String>,
}

#[test]
fn model_names_match_their_rows() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/model_names.json")).unwrap();
    assert_eq!(cases.len(), 199);
    assert_eq!(cases.iter().filter(|c| c.row.is_some()).count(), 118);
    let mismatches: Vec<String> = cases
        .iter()
        .filter_map(|c| {
            let found = bundled().find(&c.model, AT).map(|m| m.name.to_string());
            (found != c.row).then(|| format!("{}: want {:?}, got {found:?}", c.model, c.row))
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[test]
fn regional_model_ids_price_at_their_last_tier() {
    // (model, input, output) in pico-USD per token
    let cases = [
        ("us.openai.gpt-5.6-luna", 220_000, Some(1_320_000)),
        ("us.openai.gpt-6-luna", 110_000, Some(550_000)),
        ("global.openai.gpt-6-luna", 100_000, Some(500_000)),
        ("us.openai.gpt-5.6-terra", 2_200_000, Some(13_200_000)),
        ("us.cohere.embed-v4:0", 120_000, None),
        ("eu.cohere.embed-v4:0", 120_000, None),
        ("us.anthropic.claude-opus-4-7", 5_000_000, Some(25_000_000)),
        ("ca.anthropic.claude-haiku-5-5", 100_000, Some(500_000)),
    ];
    for (model, input, output) in cases {
        let last = row(model).tiers.last().unwrap();
        assert!(last.conditions.is_empty(), "{model}");
        assert_eq!(last.price("input"), Some(input), "{model}");
        if output.is_some() {
            assert_eq!(last.price("output"), output, "{model}");
        }
    }
}

#[test]
fn cache_reads_and_writes_count_toward_the_long_context_threshold() {
    let model = row("us.openai.gpt-6-luna");
    let at = |input: u64| {
        usage(&[
            ("input", input),
            ("cache_read", 100_000),
            ("cache_write", 72_000),
            ("output", 1_000),
            ("reasoning", 1_000),
            ("total", input + 1_000),
        ])
    };
    assert_eq!(&*model.tier(&at(272_000), b"").unwrap().name, "Standard");
    let long = model.tier(&at(272_001), b"").unwrap();
    assert_eq!(&*long.name, "Large Context (>272K)");
    assert_eq!(long.price("input"), Some(220_000));
    // Reasoning has no price of its own, so it is priced with output.
    assert_eq!(
        (long.price("output"), long.price("reasoning")),
        (Some(825_000), None)
    );
}

#[test]
fn reasoning_is_priced_at_the_output_rate() {
    let cases = [
        ("gpt-5.4-mini", 4_500_000),
        ("gpt-5.4-mini-2026-03-17", 4_500_000),
        ("gpt-5.4-nano", 1_250_000),
        ("gpt-5.4-nano-2026-03-17", 1_250_000),
    ];
    for (model, output) in cases {
        let row = row(model);
        for tier in &row.tiers {
            assert_eq!(tier.price("reasoning"), None, "{model} {}", tier.name);
            let mut with = usage(&[("output", 1_000), ("reasoning", 400), ("total", 1_000)]);
            let mut without = usage(&[("output", 1_000), ("total", 1_000)]);
            assert_eq!(tier.apply(&mut with), tier.apply(&mut without), "{model}");
        }
        assert_eq!(row.tiers.last().unwrap().price("output"), Some(output));
    }
}

#[test]
fn cache_writes_are_priced() {
    let cases = [
        ("gpt-5.4-mini", 750_000),
        ("gpt-5.4-mini-2026-03-17", 750_000),
        ("gpt-5.5-2026-04-23", 5_000_000),
        ("gpt-4.1", 2_000_000),
        ("gpt-5.6-sol", 5_000_000),
        ("gpt-6-sol", 2_500_000),
    ];
    let u = usage(&[
        ("input", 230_000),
        ("cache_write", 227_060),
        ("output", 1_000),
        ("total", 231_000),
    ]);
    for (model, write) in cases {
        let tier = row(model).tier(&u, b"").unwrap();
        assert_eq!(&*tier.name, "Standard", "{model}");
        // A write without a price of its own is priced as input.
        let effective = tier.price("cache_write").or(tier.price("input"));
        assert_eq!(effective, Some(write), "{model}");
        assert!(tier.price("output").is_some(), "{model}");
    }
}

#[test]
fn cache_writes_count_toward_the_large_context_threshold() {
    let u = usage(&[
        ("input", 300_000),
        ("cache_write", 200_000),
        ("output", 1_000),
        ("total", 301_000),
    ]);
    for model in ["gpt-5.4", "gpt-5.4-2026-03-05", "gpt-5.5-2026-04-23"] {
        let tier = row(model).tier(&u, b"").unwrap();
        assert_eq!(&*tier.name, "Large Context (>272K)", "{model}");
    }
}

#[test]
fn pro_tiers_split_on_service_tier_and_prompt_size() {
    // (model, standard, large context, priority, priority large context): [input, output, cache_read]
    let cases = [
        (
            "gemini-3.1-pro-preview",
            [2_000_000, 12_000_000, 200_000],
            [4_000_000, 18_000_000, 400_000],
            [3_600_000, 21_600_000, 360_000],
            [7_200_000, 32_400_000, 720_000],
        ),
        (
            "gemini-2.5-pro",
            [1_250_000, 10_000_000, 125_000],
            [2_500_000, 15_000_000, 250_000],
            [2_250_000, 18_000_000, 225_000],
            [4_500_000, 27_000_000, 450_000],
        ),
    ];
    for (model, standard, large, priority, priority_large) in cases {
        let row = row(model);
        for cached in [50_000, 50_001] {
            for service_tier in [Some("priority"), Some("standard"), None] {
                let params =
                    service_tier.map_or(String::new(), |s| format!(r#"{{"service_tier": "{s}"}}"#));
                let u = usage(&[
                    ("input", 150_000 + cached),
                    ("cache_read", cached),
                    ("output", 1_000),
                    ("total", 151_000 + cached),
                ]);
                let tier = row.tier(&u, params.as_bytes()).unwrap();
                let (name, prices) = match (service_tier == Some("priority"), cached > 50_000) {
                    (true, true) => ("Priority Large Context", priority_large),
                    (true, false) => ("Priority", priority),
                    (false, true) => ("Large Context", large),
                    (false, false) => ("Standard", standard),
                };
                let case = format!("{model} cached {cached} {service_tier:?}");
                assert_eq!(&*tier.name, name, "{case}");
                assert_eq!(
                    [
                        tier.price("input"),
                        tier.price("output"),
                        tier.price("cache_read")
                    ],
                    prices.map(Some),
                    "{case}"
                );
            }
        }
    }
}
