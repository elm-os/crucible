//! Model prices (contract 3.4): the versioned, immutable [`PriceTable`], the bundled table, model matching, tier
//! selection and the exact cost of usage. `docs/architecture/price-table.md` describes the data file and how to
//! change it.

use core::cell::OnceCell;
use core::fmt;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use crucible_records::{CivilDate, NanoUsd, Timestamp, UsageEntry, Uuid};
use regex::{RegexBuilder, RegexSet, RegexSetBuilder};
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value};
use xxhash_rust::xxh3::xxh3_128;

/// The bundled table's data file, compiled in.
const BUNDLED_PRICES: &str = include_str!("../data/prices.json");

/// The bundled table's `price_table_version`, which a `BatchSeal` records (contract 5.3): the low 32 bits of the
/// xxh3-64 of the data file. A test fails until the constant matches the file, so every change to the file ships
/// with a new version.
pub const BUNDLED_PRICE_TABLE_VERSION: u32 = 0x582c_7ae8;

/// The longest match pattern a row may have, in bytes.
const MAX_PATTERN_BYTES: usize = 1_024;

/// The compiled size one match pattern may reach. The largest bundled pattern needs about 23 KB.
const PATTERN_SIZE_LIMIT: usize = 256 * 1_024;

/// How deeply groups and repetitions may nest in one match pattern. The bundled patterns need 10.
const PATTERN_NEST_LIMIT: u32 = 32;

/// The compiled size the match patterns of one table may reach together. The bundled set needs about 0.6 MB.
const SET_SIZE_LIMIT: usize = 16 * 1_024 * 1_024;

/// How a tier condition compares the units of a usage key with its threshold.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compare {
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Gte,
    /// Less than.
    Lt,
    /// Less than or equal.
    Lte,
    /// Equal.
    Eq,
    /// Not equal.
    Neq,
}

impl Compare {
    const fn holds(self, units: u64, threshold: u64) -> bool {
        match self {
            Self::Gt => units > threshold,
            Self::Gte => units >= threshold,
            Self::Lt => units < threshold,
            Self::Lte => units <= threshold,
            Self::Eq => units == threshold,
            Self::Neq => units != threshold,
        }
    }
}

/// A condition of a tier (contract 3.4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Condition {
    /// The units of a calculated usage key (0 when the key is absent) compared with a threshold.
    Usage {
        /// The usage key.
        key: Box<str>,
        /// The comparison.
        op: Compare,
        /// The threshold.
        value: u64,
    },
    /// A top-level member of the span's `model_parameters` object whose value is one of `values`: a string as it
    /// is, a number or a boolean as its JSON text.
    Param {
        /// The parameter name.
        key: Box<str>,
        /// The values that satisfy the condition.
        values: Vec<Box<str>>,
    },
}

/// One price tier of a model row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PriceTier {
    /// Recorded as `price_tier_id`.
    pub id: Uuid,
    /// A label, unique within the row.
    pub name: Box<str>,
    /// Tiers are tried in ascending priority; priorities are unique within the row.
    pub priority: u32,
    /// Every condition must hold. A tier without conditions always holds, so only the last tier may have none.
    pub conditions: Vec<Condition>,
    /// Pico-USD per unit, by usage key. Prices are non-negative and keys unique; a loaded table holds them in
    /// ascending key order.
    pub prices: Vec<(Box<str>, i64)>,
}

/// One model row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelPrice {
    /// Recorded as `price_model_id`.
    pub id: Uuid,
    /// A label. Together with `start` it is unique within the table.
    pub name: Box<str>,
    /// The match pattern, compiled as `(?i)^(?:pattern)$`.
    pub pattern: Box<str>,
    /// The row applies to spans that start at or after this time.
    pub start: Timestamp,
    /// The tiers; a loaded table holds them in ascending priority.
    pub tiers: Vec<PriceTier>,
}

/// A versioned, immutable set of model rows (contract 3.4).
///
/// A row has a name, a match pattern, a start date and tiers; a tier has a priority, conditions and prices in
/// pico-USD per unit of a usage key. A row matches a model name when its pattern matches the whole name, ignoring
/// case: every pattern is compiled as `(?i)^(?:pattern)$`. Matching runs in linear time, and patterns that are too
/// long, too deeply nested or too large once compiled are rejected when the table loads.
///
/// Selection is [`PriceSnapshot::find`], then [`ModelPrice::tier`]: the project's rows before the bundled rows;
/// among the rows that match and start at or before the span's `start_time`, the greatest start, ties going to the
/// row listed first; then the first tier by ascending priority whose conditions all hold.
#[derive(Clone, Debug)]
pub struct PriceTable {
    version: u32,
    models: Vec<ModelPrice>,
    patterns: RegexSet,
}

/// Why a price table does not load.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PriceTableError {
    /// The text is not a table in the data file format.
    #[error("the price table does not parse: {0}")]
    Format(String),
    /// A row breaks a rule of the format.
    #[error("price row {index} ({name}): {reason}")]
    Row {
        /// Position of the row in the table.
        index: usize,
        /// The row's name.
        name: Box<str>,
        /// The broken rule.
        reason: String,
    },
    /// The match patterns do not compile together.
    #[error("the match patterns do not compile together: {0}")]
    Patterns(String),
}

static BUNDLED: LazyLock<Result<PriceTable, PriceTableError>> =
    LazyLock::new(|| PriceTable::from_json(BUNDLED_PRICE_TABLE_VERSION, BUNDLED_PRICES));

impl PriceTable {
    /// A table of `models` at `version`, validated, with its patterns compiled. Each tier's prices are sorted by
    /// key and each row's tiers by priority.
    pub fn new(version: u32, mut models: Vec<ModelPrice>) -> Result<Self, PriceTableError> {
        let mut rows = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut patterns = Vec::with_capacity(models.len());
        for (index, model) in models.iter_mut().enumerate() {
            let checked = check(model).and_then(|()| {
                if !rows.insert((model.name.clone(), model.start)) {
                    return Err("another row has the same name and start".to_owned());
                }
                for id in std::iter::once(model.id).chain(model.tiers.iter().map(|t| t.id)) {
                    if !ids.insert(id) {
                        return Err(format!("id {id} is used twice"));
                    }
                }
                Ok(())
            });
            if let Err(reason) = checked {
                return Err(PriceTableError::Row {
                    index,
                    name: model.name.clone(),
                    reason,
                });
            }
            patterns.push(format!("^(?:{})$", model.pattern));
        }
        let patterns = RegexSetBuilder::new(patterns)
            .case_insensitive(true)
            .size_limit(SET_SIZE_LIMIT)
            .nest_limit(PATTERN_NEST_LIMIT + 1)
            .build()
            .map_err(|e| PriceTableError::Patterns(e.to_string()))?;
        Ok(Self {
            version,
            models,
            patterns,
        })
    }

    /// A table at `version` from text in the data file format. Row and tier ids are version 8 UUIDs hashed from
    /// the row's name and start and the tier's name.
    pub fn from_json(version: u32, text: &str) -> Result<Self, PriceTableError> {
        let rows: Vec<ModelRow> =
            serde_json::from_str(text).map_err(|e| PriceTableError::Format(e.to_string()))?;
        let mut models = Vec::with_capacity(rows.len());
        for (index, row) in rows.into_iter().enumerate() {
            let start = match &row.start {
                None => Timestamp::EPOCH,
                Some(date) => parse_date(date).ok_or_else(|| PriceTableError::Row {
                    index,
                    name: row.name.as_str().into(),
                    reason: format!("start {date:?} is not a YYYY-MM-DD date"),
                })?,
            };
            models.push(row.into_model(start));
        }
        Self::new(version, models)
    }

    /// The bundled table, loaded on first use. A test loads it, so the error is unreachable in a release.
    pub fn bundled() -> Result<&'static Self, PriceTableError> {
        BUNDLED.as_ref().map_err(Clone::clone)
    }

    /// The `price_table_version` this table is immutable under.
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// The rows, in table order.
    pub fn models(&self) -> &[ModelPrice] {
        &self.models
    }

    /// The row for `model` at `at`: among the rows whose pattern matches and that start at or before `at`, the one
    /// with the greatest start, ties going to the row listed first. An empty name matches no row.
    pub fn find(&self, model: &str, at: Timestamp) -> Option<&ModelPrice> {
        if model.is_empty() {
            return None;
        }
        let mut best: Option<&ModelPrice> = None;
        for index in self.patterns.matches(model).iter() {
            let row = &self.models[index];
            if row.start <= at && best.is_none_or(|b| row.start > b.start) {
                best = Some(row);
            }
        }
        best
    }
}

/// The tables one frame is priced with (contract 3.4): the project's override rows, searched first, then the
/// bundled rows.
#[derive(Clone, Copy, Debug)]
pub struct PriceSnapshot<'a> {
    /// The project's override rows. The control plane fills them from milestone 4; until then there are none.
    pub project: Option<&'a PriceTable>,
    /// The bundled rows.
    pub bundled: &'a PriceTable,
}

impl<'a> PriceSnapshot<'a> {
    /// The row for `model` at `at`: the project's row when one applies, else the bundled row.
    pub fn find(&self, model: &str, at: Timestamp) -> Option<&'a ModelPrice> {
        self.project
            .and_then(|table| table.find(model, at))
            .or_else(|| self.bundled.find(model, at))
    }
}

impl ModelPrice {
    /// The first tier by ascending priority whose conditions all hold for the calculated `usage` and the span's
    /// `model_parameters` (a JSON object; any other text has no parameters).
    pub fn tier(&self, usage: &[UsageEntry], model_parameters: &[u8]) -> Option<&PriceTier> {
        let params = Params {
            text: model_parameters,
            members: OnceCell::new(),
        };
        self.tiers.iter().find(|tier| {
            tier.conditions.iter().all(|condition| match condition {
                Condition::Usage { key, op, value } => op.holds(units(usage, key), *value),
                Condition::Param { key, values } => params.is_one_of(key, values),
            })
        })
    }
}

impl PriceTier {
    /// The price of `key` in pico-USD per unit.
    pub fn price(&self, key: &str) -> Option<i64> {
        self.prices
            .binary_search_by(|(k, _)| (**k).cmp(key))
            .ok()
            .map(|i| self.prices[i].1)
    }

    /// Sets the cost of every usage entry and returns the total.
    ///
    /// `cache_read` and `cache_write` count inside `input`, `reasoning` inside `output`, and `input` and
    /// `output` inside `total` (contract 10.5), so these keys are parts, not additions. Every unit is priced once,
    /// by the most specific key that holds it and has a price: a key is priced on its units less the units priced
    /// within its parts, and the units of a part without a price stay in the key that holds it. Any other key is
    /// priced on its own units. A key's cost is `round_half_even(units * price / 1000)` nano-USD, exact and
    /// saturating, and the total is the saturating sum of the per-key costs.
    pub fn apply(&self, usage: &mut [UsageEntry]) -> NanoUsd {
        let at = |key: &str| units(usage, key);
        // A key's own units, and the units priced within it and its parts.
        let claim = |key: &str, parts: u64| {
            let own = at(key).saturating_sub(parts);
            let priced = if self.price(key).is_some() { own } else { 0 };
            (own, parts.saturating_add(priced))
        };
        let (cache_read, cache_read_claimed) = claim(UsageEntry::CACHE_READ, 0);
        let (cache_write, cache_write_claimed) = claim(UsageEntry::CACHE_WRITE, 0);
        let (reasoning, reasoning_claimed) = claim(UsageEntry::REASONING, 0);
        let (input, input_claimed) = claim(
            UsageEntry::INPUT,
            cache_read_claimed.saturating_add(cache_write_claimed),
        );
        let (output, output_claimed) = claim(UsageEntry::OUTPUT, reasoning_claimed);
        let (total, _) = claim(
            UsageEntry::TOTAL,
            input_claimed.saturating_add(output_claimed),
        );
        for entry in usage.iter_mut() {
            let own = match &*entry.key {
                UsageEntry::CACHE_READ => cache_read,
                UsageEntry::CACHE_WRITE => cache_write,
                UsageEntry::REASONING => reasoning,
                UsageEntry::INPUT => input,
                UsageEntry::OUTPUT => output,
                UsageEntry::TOTAL => total,
                _ => entry.units,
            };
            entry.cost = self
                .price(&entry.key)
                .map_or(NanoUsd::ZERO, |price| cost_nusd(own, price));
        }
        NanoUsd::saturating_sum(usage.iter().map(|e| e.cost))
    }
}

/// The cost of `units` at `price_pusd` pico-USD per unit (contract 3.4): `units * price_pusd / 1000` nano-USD,
/// computed exactly in 128-bit arithmetic, rounded half to even and saturated at [`NanoUsd::MAX`]. A negative
/// price counts as 0.
pub(crate) fn cost_nusd(units: u64, price_pusd: i64) -> NanoUsd {
    // Below 2^64 * 2^63, so the product fits.
    let product = u128::from(units) * u128::from(price_pusd.max(0).unsigned_abs());
    let (quotient, remainder) = (product / 1_000, product % 1_000);
    let round_up = remainder > 500 || (remainder == 500 && quotient % 2 == 1);
    NanoUsd::from_nanos(i64::try_from(quotient + u128::from(round_up)).unwrap_or(i64::MAX))
}

/// The units of `key` in `usage`; 0 when it is absent.
fn units(usage: &[UsageEntry], key: &str) -> u64 {
    usage.iter().find(|e| &*e.key == key).map_or(0, |e| e.units)
}

/// A span's model parameters, parsed on first use.
struct Params<'a> {
    text: &'a [u8],
    members: OnceCell<Option<Map<String, Value>>>,
}

impl Params<'_> {
    fn is_one_of(&self, key: &str, values: &[Box<str>]) -> bool {
        let members = self.members.get_or_init(|| {
            if self.text.is_empty() {
                None
            } else {
                serde_json::from_slice(self.text).ok()
            }
        });
        let found = |text: &str| values.iter().any(|v| **v == *text);
        match members.as_ref().and_then(|m| m.get(key)) {
            Some(Value::String(s)) => found(s),
            Some(Value::Number(n)) => found(&n.to_string()),
            Some(Value::Bool(b)) => found(if *b { "true" } else { "false" }),
            _ => false,
        }
    }
}

/// Validates one row and puts its tiers and prices in order.
fn check(model: &mut ModelPrice) -> Result<(), String> {
    if model.name.is_empty() {
        return Err("the name is empty".into());
    }
    if model.pattern.is_empty() || model.pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "the pattern must have 1 to {MAX_PATTERN_BYTES} bytes"
        ));
    }
    // A pattern that compiles on its own has balanced groups, so the anchoring group around it cannot change its
    // structure.
    RegexBuilder::new(&model.pattern)
        .case_insensitive(true)
        .size_limit(PATTERN_SIZE_LIMIT)
        .nest_limit(PATTERN_NEST_LIMIT)
        .build()
        .map_err(|e| format!("the pattern is rejected: {e}"))?;
    if model.tiers.is_empty() {
        return Err("the row has no tiers".into());
    }
    model.tiers.sort_by_key(|t| t.priority);
    if model
        .tiers
        .windows(2)
        .any(|w| w[0].priority == w[1].priority)
    {
        return Err("two tiers have the same priority".into());
    }
    let last = model.tiers.len() - 1;
    let mut names = BTreeSet::new();
    for (i, tier) in model.tiers.iter_mut().enumerate() {
        let name = &tier.name;
        if name.is_empty() || !names.insert(name.clone()) {
            return Err(format!("tier name {name:?} is empty or repeated"));
        }
        if tier.conditions.is_empty() && i != last {
            return Err(format!(
                "tier {name:?} has no conditions, so the tiers after it can never apply"
            ));
        }
        for condition in &tier.conditions {
            let ok = match condition {
                Condition::Usage { key, .. } => !key.is_empty(),
                Condition::Param { key, values } => {
                    !key.is_empty() && !values.is_empty() && values.iter().all(|v| !v.is_empty())
                }
            };
            if !ok {
                return Err(format!(
                    "tier {name:?} has a condition with an empty key or value"
                ));
            }
        }
        tier.prices.sort_by(|a, b| a.0.cmp(&b.0));
        if tier.prices.is_empty() {
            return Err(format!("tier {name:?} has no prices"));
        }
        if tier.prices.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(format!("tier {name:?} prices a usage key twice"));
        }
        if tier.prices.iter().any(|(k, p)| k.is_empty() || *p < 0) {
            return Err(format!(
                "tier {name:?} has an empty usage key or a negative price"
            ));
        }
    }
    Ok(())
}

/// `YYYY-MM-DD` as the first instant of that UTC day.
fn parse_date(text: &str) -> Option<Timestamp> {
    let bytes = text.as_bytes();
    let digits = |range: core::ops::Range<usize>| {
        bytes
            .get(range.clone())
            .filter(|d| d.iter().all(u8::is_ascii_digit))
            .and_then(|_| text.get(range))
    };
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    CivilDate::new(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?).map(CivilDate::start)
}

/// A version 8 UUID hashed from `parts`.
fn hashed_id(parts: &[&str]) -> Uuid {
    let mut text = Vec::new();
    for part in parts {
        text.extend_from_slice(part.as_bytes());
        text.push(0);
    }
    let mut bytes = xxh3_128(&text).to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// A row of the data file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRow {
    name: String,
    #[serde(rename = "match")]
    pattern: String,
    #[serde(default)]
    start: Option<String>,
    tiers: Vec<TierRow>,
}

/// A tier of the data file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TierRow {
    name: String,
    priority: u32,
    #[serde(default)]
    when: Vec<ConditionRow>,
    #[serde(deserialize_with = "price_list")]
    prices: Vec<(String, i64)>,
}

/// A condition of the data file.
#[derive(Deserialize)]
#[serde(untagged)]
enum ConditionRow {
    Usage(UsageRow),
    Param(ParamRow),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageRow {
    usage: String,
    op: Compare,
    value: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParamRow {
    param: String,
    #[serde(rename = "in")]
    values: Vec<String>,
}

impl ModelRow {
    fn into_model(self, start: Timestamp) -> ModelPrice {
        let start_text = start.micros().to_string();
        let tiers = self
            .tiers
            .into_iter()
            .map(|tier| PriceTier {
                id: hashed_id(&["tier", &self.name, &start_text, &tier.name]),
                name: tier.name.into(),
                priority: tier.priority,
                conditions: tier
                    .when
                    .into_iter()
                    .map(|c| match c {
                        ConditionRow::Usage(u) => Condition::Usage {
                            key: u.usage.into(),
                            op: u.op,
                            value: u.value,
                        },
                        ConditionRow::Param(p) => Condition::Param {
                            key: p.param.into(),
                            values: p.values.into_iter().map(Into::into).collect(),
                        },
                    })
                    .collect(),
                prices: tier
                    .prices
                    .into_iter()
                    .map(|(k, p)| (k.into(), p))
                    .collect(),
            })
            .collect();
        ModelPrice {
            id: hashed_id(&["model", &self.name, &start_text]),
            name: self.name.into(),
            pattern: self.pattern.into(),
            start,
            tiers,
        }
    }
}

/// A prices object kept as a list, so a key written twice reaches validation instead of overwriting.
fn price_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<(String, i64)>, D::Error> {
    struct Prices;
    impl<'de> Visitor<'de> for Prices {
        type Value = Vec<(String, i64)>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an object of integer prices")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(entry) = map.next_entry::<String, i64>()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_map(Prices)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use xxhash_rust::xxh3::xxh3_64;

    use super::*;

    /// 2026-11-17T09:30:00Z.
    const AT: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

    fn n(nanos: i64) -> NanoUsd {
        NanoUsd::from_nanos(nanos)
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

    fn costs(usage: &[UsageEntry]) -> Vec<(&str, i64)> {
        usage.iter().map(|e| (&*e.key, e.cost.nanos())).collect()
    }

    fn tier(prices: &[(&str, i64)]) -> PriceTier {
        PriceTier {
            id: hashed_id(&["test tier"]),
            name: "t".into(),
            priority: 1,
            conditions: Vec::new(),
            prices: prices.iter().map(|&(k, p)| (k.into(), p)).collect(),
        }
        .sorted()
    }

    impl PriceTier {
        fn sorted(mut self) -> Self {
            self.prices.sort_by(|a, b| a.0.cmp(&b.0));
            self
        }
    }

    fn table(json: &str) -> PriceTable {
        PriceTable::from_json(7, json).unwrap()
    }

    fn rejection(json: &str) -> String {
        PriceTable::from_json(7, json).unwrap_err().to_string()
    }

    /// One row named `name` matching `pattern`, with one unconditional tier.
    fn row(name: &str, pattern: &str, start: Option<&str>) -> String {
        let start = start.map_or(String::new(), |s| format!(r#""start": "{s}", "#));
        format!(
            r#"{{"name": "{name}", "match": "{pattern}", {start}"tiers": [{{"name": "Standard", "priority": 1, "prices": {{"input": 1000}}}}]}}"#
        )
    }

    fn date(text: &str) -> Timestamp {
        parse_date(text).unwrap()
    }

    #[test]
    fn the_bundled_version_is_the_hash_of_the_data_file() {
        let hash = xxh3_64(BUNDLED_PRICES.as_bytes()) as u32;
        assert_eq!(
            BUNDLED_PRICE_TABLE_VERSION, hash,
            "the data file changed: set BUNDLED_PRICE_TABLE_VERSION to {hash:#010x}"
        );
        assert_ne!(BUNDLED_PRICE_TABLE_VERSION, 0);
    }

    #[test]
    fn the_bundled_table_loads() {
        let bundled = PriceTable::bundled().unwrap();
        assert_eq!(bundled.version(), BUNDLED_PRICE_TABLE_VERSION);
        assert_eq!(bundled.models().len(), 189);
        assert_eq!(
            bundled
                .models()
                .iter()
                .map(|m| m.tiers.len())
                .sum::<usize>(),
            337
        );
        let normalized = [
            UsageEntry::INPUT,
            UsageEntry::OUTPUT,
            UsageEntry::TOTAL,
            UsageEntry::CACHE_READ,
            UsageEntry::CACHE_WRITE,
            UsageEntry::REASONING,
        ];
        for model in bundled.models() {
            // Every bundled row prices something: its last tier holds unconditionally.
            assert!(
                model.tiers.last().is_some_and(|t| t.conditions.is_empty()),
                "{}",
                model.name
            );
            assert_eq!(model.start, Timestamp::EPOCH, "{}", model.name);
            for tier in &model.tiers {
                for (key, _) in &tier.prices {
                    assert!(
                        normalized.contains(&&**key),
                        "{} {} {key}",
                        model.name,
                        tier.name
                    );
                }
            }
            for id in std::iter::once(model.id).chain(model.tiers.iter().map(|t| t.id)) {
                let bytes = id.to_bytes();
                assert_eq!((bytes[6] >> 4, bytes[8] >> 6), (8, 0b10), "{}", model.name);
            }
        }
        // Loading the same text again gives the same rows and ids.
        let again = PriceTable::from_json(BUNDLED_PRICE_TABLE_VERSION, BUNDLED_PRICES).unwrap();
        assert_eq!(again.models(), bundled.models());
    }

    #[test]
    fn the_kernel_rounds_half_to_even_and_saturates() {
        // (units, pico-USD per unit, nano-USD)
        let cases = [
            (1, 500, 0),
            (3, 500, 2),
            (5, 500, 2),
            (1, 1_500, 2),
            (1, 2_500, 2),
            (1, 499, 0),
            (1, 501, 1),
            (0, i64::MAX, 0),
            (1_000_000, 2_500_000, 2_500_000_000),
            (7, -5, 0),
            (u64::MAX, i64::MAX, i64::MAX),
            (u64::MAX, 1_000, i64::MAX),
            // The largest product below the top tie: 9_204_925_292_781_066_255.885 rounds up and fits.
            (u64::MAX, 499, 9_204_925_292_781_066_256),
            // Exactly i64::MAX + 0.5: the even neighbour does not fit, so the cost saturates.
            (u64::MAX, 500, i64::MAX),
        ];
        for (units, price, nanos) in cases {
            assert_eq!(cost_nusd(units, price), n(nanos), "{units} x {price}");
        }
    }

    #[test]
    fn usage_comparisons() {
        use Compare::*;
        let table = [
            (Gt, [false, false, true]),
            (Gte, [false, true, true]),
            (Lt, [true, false, false]),
            (Lte, [true, true, false]),
            (Eq, [false, true, false]),
            (Neq, [true, false, true]),
        ];
        for (op, expected) in table {
            assert_eq!(
                [op.holds(9, 10), op.holds(10, 10), op.holds(11, 10)],
                expected,
                "{op:?}"
            );
        }
    }

    #[test]
    fn parts_are_priced_once() {
        let tier = tier(&[
            ("input", 2_500_000),
            ("cache_read", 1_250_000),
            ("output", 10_000_000),
        ]);
        let mut u = usage(&[
            ("input", 1_000),
            ("cache_read", 400),
            ("cache_write", 100),
            ("output", 500),
            ("reasoning", 200),
            ("total", 1_500),
        ]);
        let total = tier.apply(&mut u);
        // The unpriced cache write stays in input, and the unpriced reasoning in output.
        assert_eq!(
            costs(&u),
            [
                ("cache_read", 500_000),
                ("cache_write", 0),
                ("input", 1_500_000),
                ("output", 5_000_000),
                ("reasoning", 0),
                ("total", 0),
            ]
        );
        assert_eq!(total, n(7_000_000));
    }

    #[test]
    fn a_total_price_covers_unpriced_input_and_output() {
        let tier = tier(&[("total", 4_000_000)]);
        let mut u = usage(&[("input", 100), ("output", 50), ("total", 150)]);
        assert_eq!(tier.apply(&mut u), n(600_000));
        assert_eq!(costs(&u), [("input", 0), ("output", 0), ("total", 600_000)]);
    }

    #[test]
    fn a_priced_part_of_an_unpriced_key_counts_against_total() {
        let tier = tier(&[("cache_read", 1_000), ("total", 4_000)]);
        let mut u = usage(&[
            ("input", 100),
            ("cache_read", 30),
            ("output", 50),
            ("total", 150),
        ]);
        assert_eq!(tier.apply(&mut u), n(30 + 480));
        assert_eq!(
            costs(&u),
            [
                ("cache_read", 30),
                ("input", 0),
                ("output", 0),
                ("total", 480)
            ]
        );
    }

    #[test]
    fn parts_larger_than_their_key_leave_it_nothing() {
        // A client that reports input without its cache reads.
        let tier = tier(&[("input", 2_000), ("cache_read", 1_000)]);
        let mut u = usage(&[("input", 100), ("cache_read", 400)]);
        assert_eq!(tier.apply(&mut u), n(400));
        assert_eq!(costs(&u), [("cache_read", 400), ("input", 0)]);
    }

    #[test]
    fn other_keys_are_priced_on_their_own_units() {
        let tier = tier(&[("input", 1_000), ("web_search", 10_000_000)]);
        let mut u = usage(&[("input", 10), ("web_search", 3), ("unpriced", 99)]);
        assert_eq!(tier.apply(&mut u), n(30_010));
        assert_eq!(
            costs(&u),
            [("input", 10), ("unpriced", 0), ("web_search", 30_000)]
        );
    }

    #[test]
    fn matching_is_whole_name_and_ignores_case() {
        let t = table(&format!(
            "[{}, {}]",
            row("four", "gpt-4o", None),
            row("either", "a|b", None)
        ));
        let name = |model: &str| t.find(model, AT).map(|m| &*m.name);
        assert_eq!(name("gpt-4o"), Some("four"));
        assert_eq!(name("GPT-4O"), Some("four"));
        assert_eq!(name("gpt-4o-mini"), None);
        assert_eq!(name("xgpt-4o"), None);
        // A top-level alternation is anchored as a whole.
        assert_eq!(name("a"), Some("either"));
        assert_eq!(name("b"), Some("either"));
        assert_eq!(name("ab"), None);
        assert_eq!(name(""), None);
        let any = table(&format!("[{}]", row("any", ".*", None)));
        assert!(any.find("", AT).is_none());
        assert!(any.find("x", AT).is_some());
    }

    #[test]
    fn the_greatest_start_at_or_before_the_span_applies() {
        let t = table(&format!(
            "[{}, {}, {}]",
            row("2025", "m", Some("2025-01-01")),
            row("2024", "m", Some("2024-01-01")),
            row("2027", "m", Some("2027-01-01")),
        ));
        let name = |at: Timestamp| t.find("m", at).map(|m| &*m.name);
        assert_eq!(name(AT), Some("2025"));
        assert_eq!(name(date("2025-01-01")), Some("2025"));
        assert_eq!(
            name(Timestamp::from_micros(date("2025-01-01").micros() - 1)),
            Some("2024")
        );
        assert_eq!(name(date("2027-06-01")), Some("2027"));
        assert_eq!(name(date("2023-12-31")), None);
    }

    #[test]
    fn ties_go_to_the_row_listed_first() {
        let t = table(&format!(
            "[{}, {}]",
            row("first", "m.*", None),
            row("second", "m", None)
        ));
        assert_eq!(t.find("m", AT).map(|m| &*m.name), Some("first"));
        let dated = table(&format!(
            "[{}, {}]",
            row("open", "m", None),
            row("dated", "m.*", Some("2020-01-01"))
        ));
        assert_eq!(dated.find("m", AT).map(|m| &*m.name), Some("dated"));
    }

    #[test]
    fn project_rows_come_before_bundled_rows() {
        let bundled = table(&format!(
            "[{}, {}]",
            row("bundled", "m", None),
            row("other", "n", None)
        ));
        let project = table(&format!(
            "[{}, {}]",
            row("override", "m", None),
            row("later", "n", Some("2030-01-01"))
        ));
        let snapshot = PriceSnapshot {
            project: Some(&project),
            bundled: &bundled,
        };
        let name = |model: &str| snapshot.find(model, AT).map(|m| &*m.name);
        assert_eq!(name("m"), Some("override"));
        // A project row that has not started yet leaves the bundled row in place.
        assert_eq!(name("n"), Some("other"));
        assert_eq!(name("z"), None);
        let none = PriceSnapshot {
            project: None,
            bundled: &bundled,
        };
        assert_eq!(none.find("m", AT).map(|m| &*m.name), Some("bundled"));
    }

    #[test]
    fn tiers_are_tried_in_priority_order() {
        let t = table(
            r#"[{"name": "m", "match": "m", "tiers": [
                {"name": "Standard", "priority": 9, "prices": {"input": 1}},
                {"name": "Long", "priority": 1, "when": [{"usage": "input", "op": "gt", "value": 100}], "prices": {"input": 2}},
                {"name": "Fast", "priority": 2, "when": [{"param": "service_tier", "in": ["fast", "priority"]}], "prices": {"input": 3}}
            ]}]"#,
        );
        let model = t.find("m", AT).unwrap();
        assert_eq!(
            model.tiers.iter().map(|t| t.priority).collect::<Vec<_>>(),
            [1, 2, 9]
        );
        let pick = |input: u64, params: &str| {
            &*model
                .tier(&usage(&[("input", input)]), params.as_bytes())
                .unwrap()
                .name
        };
        assert_eq!(pick(150, r#"{"service_tier": "priority"}"#), "Long");
        assert_eq!(pick(100, r#"{"service_tier": "priority"}"#), "Fast");
        assert_eq!(pick(100, r#"{"service_tier": "default"}"#), "Standard");
        assert_eq!(pick(100, ""), "Standard");
        assert_eq!(model.tier(&[], b"").map(|t| &*t.name), Some("Standard"));
    }

    #[test]
    fn param_conditions_read_top_level_values() {
        let t = table(
            r#"[{"name": "m", "match": "m", "tiers": [
                {"name": "A", "priority": 1, "when": [{"param": "service_tier", "in": ["priority"]}], "prices": {"input": 1}},
                {"name": "B", "priority": 2, "when": [{"param": "n", "in": ["2"]}, {"param": "stream", "in": ["true"]}], "prices": {"input": 1}}
            ]}]"#,
        );
        let model = t.find("m", AT).unwrap();
        let pick = |params: &str| model.tier(&[], params.as_bytes()).map(|t| &*t.name);
        assert_eq!(pick(r#"{"service_tier": "priority"}"#), Some("A"));
        assert_eq!(pick(r#"{"n": 2, "stream": true}"#), Some("B"));
        assert_eq!(pick(r#"{"n": 2, "stream": false}"#), None);
        assert_eq!(pick(r#"{"nested": {"service_tier": "priority"}}"#), None);
        assert_eq!(pick(r#"{"service_tier": null}"#), None);
        assert_eq!(pick(r#"{"service_tier": ["priority"]}"#), None);
        assert_eq!(pick(r#"["priority"]"#), None);
        assert_eq!(pick(r#"{"service_tier": "prior"#), None);
        assert_eq!(pick(""), None);
    }

    #[test]
    fn the_format_rejects_what_it_does_not_define() {
        let tier = r#"{"name": "S", "priority": 1, "prices": {"input": 1}}"#;
        let cases = [
            (r#"{"rows": []}"#.to_owned(), "does not parse"),
            (format!(r#"[{{"name": "m", "match": "m", "tiers": [{tier}], "extra": 1}}]"#), "unknown field"),
            (format!(r#"[{{"name": "m", "match": "m", "start": "2024-02-30", "tiers": [{tier}]}}]"#), "YYYY-MM-DD"),
            (format!(r#"[{{"name": "m", "match": "m", "start": "2024-1-01", "tiers": [{tier}]}}]"#), "YYYY-MM-DD"),
            (r#"[{"name": "m", "match": "m", "tiers": [{"name": "S", "priority": 1, "prices": {"input": 1.5}}]}]"#.to_owned(), "does not parse"),
            (r#"[{"name": "m", "match": "m", "tiers": [{"name": "S", "priority": 1, "when": [{"usage": "input", "op": "above", "value": 1}], "prices": {"input": 1}}]}]"#.to_owned(), "does not parse"),
        ];
        for (json, reason) in cases {
            let error = rejection(&json);
            assert!(error.contains(reason), "{json}: {error}");
        }
    }

    #[test]
    fn rows_that_break_a_rule_are_rejected() {
        let tiers = |tiers: &str| format!(r#"[{{"name": "m", "match": "m", "tiers": [{tiers}]}}]"#);
        let pattern = |p: &str| {
            format!(
                r#"[{{"name": "m", "match": {}, "tiers": [{{"name": "S", "priority": 1, "prices": {{"input": 1}}}}]}}]"#,
                serde_json::to_string(p).unwrap()
            )
        };
        let s = r#"{"name": "S", "priority": 1, "prices": {"input": 1}}"#;
        let cases = [
            (pattern(""), "1 to 1024 bytes"),
            (
                pattern(&"a".repeat(MAX_PATTERN_BYTES + 1)),
                "1 to 1024 bytes",
            ),
            (pattern("("), "pattern is rejected"),
            // An unbalanced pattern cannot escape the anchoring group.
            (pattern("a)|(b"), "pattern is rejected"),
            (
                pattern(&format!("{}a{}", "(".repeat(40), ")".repeat(40))),
                "pattern is rejected",
            ),
            (pattern(r"(?:\w{100}){100}"), "pattern is rejected"),
            (
                r#"[{"name": "", "match": "m", "tiers": []}]"#.to_owned(),
                "name is empty",
            ),
            (tiers(""), "no tiers"),
            (
                tiers(
                    r#"{"name": "S", "priority": 1, "when": [{"usage": "input", "op": "gt", "value": 1}], "prices": {"input": 1}}, {"name": "S", "priority": 2, "prices": {"input": 1}}"#,
                ),
                "empty or repeated",
            ),
            (
                tiers(r#"{"name": "", "priority": 1, "prices": {"input": 1}}"#),
                "empty or repeated",
            ),
            (
                tiers(&format!(
                    r#"{s}, {{"name": "T", "priority": 1, "when": [{{"usage": "input", "op": "gt", "value": 1}}], "prices": {{"input": 1}}}}"#
                )),
                "same priority",
            ),
            (
                tiers(&format!(
                    r#"{s}, {{"name": "T", "priority": 2, "when": [{{"usage": "input", "op": "gt", "value": 1}}], "prices": {{"input": 1}}}}"#
                )),
                "can never apply",
            ),
            (
                tiers(r#"{"name": "S", "priority": 1, "prices": {}}"#),
                "no prices",
            ),
            (
                tiers(r#"{"name": "S", "priority": 1, "prices": {"input": 1, "input": 2}}"#),
                "twice",
            ),
            (
                tiers(r#"{"name": "S", "priority": 1, "prices": {"input": -1}}"#),
                "negative price",
            ),
            (
                tiers(r#"{"name": "S", "priority": 1, "prices": {"": 1}}"#),
                "empty usage key",
            ),
            (
                tiers(
                    r#"{"name": "S", "priority": 1, "when": [{"usage": "", "op": "gt", "value": 1}], "prices": {"input": 1}}"#,
                ),
                "empty key or value",
            ),
            (
                tiers(
                    r#"{"name": "S", "priority": 1, "when": [{"param": "p", "in": []}], "prices": {"input": 1}}"#,
                ),
                "empty key or value",
            ),
            (
                tiers(
                    r#"{"name": "S", "priority": 1, "when": [{"param": "p", "in": [""]}], "prices": {"input": 1}}"#,
                ),
                "empty key or value",
            ),
            (
                format!("[{}, {}]", row("m", "a", None), row("m", "b", None)),
                "same name and start",
            ),
        ];
        for (json, reason) in cases {
            let error = rejection(&json);
            assert!(error.contains(reason), "{json}: {error}");
        }
        // The same name with another start is a separate row.
        assert_eq!(
            table(&format!(
                "[{}, {}]",
                row("m", "a", None),
                row("m", "a", Some("2025-01-01"))
            ))
            .models()
            .len(),
            2
        );
    }

    /// A value below `2^bits` whose magnitude is itself random: a random word cut to a random width.
    fn below(bits: u32) -> impl Strategy<Value = u64> {
        (any::<u64>(), 0..=bits).prop_map(|(word, width)| word.checked_shr(64 - width).unwrap_or(0))
    }

    /// A tier that prices a random subset of the normalized keys and one other key.
    fn any_tier() -> impl Strategy<Value = PriceTier> {
        let keys = [
            "input",
            "output",
            "total",
            "cache_read",
            "cache_write",
            "reasoning",
            "other",
        ];
        proptest::collection::vec(proptest::option::of(0i64..20_000_000), keys.len()).prop_map(
            move |prices| {
                let prices: Vec<(&str, i64)> = keys
                    .iter()
                    .zip(prices)
                    .filter_map(|(k, p)| p.map(|p| (*k, p)))
                    .collect();
                tier(&prices)
            },
        )
    }

    /// Usage as stage A builds it: parts inside their keys, and `total` as input plus output.
    fn any_usage() -> impl Strategy<Value = (u64, u64, u64, u64, u64, u64)> {
        (
            0u64..1 << 40,
            0u64..1 << 40,
            0u64..1 << 40,
            0u64..1 << 40,
            0u64..1 << 40,
            0u64..1 << 40,
        )
    }

    fn consistent(
        (cache_read, cache_write, input_rest, reasoning, output_rest, other): (
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
        ),
    ) -> Vec<UsageEntry> {
        let input = cache_read + cache_write + input_rest;
        let output = reasoning + output_rest;
        usage(&[
            ("cache_read", cache_read),
            ("cache_write", cache_write),
            ("input", input),
            ("reasoning", reasoning),
            ("output", output),
            ("total", input + output),
            ("other", other),
        ])
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Exact where the cost fits: the integer nearest to `units * price / 1000`, the even one on a tie.
        /// Products in this domain stay below 2^72, so no case saturates.
        #[test]
        fn the_kernel_is_exact(units in below(40), price in below(32)) {
            let price = i64::try_from(price).unwrap();
            let exact = i128::from(units) * i128::from(price);
            let cost = i128::from(cost_nusd(units, price).nanos());
            let error = cost * 1_000 - exact;
            prop_assert!(error.abs() <= 500, "{units} x {price}: {cost}");
            if error.abs() == 500 {
                prop_assert_eq!(cost % 2, 0, "{} x {}: {}", units, price, cost);
            }
        }

        /// `(2k + 1) * 500 / 1000` is `k + 0.5`, which rounds to the even one of `k` and `k + 1`.
        #[test]
        fn the_kernel_rounds_ties_to_even(k in 0u64..1 << 40) {
            let even = if k % 2 == 0 { k } else { k + 1 };
            prop_assert_eq!(cost_nusd(2 * k + 1, 500), n(i64::try_from(even).unwrap()));
        }

        /// Products in this domain are at least 2^84, far beyond `i64::MAX * 1000`.
        #[test]
        fn the_kernel_saturates(units in (1u64 << 44)..=u64::MAX, price in (1i64 << 40)..=i64::MAX) {
            prop_assert_eq!(cost_nusd(units, price), NanoUsd::MAX);
        }

        /// Units and prices of every magnitude, so pairs fall on both sides of the saturation boundary.
        #[test]
        fn the_kernel_is_monotone_in_units(a in below(64), b in below(64), price in below(40)) {
            let price = i64::try_from(price).unwrap();
            let (low, high) = (a.min(b), a.max(b));
            prop_assert!(cost_nusd(low, price) <= cost_nusd(high, price));
        }

        /// Splitting units between keys of one price moves the cost by at most one nano-USD.
        #[test]
        fn the_kernel_splits_within_rounding(a in 0u64..1 << 32, b in 0u64..1 << 32, price in 0i64..1 << 30) {
            let cost = |units| i128::from(cost_nusd(units, price).nanos());
            prop_assert!((cost(a + b) - cost(a) - cost(b)).abs() <= 1);
        }

        #[test]
        fn the_total_is_the_sum_of_the_key_costs(tier in any_tier(), parts in any_usage()) {
            let mut u = consistent(parts);
            let total = tier.apply(&mut u);
            prop_assert_eq!(total, NanoUsd::saturating_sum(u.iter().map(|e| e.cost)));
            for entry in &u {
                prop_assert!(entry.cost >= NanoUsd::ZERO);
                if tier.price(&entry.key).is_none() {
                    prop_assert_eq!(entry.cost, NanoUsd::ZERO);
                }
            }
        }

        /// Parts priced like the key that holds them cost what the key alone would: no unit is priced twice.
        #[test]
        fn parts_at_their_key_price_cost_nothing_extra(input in 0i64..20_000_000, output in 0i64..20_000_000, parts in any_usage()) {
            let plain = tier(&[("input", input), ("output", output)]);
            let split = tier(&[("input", input), ("cache_read", input), ("cache_write", input), ("output", output), ("reasoning", output)]);
            let (mut a, mut b) = (consistent(parts), consistent(parts));
            let (a, b) = (plain.apply(&mut a).nanos(), split.apply(&mut b).nanos());
            prop_assert!((a - b).abs() <= 3, "{a} {b}");
        }

        /// With the parts held, more input or output never lowers the cost.
        #[test]
        fn more_input_or_output_never_costs_less(tier in any_tier(), parts in any_usage(), extra in 0u64..1 << 40, which in any::<bool>()) {
            let (cache_read, cache_write, input_rest, reasoning, output_rest, other) = parts;
            let more = if which {
                (cache_read, cache_write, input_rest + extra, reasoning, output_rest, other)
            } else {
                (cache_read, cache_write, input_rest, reasoning, output_rest + extra, other)
            };
            let (mut a, mut b) = (consistent(parts), consistent(more));
            prop_assert!(tier.apply(&mut a) <= tier.apply(&mut b));
        }
    }
}
