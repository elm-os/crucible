//! Usage and cost (contracts 3.1, 3.4 and 10.5).
//!
//! - `usage` holds the provided usage under its normalized keys, computed only from provided usage. When `total`
//!   is not provided but `input` or `output` is, `total` is `input + output` (saturating). At most
//!   [`caps::USAGE_MAX_KEYS`] keys are kept, in ascending byte order.
//! - When the client sent any cost, `cost_source` is `provided`, `total_cost_nusd` is the provided `total`, else the
//!   sum of the other provided parts, and each usage key's cost is the provided cost under the same key (0 when
//!   there is none). Negative provided costs count as 0.
//! - Otherwise, when there is usage, the cost is calculated from the price tables: the model is `model_response`,
//!   else `model_requested`; the row and the tier are chosen and the usage is priced as [`crate::PriceTier`]
//!   describes. `cost_source` is then `calculated`, `total_cost_nusd` is the sum of the per-key costs, and the row
//!   and tier ids are recorded.
//! - With no usage, no matching row or no tier whose conditions hold, `cost_source` is `none` and every cost is 0.

use crucible_records::{CostSource, NanoUsd, SpanCore, SpanRecord, UsageEntry, caps};

use crate::price::PriceSnapshot;

/// Sorts a provided map by key and keeps the first entry of each key.
fn normalize_map<V>(map: &mut Vec<(Box<str>, V)>) {
    map.sort_by(|a, b| a.0.cmp(&b.0));
    map.dedup_by(|later, earlier| later.0 == earlier.0);
}

fn get<V: Copy>(map: &[(Box<str>, V)], key: &str) -> Option<V> {
    map.binary_search_by(|(k, _)| (**k).cmp(key))
        .ok()
        .map(|i| map[i].1)
}

/// Fills `usage`, the token scalars and the cost fields from the provided usage and cost, else from `prices`.
pub(crate) fn apply(record: &mut SpanRecord, prices: &PriceSnapshot<'_>) {
    let (core, payload) = (&mut record.core, &mut record.payload);
    normalize_map(&mut payload.usage_provided);
    normalize_map(&mut payload.cost_provided);
    for (_, cost) in &mut payload.cost_provided {
        *cost = (*cost).max(NanoUsd::ZERO);
    }
    let provided = &payload.usage_provided;
    let mut usage: Vec<(Box<str>, u64)> = provided.clone();
    let (input, output) = (
        get(provided, UsageEntry::INPUT),
        get(provided, UsageEntry::OUTPUT),
    );
    if get(provided, UsageEntry::TOTAL).is_none() && (input.is_some() || output.is_some()) {
        let total = input.unwrap_or(0).saturating_add(output.unwrap_or(0));
        usage.push((UsageEntry::TOTAL.into(), total));
        normalize_map(&mut usage);
    }
    usage.truncate(caps::USAGE_MAX_KEYS);
    let costs = &payload.cost_provided;
    core.usage = usage
        .into_iter()
        .map(|(key, units)| {
            let cost = get(costs, &key).unwrap_or(NanoUsd::ZERO);
            UsageEntry { key, units, cost }
        })
        .collect();
    let units = |key: &str| {
        core.usage
            .iter()
            .find(|e| &*e.key == key)
            .map_or(0, |e| e.units)
    };
    (core.input_tokens, core.output_tokens, core.total_tokens) = (
        units(UsageEntry::INPUT),
        units(UsageEntry::OUTPUT),
        units(UsageEntry::TOTAL),
    );
    core.price_model_id = None;
    core.price_tier_id = None;
    if costs.is_empty() {
        core.cost_source = CostSource::None;
        core.total_cost_nusd = NanoUsd::ZERO;
        if !core.usage.is_empty() {
            calculate(core, &payload.model_parameters, prices);
        }
    } else {
        core.cost_source = CostSource::Provided;
        core.total_cost_nusd = get(costs, UsageEntry::TOTAL).unwrap_or_else(|| {
            NanoUsd::saturating_sum(
                costs
                    .iter()
                    .filter(|(k, _)| &**k != UsageEntry::TOTAL)
                    .map(|&(_, c)| c),
            )
        });
    }
}

/// Prices the calculated usage, when a row and a tier apply.
fn calculate(core: &mut SpanCore, model_parameters: &[u8], prices: &PriceSnapshot<'_>) {
    let model = if core.model_response.is_empty() {
        &core.model_requested
    } else {
        &core.model_response
    };
    let Some(row) = prices.find(model, core.start_time) else {
        return;
    };
    let Some(tier) = row.tier(&core.usage, model_parameters) else {
        return;
    };
    core.total_cost_nusd = tier.apply(&mut core.usage);
    core.cost_source = CostSource::Calculated;
    core.price_model_id = Some(row.id);
    core.price_tier_id = Some(tier.id);
}
