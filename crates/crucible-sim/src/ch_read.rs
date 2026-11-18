//! The read and maintenance side of the simulated ClickHouse (contract section 2.6, `ChModel`): a typed-plan
//! interpreter over the rows that landed, `SimpleAggregateFunction` merges on a seeded schedule, lightweight
//! deletes, mask application and partition drops.
//!
//! - **Layout.** The landed blocks of [`crate::ch_model`] stay as they are: the ledger the write-side oracles
//!   read. This module keeps the store's own view of them: per table a list of stored parts whose rows are
//!   typed [`Value`]s, in `TableDef` column order. A merge replaces several parts of one partition with one,
//!   and on an aggregating table combines the rows that share a sort key through each column's merge function
//!   (`min`, `max`, `sum`, `sumMap`, `groupUniqArrayArray`). Reads never use `FINAL`: an aggregating table is
//!   only reachable through `Aggregate`, whose measures re-aggregate the partial rows, so an answer is the same
//!   whether or not a merge ran.
//! - **Interpreter.** [`interpret`] computes what the statement of `crucible-store-clickhouse`'s renderer
//!   computes on a real server: the project predicate and the window predicates on every table reference,
//!   three-valued predicate logic, the keyset cursor as a tuple comparison, `ORDER BY` with nulls last, the
//!   server's `UUID` order (second half first), `sumMap` without zero sums, `argMax` that skips null arguments,
//!   and the semi-join id cap. Where the renderer refuses a plan (a zero-filled time series), so does the model.
//!   Distinct counts are exact.
//! - **Deletes.** A lightweight delete hides its rows from every later read at once and leaves a mask on the
//!   parts it touched; `APPLY DELETED MASK` and merges remove masked rows for good. A partition drop removes
//!   the partition's parts, markers included, and its deduplication entries.
//!
//! The differential check of `crucible-conformance` runs the same plans and maintenance calls against this
//! model and real servers.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crucible_chfmt::{DecodedBlock, Values};
use crucible_env::{MutationId, MutationStatus, QueryError, Rng};
use crucible_plan::{
    AggOutput, Aggregate, CmpOp, DeletePlan, DeleteTarget, Direction, FieldRange, Fill, Literal,
    Measure, Node, PointLookup, Pred, QueryPlan, Row, Scan, SemiJoin, Step, TextSearch, TimeSeries,
    TokenMode, Value,
};
use crucible_records::{
    CivilDate, PartitionId, ProjectId, TableId, TimeWindow, Timestamp, Uuid, truncate_str,
};
use crucible_schema::{AggFn, ColumnRef, IndexKind, TableDef, TableKind, Tokenizer, Ty, table};

use crate::ch_model::Part;

/// The byte cap of one `metadata_kv` item (contract 3.1), as the renderer applies it.
const METADATA_KV_CAP: usize = 1_024;
/// The gram length of the text index of `name`.
const NAME_NGRAM: usize = 3;
/// How often, per million, a merge follows newly landed parts unless [`ReadSide::merge_ppm`] says otherwise.
const DEFAULT_MERGE_PPM: u32 = 300_000;

/// The rows a plan returns, grouped by what the plan's order defines.
///
/// A plan with an order gives one run per distinct order key, in order; the rows inside a run tie and come in
/// no defined order. A plan without an order gives one run. When `cut` is set a limit removed rows, so which
/// rows of the last run were kept is undefined too.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Answer {
    /// The runs of rows that tie on the plan's order.
    pub runs: Vec<Vec<Row>>,
    /// Whether a limit removed rows.
    pub cut: bool,
}

impl Answer {
    /// Every row, run after run.
    pub fn into_rows(self) -> Vec<Row> {
        self.runs.into_iter().flatten().collect()
    }
}

type Cells = Vec<Value>;

/// One part as the store holds it.
#[derive(Debug)]
struct Stored {
    partition: PartitionId,
    rows: Vec<Cells>,
    /// The delete mask: one flag per row.
    deleted: Vec<bool>,
    /// The model time until which the part still counts as masked although its masked rows are gone: a mask
    /// application rewrites the part in the background, and the old part stays active until then.
    mask_until: u64,
}

impl Stored {
    fn masked(&self, now: u64) -> bool {
        self.deleted.iter().any(|d| *d) || now < self.mask_until
    }

    fn visible(&self) -> impl Iterator<Item = &Cells> {
        self.rows
            .iter()
            .zip(&self.deleted)
            .filter(|(_, deleted)| !**deleted)
            .map(|(row, _)| row)
    }
}

#[derive(Debug, Default)]
struct StoredTable {
    /// How many landed parts of the ledger this view holds.
    absorbed: usize,
    parts: Vec<Stored>,
    /// Whether a delete or a partition drop ever changed the table, so that the ledger no longer tells what
    /// the store holds.
    touched: bool,
    /// Mutations by name, with the model time at which each is done.
    mutations: BTreeMap<String, u64>,
    /// A landed block that could not be read as rows of the table.
    broken: Option<String>,
}

/// The store's view of every table.
#[derive(Debug)]
pub(crate) struct ReadSide {
    tables: BTreeMap<TableId, StoredTable>,
    /// How often, per million, a merge follows newly landed parts.
    pub(crate) merge_ppm: u32,
    next_mutation: u64,
    /// Merges done.
    pub(crate) merges: u64,
}

impl Default for ReadSide {
    fn default() -> Self {
        Self {
            tables: BTreeMap::new(),
            merge_ppm: DEFAULT_MERGE_PPM,
            next_mutation: 0,
            merges: 0,
        }
    }
}

fn cell(values: &Values, ty: &Ty, row: usize) -> Option<Value> {
    Some(match (ty, values) {
        (Ty::Saf(_, inner), _) => return cell(values, inner, row),
        (Ty::UInt8, Values::UInt8(v)) => Value::UInt(u64::from(*v.get(row)?)),
        (Ty::UInt16, Values::UInt16(v)) => Value::UInt(u64::from(*v.get(row)?)),
        (Ty::UInt32, Values::UInt32(v)) => Value::UInt(u64::from(*v.get(row)?)),
        (Ty::UInt64, Values::UInt64(v)) => Value::UInt(*v.get(row)?),
        (Ty::Int32, Values::Int32(v)) => Value::Int(i64::from(*v.get(row)?)),
        (Ty::Int64, Values::Int64(v)) => Value::Int(*v.get(row)?),
        (Ty::Float32, Values::Float32(v)) => Value::Float(f64::from(f32::from_bits(*v.get(row)?))),
        (Ty::Float64, Values::Float64(v)) => Value::Float(f64::from_bits(*v.get(row)?)),
        (Ty::String | Ty::LowCardinalityString, Values::String(v)) => {
            Value::string(v.get(row)?.clone())
        }
        (Ty::FixedString(_), Values::FixedString(_, v)) => Value::Bytes(v.get(row)?.clone()),
        (Ty::Uuid, Values::Uuid(v)) => Value::Uuid(Uuid::from_bytes(*v.get(row)?)),
        (Ty::Date, Values::UInt16(v)) => {
            Value::Date(CivilDate::from_day_number(i64::from(*v.get(row)?)))
        }
        (Ty::DateTime, Values::UInt32(v)) => {
            Value::Time(Timestamp::from_micros(i64::from(*v.get(row)?) * 1_000_000))
        }
        (Ty::DateTime64, Values::Int64(v)) => Value::Time(Timestamp::from_micros(*v.get(row)?)),
        (Ty::Enum8(_), Values::Int8(v)) => Value::Enum(*v.get(row)? as u8),
        (Ty::Nullable(inner), Values::Nullable(nulls, present)) => {
            if *nulls.get(row)? {
                Value::Null
            } else {
                return cell(present, inner, row);
            }
        }
        (Ty::Array(element), Values::Array(ends, items)) => {
            let end = usize::try_from(*ends.get(row)?).ok()?;
            let start = match row.checked_sub(1) {
                Some(before) => usize::try_from(*ends.get(before)?).ok()?,
                None => 0,
            };
            Value::Array(
                (start..end)
                    .map(|at| cell(items, element, at))
                    .collect::<Option<Vec<_>>>()?,
            )
        }
        (Ty::Tuple2(a, b), Values::Tuple(parts)) => match parts.as_slice() {
            [first, second] => Value::Tuple2(
                Box::new(cell(first, a, row)?),
                Box::new(cell(second, b, row)?),
            ),
            _ => return None,
        },
        _ => return None,
    })
}

fn rows_of(def: &TableDef, block: &DecodedBlock) -> Option<Vec<Cells>> {
    if block.columns.len() != def.columns.len() {
        return None;
    }
    (0..block.rows)
        .map(|row| {
            def.columns
                .iter()
                .zip(&block.columns)
                .map(|(column, sent)| cell(&sent.values, &column.ty, row))
                .collect()
        })
        .collect()
}

fn bytes_of(value: &Value) -> Option<&[u8]> {
    match value {
        Value::String(s) => Some(s.as_bytes()),
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}

/// The server orders a `UUID` by its second half, then its first.
fn uuid_key(uuid: &Uuid) -> (u64, u64) {
    let bytes = uuid.to_bytes();
    let word = |from: usize| {
        let mut half = [0u8; 8];
        half.copy_from_slice(&bytes[from..from + 8]);
        u64::from_be_bytes(half)
    };
    (word(8), word(0))
}

/// The order of two values of one type; `None` when either is null or they do not compare.
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::UInt(x), Value::UInt(y)) => Some(x.cmp(y)),
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => Some(uuid_key(x).cmp(&uuid_key(y))),
        (Value::Date(x), Value::Date(y)) => Some(x.day_number().cmp(&y.day_number())),
        (Value::Time(x), Value::Time(y)) => Some(x.micros().cmp(&y.micros())),
        (Value::Enum(x), Value::Enum(y)) => Some((*x as i8).cmp(&(*y as i8))),
        (Value::Array(x), Value::Array(y)) => {
            for (left, right) in x.iter().zip(y) {
                match compare(left, right)? {
                    Ordering::Equal => {}
                    other => return Some(other),
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        (Value::Tuple2(a1, a2), Value::Tuple2(b1, b2)) => match compare(a1, b1)? {
            Ordering::Equal => compare(a2, b2),
            other => Some(other),
        },
        _ => Some(bytes_of(a)?.cmp(bytes_of(b)?)),
    }
}

fn equal(a: &Value, b: &Value) -> bool {
    compare(a, b) == Some(Ordering::Equal)
}

/// `ORDER BY` on one value: nulls come last in either direction.
fn sort_order(a: &Value, b: &Value, direction: Direction) -> Ordering {
    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let order = compare(a, b).unwrap_or(Ordering::Equal);
            match direction {
                Direction::Asc => order,
                Direction::Desc => order.reverse(),
            }
        }
    }
}

/// A byte image of a value under which equal values, and only those, are equal: the key of groups and sets.
fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0),
        Value::UInt(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Int(v) => {
            out.push(2);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Value::Float(v) => {
            out.push(3);
            out.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        Value::String(_) | Value::Bytes(_) => {
            let bytes = bytes_of(value).unwrap_or_default();
            out.push(4);
            out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        Value::Uuid(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_bytes());
        }
        Value::Date(v) => {
            out.push(6);
            out.extend_from_slice(&v.day_number().to_be_bytes());
        }
        Value::Time(v) => {
            out.push(7);
            out.extend_from_slice(&v.micros().to_be_bytes());
        }
        Value::Enum(v) => out.extend_from_slice(&[8, *v]),
        Value::Array(items) => {
            out.push(9);
            out.extend_from_slice(&(items.len() as u64).to_be_bytes());
            items.iter().for_each(|item| encode(item, out));
        }
        Value::Tuple2(a, b) => {
            out.push(10);
            encode(a, out);
            encode(b, out);
        }
    }
}

fn key_of<'a>(values: impl IntoIterator<Item = &'a Value>) -> Vec<u8> {
    let mut out = Vec::new();
    values.into_iter().for_each(|value| encode(value, &mut out));
    out
}

fn literal(value: &Literal) -> Value {
    match value {
        Literal::Uuid(v) => Value::Uuid(*v),
        Literal::Bytes(v) => Value::Bytes(v.clone()),
        Literal::String(v) => Value::String(v.clone()),
        Literal::UInt(v) => Value::UInt(*v),
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::Time(v) => Value::Time(*v),
        Literal::Date(v) => Value::Date(*v),
        Literal::Enum(v) => Value::Enum(*v),
    }
}

fn internal(what: impl Into<String>) -> QueryError {
    QueryError::Internal(what.into())
}

fn index(def: &TableDef, name: &str) -> Result<usize, QueryError> {
    def.columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| internal(format!("{} has no column {name}", def.name())))
}

fn indexes(def: &TableDef, columns: &[ColumnRef]) -> Result<Vec<usize>, QueryError> {
    columns.iter().map(|c| index(def, c.name())).collect()
}

fn holds(op: CmpOp, order: Ordering) -> bool {
    match op {
        CmpOp::Eq => order.is_eq(),
        CmpOp::Ne => order.is_ne(),
        CmpOp::Lt => order.is_lt(),
        CmpOp::Le => order.is_le(),
        CmpOp::Gt => order.is_gt(),
        CmpOp::Ge => order.is_ge(),
    }
}

fn text_of(value: &Value) -> String {
    String::from_utf8_lossy(bytes_of(value).unwrap_or_default()).into_owned()
}

/// The `n`-grams of a string, over code points.
fn ngrams(text: &str, n: usize) -> BTreeSet<Vec<char>> {
    let chars: Vec<char> = text.chars().collect();
    chars.windows(n).map(<[char]>::to_vec).collect()
}

/// The tokens the text index of a column holds for one value.
fn tokens(value: &Value, tokenizer: Tokenizer, lower: bool) -> BTreeSet<Vec<u8>> {
    let prepare = |bytes: &[u8]| {
        if lower {
            bytes.to_ascii_lowercase()
        } else {
            bytes.to_vec()
        }
    };
    match (value, tokenizer) {
        (Value::Array(items), _) => items.iter().filter_map(bytes_of).map(prepare).collect(),
        (_, Tokenizer::SplitByNonAlpha) => prepare(bytes_of(value).unwrap_or_default())
            .split(|byte| byte.is_ascii() && !byte.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
        (_, Tokenizer::Ngrams(n)) => {
            let text =
                String::from_utf8_lossy(&prepare(bytes_of(value).unwrap_or_default())).into_owned();
            ngrams(&text, usize::from(n))
                .into_iter()
                .map(|gram| gram.into_iter().collect::<String>().into_bytes())
                .collect()
        }
        (_, Tokenizer::Array) => BTreeSet::from([prepare(bytes_of(value).unwrap_or_default())]),
    }
}

/// A predicate with its columns resolved to positions and its literals to values.
#[derive(Debug)]
enum Test {
    And(Vec<Test>),
    Or(Vec<Test>),
    Not(Box<Test>),
    Cmp(usize, CmpOp, Value),
    In(usize, Vec<Value>),
    StartsWith(usize, Vec<u8>),
    HasAny(usize, Vec<Value>),
    Has(usize, Vec<u8>),
    TextMatch(usize, String),
}

impl Test {
    fn of(def: &TableDef, pred: &Pred) -> Result<Self, QueryError> {
        let all = |operands: &[Pred]| -> Result<Vec<Self>, QueryError> {
            operands.iter().map(|p| Self::of(def, p)).collect()
        };
        Ok(match pred {
            Pred::And(operands) => Self::And(all(operands)?),
            Pred::Or(operands) => Self::Or(all(operands)?),
            Pred::Not(operand) => Self::Not(Box::new(Self::of(def, operand)?)),
            Pred::Cmp(column, op, value) => {
                Self::Cmp(index(def, column.name())?, *op, literal(value))
            }
            Pred::In(column, values) => Self::In(
                index(def, column.name())?,
                values.iter().map(literal).collect(),
            ),
            Pred::StartsWith(column, prefix) => {
                Self::StartsWith(index(def, column.name())?, prefix.as_bytes().to_vec())
            }
            Pred::HasAny(column, values) => Self::HasAny(
                index(def, column.name())?,
                values.iter().map(literal).collect(),
            ),
            Pred::MetadataEq { key, value } => {
                let item = format!("{key}={value}");
                Self::Has(
                    index(def, "metadata_kv")?,
                    truncate_str(&item, METADATA_KV_CAP).as_bytes().to_vec(),
                )
            }
            Pred::TextMatch(needle) => Self::TextMatch(index(def, "name")?, needle.clone()),
        })
    }

    /// SQL's three-valued logic: `None` is unknown, which a `WHERE` treats as false.
    fn eval(&self, row: &Cells) -> Option<bool> {
        match self {
            Self::And(operands) => {
                let mut unknown = false;
                for operand in operands {
                    match operand.eval(row) {
                        Some(false) => return Some(false),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                (!unknown).then_some(true)
            }
            Self::Or(operands) => {
                let mut unknown = false;
                for operand in operands {
                    match operand.eval(row) {
                        Some(true) => return Some(true),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                (!unknown).then_some(false)
            }
            Self::Not(operand) => operand.eval(row).map(|v| !v),
            Self::Cmp(at, op, value) => compare(row.get(*at)?, value).map(|o| holds(*op, o)),
            Self::In(at, values) => {
                let found = row.get(*at)?;
                if matches!(found, Value::Null) {
                    return None;
                }
                Some(values.iter().any(|value| equal(found, value)))
            }
            Self::StartsWith(at, prefix) => Some(bytes_of(row.get(*at)?)?.starts_with(prefix)),
            Self::HasAny(at, values) => match row.get(*at)? {
                Value::Array(items) => Some(
                    items
                        .iter()
                        .any(|item| values.iter().any(|value| equal(item, value))),
                ),
                _ => None,
            },
            Self::Has(at, wanted) => match row.get(*at)? {
                Value::Array(items) => Some(
                    items
                        .iter()
                        .any(|item| bytes_of(item) == Some(wanted.as_slice())),
                ),
                _ => None,
            },
            Self::TextMatch(at, needle) => {
                let name = text_of(row.get(*at)?);
                // A case-insensitive substring match, and for a needle of at least one gram the test the text
                // index answers: every gram of the lowered needle is a gram of the lowered name. The index
                // lowers ASCII letters only.
                let substring = name.to_lowercase().contains(&needle.to_lowercase());
                let grams = needle.chars().count() < NAME_NGRAM || {
                    let held = ngrams(&name.to_ascii_lowercase(), NAME_NGRAM);
                    ngrams(&needle.to_ascii_lowercase(), NAME_NGRAM)
                        .iter()
                        .all(|gram| held.contains(gram))
                };
                Some(substring && grams)
            }
        }
    }

    fn passes(&self, row: &Cells) -> bool {
        self.eval(row) == Some(true)
    }
}

/// The longest literal `In` list of a predicate, which the renderer lets the set-size cap admit.
fn longest_in(pred: &Pred) -> usize {
    match pred {
        Pred::And(operands) | Pred::Or(operands) => {
            operands.iter().map(longest_in).max().unwrap_or(0)
        }
        Pred::Not(operand) => longest_in(operand),
        Pred::In(_, values) => values.len(),
        _ => 0,
    }
}

/// The window predicates of one table reference, as the renderer writes them.
#[derive(Debug)]
struct Window {
    /// `low <= column`, and `column < high` or, when the flag is set, `column <= high`.
    filter: Vec<(usize, Value, Value, bool)>,
    /// After grouping: `max(time) >= from` and, when the table has it, `min(first_seen) < to`.
    grouped: Option<(usize, Option<usize>)>,
    window: TimeWindow,
}

impl Window {
    fn of(def: &TableDef, window: TimeWindow) -> Result<Self, QueryError> {
        let last = window.to().saturating_add_micros(-1);
        let minutes = (
            Value::Time(window.from().floor_minute()),
            Value::Time(last.floor_minute()),
        );
        let days = (Value::Date(window.from().date()), Value::Date(last.date()));
        let time = def.retention.time_column;
        let at = index(def, time)?;
        let ty = def.columns[at].ty;
        let (mut filter, mut grouped) = (Vec::new(), None);
        match ty {
            Ty::DateTime64 => filter.push((
                at,
                Value::Time(window.from()),
                Value::Time(window.to()),
                false,
            )),
            Ty::DateTime => filter.push((at, minutes.0.clone(), minutes.1.clone(), true)),
            Ty::Date => filter.push((at, days.0.clone(), days.1.clone(), true)),
            Ty::Saf(_, &Ty::DateTime64) => {
                grouped = Some((at, index(def, "first_seen").ok()));
            }
            _ => {
                return Err(internal(format!(
                    "table {} has no window predicate",
                    def.name()
                )));
            }
        }
        if time != "start_minute"
            && let Ok(at) = index(def, "start_minute")
        {
            filter.push((at, minutes.0, minutes.1, true));
        }
        if time != "target_day"
            && let Ok(at) = index(def, "target_day")
        {
            filter.push((at, days.0, days.1, true));
        }
        Ok(Self {
            filter,
            grouped,
            window,
        })
    }

    fn passes(&self, row: &Cells) -> bool {
        self.filter.iter().all(|(at, low, high, inclusive)| {
            let Some(found) = row.get(*at) else {
                return false;
            };
            let above = compare(found, low).is_some_and(Ordering::is_ge);
            let below = compare(found, high)
                .is_some_and(|o| if *inclusive { o.is_le() } else { o.is_lt() });
            above && below
        })
    }

    fn group_passes(&self, rows: &[&Cells]) -> bool {
        let Some((time, first_seen)) = self.grouped else {
            return true;
        };
        let latest = extreme(rows, time, Ordering::Greater);
        let seen = compare(&latest, &Value::Time(self.window.from())).is_some_and(Ordering::is_ge);
        let started = first_seen.is_none_or(|at| {
            let first = extreme(rows, at, Ordering::Less);
            compare(&first, &Value::Time(self.window.to())).is_some_and(Ordering::is_lt)
        });
        seen && started
    }
}

/// The minimum (`Less`) or maximum (`Greater`) of a column over rows, nulls skipped; null when all are null.
fn extreme(rows: &[&Cells], at: usize, keep: Ordering) -> Value {
    let mut best: Option<&Value> = None;
    for row in rows {
        let Some(found) = row.get(at).filter(|v| !matches!(v, Value::Null)) else {
            continue;
        };
        if best.is_none_or(|b| compare(found, b) == Some(keep)) {
            best = Some(found);
        }
    }
    best.cloned().unwrap_or(Value::Null)
}

/// Adds one `(keys, counts)` tuple to a histogram.
fn add_histogram(sums: &mut BTreeMap<u64, u64>, value: &Value) {
    let Value::Tuple2(keys, counts) = value else {
        return;
    };
    let (Value::Array(keys), Value::Array(counts)) = (&**keys, &**counts) else {
        return;
    };
    for (key, count) in keys.iter().zip(counts) {
        if let (Value::UInt(key), Value::UInt(count)) = (key, count) {
            let sum = sums.entry(*key).or_default();
            *sum = sum.wrapping_add(*count);
        }
    }
}

/// The result of `sumMap`: keys ascending and unique, and no key whose sum is zero.
fn histogram(sums: &BTreeMap<u64, u64>) -> Value {
    let kept = || sums.iter().filter(|(_, sum)| **sum != 0);
    Value::Tuple2(
        Box::new(Value::Array(kept().map(|(k, _)| Value::UInt(*k)).collect())),
        Box::new(Value::Array(kept().map(|(_, s)| Value::UInt(*s)).collect())),
    )
}

/// The sum of a column over rows, nulls skipped; null when all are null.
fn sum(rows: &[&Cells], at: usize) -> Value {
    let mut total: Option<Value> = None;
    for row in rows {
        total = match (total, row.get(at)) {
            (None, Some(v @ (Value::UInt(_) | Value::Int(_) | Value::Float(_)))) => Some(v.clone()),
            (Some(Value::UInt(a)), Some(Value::UInt(b))) => Some(Value::UInt(a.wrapping_add(*b))),
            (Some(Value::Int(a)), Some(Value::Int(b))) => Some(Value::Int(a.wrapping_add(*b))),
            (Some(Value::Float(a)), Some(Value::Float(b))) => Some(Value::Float(a + b)),
            (kept, _) => kept,
        };
    }
    total.unwrap_or(Value::Null)
}

/// The merge of one `SimpleAggregateFunction` column over the rows of one sort key.
fn merge_cell(function: AggFn, rows: &[&Cells], at: usize) -> Value {
    match function {
        AggFn::Min => extreme(rows, at, Ordering::Less),
        AggFn::Max => extreme(rows, at, Ordering::Greater),
        AggFn::Sum => sum(rows, at),
        AggFn::SumMap => {
            let mut sums = BTreeMap::new();
            for row in rows {
                if let Some(value) = row.get(at) {
                    add_histogram(&mut sums, value);
                }
            }
            histogram(&sums)
        }
        AggFn::GroupUniqArrayArray(cap) => {
            let mut seen = BTreeSet::new();
            let mut union = Vec::new();
            for row in rows {
                let Some(Value::Array(items)) = row.get(at) else {
                    continue;
                };
                for item in items {
                    if union.len() < usize::from(cap) && seen.insert(key_of([item])) {
                        union.push(item.clone());
                    }
                }
            }
            Value::Array(union)
        }
    }
}

impl ReadSide {
    /// Takes in the parts that landed since the last call and, on the seeded schedule, merges. A landed block
    /// that does not read as rows of its table is a defect of the model: it is returned for the model's list
    /// of violations, and every later read of the table fails.
    pub(crate) fn sync(&mut self, id: TableId, landed: &[Part], rng: &dyn Rng) -> Option<String> {
        let def = table(id);
        let stored = self.tables.entry(id).or_default();
        let before = stored.absorbed;
        let mut unreadable = None;
        for part in landed.iter().skip(before) {
            match rows_of(def, &part.block) {
                Some(rows) => stored.parts.push(Stored {
                    partition: part.partition,
                    deleted: vec![false; rows.len()],
                    mask_until: 0,
                    rows,
                }),
                None => {
                    let what = format!(
                        "a landed block of {} does not read as rows of the table",
                        def.name()
                    );
                    stored.broken = Some(what.clone());
                    unreadable = Some(what);
                }
            }
        }
        stored.absorbed = landed.len();
        if stored.absorbed == before {
            return unreadable;
        }
        // A bounded number of merges per arrival: each picks a partition with several parts and a run of them.
        for _ in 0..4 {
            if self.merge_ppm == 0 || rng.next_u64() % 1_000_000 >= u64::from(self.merge_ppm) {
                break;
            }
            let Some(stored) = self.tables.get(&id) else {
                break;
            };
            let mut by_partition: BTreeMap<PartitionId, Vec<usize>> = BTreeMap::new();
            for (at, part) in stored.parts.iter().enumerate() {
                by_partition.entry(part.partition).or_default().push(at);
            }
            let several: Vec<Vec<usize>> = by_partition
                .into_values()
                .filter(|parts| parts.len() > 1)
                .collect();
            if several.is_empty() {
                break;
            }
            let parts = &several[(rng.next_u64() % several.len() as u64) as usize];
            let count = 2 + (rng.next_u64() % (parts.len() as u64 - 1)) as usize;
            let first = (rng.next_u64() % (parts.len() - count + 1) as u64) as usize;
            self.merge(id, &parts[first..first + count]);
        }
        unreadable
    }

    /// Merges every partition of every table down to one part.
    pub(crate) fn merge_all(&mut self, now: u64) {
        let ids: Vec<TableId> = self.tables.keys().copied().collect();
        for id in ids {
            let partitions: BTreeSet<PartitionId> = self
                .tables
                .get(&id)
                .map(|t| t.parts.iter().map(|p| p.partition).collect())
                .unwrap_or_default();
            for partition in partitions {
                let picks: Vec<usize> = self.tables.get(&id).map_or_else(Vec::new, |t| {
                    (0..t.parts.len())
                        .filter(|at| t.parts[*at].partition == partition)
                        .collect()
                });
                // A single part is rewritten too when it carries a mask, as `OPTIMIZE ... FINAL` does.
                let masked = self
                    .tables
                    .get(&id)
                    .is_some_and(|t| picks.iter().any(|at| t.parts[*at].masked(now)));
                if picks.len() > 1 || masked {
                    self.merge(id, &picks);
                }
            }
        }
    }

    /// Replaces the parts at `picks` (ascending, one partition) with their merge. Masked rows are left out; on
    /// an aggregating table the rows of one sort key become one row.
    fn merge(&mut self, id: TableId, picks: &[usize]) {
        let def = table(id);
        let Some(stored) = self.tables.get_mut(&id) else {
            return;
        };
        let Some(&first) = picks.first() else {
            return;
        };
        let partition = stored.parts[first].partition;
        let keys: Vec<usize> = def
            .order_by
            .iter()
            .filter_map(|name| def.columns.iter().position(|c| c.name == *name))
            .collect();
        let sort_key = |row: &Cells| key_of(keys.iter().filter_map(|at| row.get(*at)));
        let mut rows: Vec<Cells> = Vec::new();
        if def.kind == TableKind::Aggregating {
            let mut groups: BTreeMap<Vec<u8>, Vec<&Cells>> = BTreeMap::new();
            for at in picks {
                for row in stored.parts[*at].visible() {
                    groups.entry(sort_key(row)).or_default().push(row);
                }
            }
            for group in groups.values() {
                let merged = def
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(at, column)| match column.ty.agg_fn() {
                        Some(function) if !keys.contains(&at) => merge_cell(function, group, at),
                        _ => group[0][at].clone(),
                    })
                    .collect();
                rows.push(merged);
            }
        } else {
            for at in picks {
                rows.extend(stored.parts[*at].visible().cloned());
            }
        }
        rows.sort_by(|a, b| {
            keys.iter()
                .map(|at| sort_order(&a[*at], &b[*at], Direction::Asc))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        let merged = Stored {
            partition,
            deleted: vec![false; rows.len()],
            mask_until: 0,
            rows,
        };
        for at in picks.iter().rev() {
            stored.parts.remove(*at);
        }
        if !merged.rows.is_empty() {
            stored.parts.insert(first.min(stored.parts.len()), merged);
        }
        self.merges += 1;
    }

    /// Whether a delete or a drop changed the table, so that a probe must scan this view instead of the ledger.
    pub(crate) fn touched(&self, id: TableId) -> bool {
        self.tables.get(&id).is_some_and(|t| t.touched)
    }

    /// The scan of the existence probe over what the store holds now.
    pub(crate) fn probe(
        &self,
        id: TableId,
        partition: PartitionId,
        epoch: Uuid,
        batch_seq: u64,
    ) -> bool {
        let def = table(id);
        let at = |name: &str| def.columns.iter().position(|c| c.name == name);
        let (epoch_at, seq_at) = match def.kind {
            TableKind::Plain => (at("wal_epoch"), at("batch_seq")),
            TableKind::Aggregating => (at("marker_epoch"), at("marker_batch_seq")),
        };
        let (Some(epoch_at), Some(seq_at)) = (epoch_at, seq_at) else {
            return false;
        };
        let marker_only = def.kind == TableKind::Aggregating;
        self.tables.get(&id).is_some_and(|stored| {
            stored
                .parts
                .iter()
                .filter(|part| part.partition == partition)
                .flat_map(Stored::visible)
                .any(|row| {
                    (!marker_only || row.first() == Some(&Value::Uuid(Uuid::NIL)))
                        && row.get(epoch_at) == Some(&Value::Uuid(epoch))
                        && row.get(seq_at) == Some(&Value::UInt(batch_seq))
                })
        })
    }

    fn mutation(&mut self, id: TableId, done_at: u64) -> MutationId {
        self.next_mutation += 1;
        let name = format!("mutation_{:010}.txt", self.next_mutation);
        self.tables
            .entry(id)
            .or_default()
            .mutations
            .insert(name.clone(), done_at);
        MutationId::new(id, name)
    }

    /// A lightweight delete: the matching rows are masked, so no later read returns them.
    pub(crate) fn delete(
        &mut self,
        plan: &DeletePlan,
        done_at: u64,
    ) -> Result<MutationId, QueryError> {
        let def = table(plan.table());
        let test = Test::of(def, plan.pred())?;
        let target: Box<dyn Fn(&Cells) -> bool> = match plan.target() {
            DeleteTarget::Project(scope) => {
                let project = Value::Uuid(scope.project_id().uuid());
                Box::new(move |row| row.first() == Some(&project))
            }
            DeleteTarget::MarkerPurge { older_than } => {
                let marker_at = index(def, "marker_at")?;
                let limit = Value::Time(older_than);
                Box::new(move |row| {
                    row.first() == Some(&Value::Uuid(Uuid::NIL))
                        && row
                            .get(marker_at)
                            .and_then(|at| compare(at, &limit))
                            .is_some_and(Ordering::is_lt)
                })
            }
        };
        let stored = self.tables.entry(plan.table()).or_default();
        stored.touched = true;
        for part in &mut stored.parts {
            if part.partition != plan.partition() {
                continue;
            }
            for (row, deleted) in part.rows.iter().zip(&mut part.deleted) {
                if !*deleted && target(row) && test.passes(row) {
                    *deleted = true;
                }
            }
        }
        Ok(self.mutation(plan.table(), done_at))
    }

    /// `APPLY DELETED MASK IN PARTITION`: the masked rows of the partition are removed for good. The mutation
    /// runs in the background: until it is done, at `done_at`, its parts still count as masked.
    pub(crate) fn apply_deleted_mask(
        &mut self,
        id: TableId,
        partition: PartitionId,
        done_at: u64,
    ) -> MutationId {
        let stored = self.tables.entry(id).or_default();
        for part in &mut stored.parts {
            if part.partition == partition && part.deleted.iter().any(|d| *d) {
                let mut kept = part.deleted.iter().map(|deleted| !*deleted);
                part.rows.retain(|_| kept.next().unwrap_or(true));
                part.deleted = vec![false; part.rows.len()];
                part.mask_until = done_at;
            }
        }
        self.mutation(id, done_at)
    }

    /// `DROP PARTITION`: every part of the partition goes, marker rows included.
    pub(crate) fn drop_partition(&mut self, id: TableId, partition: PartitionId) {
        let stored = self.tables.entry(id).or_default();
        stored.touched = true;
        stored.parts.retain(|part| part.partition != partition);
    }

    /// A mutation the store does not list is done.
    pub(crate) fn mutation_status(&self, id: &MutationId, now: u64) -> MutationStatus {
        let done_at = self
            .tables
            .get(&id.table())
            .and_then(|stored| stored.mutations.get(id.name()));
        match done_at {
            Some(at) if now < *at => MutationStatus::Pending,
            _ => MutationStatus::Done,
        }
    }

    /// The distinct projects with visible rows in a partition, ascending, without the marker project.
    pub(crate) fn partition_projects(&self, id: TableId, partition: PartitionId) -> Vec<ProjectId> {
        let mut projects = BTreeSet::new();
        if let Some(stored) = self.tables.get(&id) {
            for part in stored.parts.iter().filter(|p| p.partition == partition) {
                for row in part.visible() {
                    if let Some(Value::Uuid(project)) = row.first()
                        && !project.is_nil()
                    {
                        projects.insert(ProjectId::from_uuid(*project));
                    }
                }
            }
        }
        projects.into_iter().collect()
    }

    /// The parts of a partition that still carry a delete mask at model time `now`.
    pub(crate) fn masked_parts(&self, id: TableId, partition: PartitionId, now: u64) -> u64 {
        self.tables.get(&id).map_or(0, |stored| {
            stored
                .parts
                .iter()
                .filter(|part| part.partition == partition && part.masked(now))
                .count() as u64
        })
    }
}

/// A row order.
type By<'a, T> = &'a dyn Fn(&T, &T) -> Ordering;

/// Orders `items`, cuts them at `limit` and groups the rows that tie into runs.
fn finish<T>(
    mut items: Vec<T>,
    order: Option<By<'_, T>>,
    limit: Option<u32>,
    out: impl Fn(T) -> Row,
) -> Answer {
    if let Some(order) = order {
        items.sort_by(|a, b| order(a, b));
    }
    let cut = limit.is_some_and(|limit| items.len() > limit as usize);
    if let Some(limit) = limit {
        items.truncate(limit as usize);
    }
    let mut runs: Vec<Vec<Row>> = Vec::new();
    let mut previous: Option<&T> = None;
    let mut starts = Vec::with_capacity(items.len());
    for item in &items {
        let tie = match (order, previous) {
            (Some(order), Some(before)) => order(before, item).is_eq(),
            (None, Some(_)) => true,
            (_, None) => false,
        };
        starts.push(!tie);
        previous = Some(item);
    }
    for (item, start) in items.into_iter().zip(starts) {
        match runs.last_mut() {
            Some(run) if !start => run.push(out(item)),
            _ => runs.push(vec![out(item)]),
        }
    }
    Answer { runs, cut }
}

/// The first instant of the bucket of `step` that holds `value`, as the renderer's bucket expression gives it.
fn bucket(value: &Value, step: Step) -> Value {
    let seconds = match value {
        Value::Time(t) => t.micros().div_euclid(1_000_000),
        Value::Date(d) => d.start().micros().div_euclid(1_000_000),
        _ => return Value::Null,
    };
    let floor = |width: i64| seconds.div_euclid(width) * width;
    let start = match step {
        Step::Minute => floor(60),
        Step::FiveMinutes => floor(300),
        Step::FifteenMinutes => floor(900),
        Step::Hour => floor(3_600),
        Step::Day => floor(86_400),
        Step::Week => {
            let day = CivilDate::from_day_number(seconds.div_euclid(86_400));
            day.monday_on_or_before().start().micros() / 1_000_000
        }
    };
    Value::Time(Timestamp::from_micros(start * 1_000_000))
}

/// One measure with its columns resolved.
#[derive(Debug)]
enum Computed {
    Count,
    Sum(usize),
    Min(usize),
    Max(usize),
    Uniq(usize),
    Histogram(usize),
    Latest {
        value: usize,
        version: usize,
        fact_id: usize,
    },
}

impl Computed {
    fn of(def: &TableDef, measure: &Measure) -> Result<Self, QueryError> {
        let at = |column: &ColumnRef| index(def, column.name());
        Ok(match measure {
            Measure::Count => Self::Count,
            Measure::Sum(c) => Self::Sum(at(c)?),
            Measure::Min(c) => Self::Min(at(c)?),
            // A pick partial is rank-prefixed, so its merge is the maximum.
            Measure::Max(c) | Measure::PickMerge(c) => Self::Max(at(c)?),
            // `uniqCombined64` of a nullable column has a nullable result, which the plan's output type cannot
            // carry: the adapter fails such a read, and so does the model.
            Measure::Uniq(c) if matches!(c.ty(), Ty::Nullable(_)) => {
                return Err(internal(format!(
                    "a distinct count of the nullable column {} has a nullable result",
                    c.name()
                )));
            }
            Measure::Uniq(c) => Self::Uniq(at(c)?),
            Measure::HistogramMerge(c) => Self::Histogram(at(c)?),
            Measure::LatestByVersion(c) => Self::Latest {
                value: at(c)?,
                version: index(def, "version")?,
                fact_id: index(def, "fact_id")?,
            },
        })
    }

    fn over(&self, rows: &[&Cells]) -> Value {
        match self {
            Self::Count => Value::UInt(rows.len() as u64),
            Self::Sum(at) => sum(rows, *at),
            Self::Min(at) => extreme(rows, *at, Ordering::Less),
            Self::Max(at) => extreme(rows, *at, Ordering::Greater),
            Self::Uniq(at) => {
                let distinct: BTreeSet<Vec<u8>> = rows
                    .iter()
                    .filter_map(|row| row.get(*at))
                    .filter(|value| !matches!(value, Value::Null))
                    .map(|value| key_of([value]))
                    .collect();
                Value::UInt(distinct.len() as u64)
            }
            Self::Histogram(at) => {
                let mut sums = BTreeMap::new();
                for row in rows {
                    if let Some(value) = row.get(*at) {
                        add_histogram(&mut sums, value);
                    }
                }
                histogram(&sums)
            }
            Self::Latest {
                value,
                version,
                fact_id,
            } => {
                // `argMax` skips the rows whose argument is null.
                let mut best: Option<&Cells> = None;
                for row in rows {
                    if matches!(row.get(*value), None | Some(Value::Null)) {
                        continue;
                    }
                    let newer = best.is_none_or(|b| match compare(&row[*version], &b[*version]) {
                        Some(Ordering::Equal) => {
                            compare(&row[*fact_id], &b[*fact_id]) == Some(Ordering::Greater)
                        }
                        other => other == Some(Ordering::Greater),
                    });
                    if newer {
                        best = Some(row);
                    }
                }
                best.map_or(Value::Null, |row| row[*value].clone())
            }
        }
    }
}

struct Interp<'a> {
    side: &'a ReadSide,
    project: Value,
    window: TimeWindow,
}

impl<'a> Interp<'a> {
    /// The rows of one table reference: of the plan's project, inside the window, matching `pred`.
    fn rows(&self, id: TableId, pred: &Pred) -> Result<(Vec<&'a Cells>, Window), QueryError> {
        let def = table(id);
        let window = Window::of(def, self.window)?;
        let test = Test::of(def, pred)?;
        let mut rows = Vec::new();
        if let Some(stored) = self.side.tables.get(&id) {
            if let Some(broken) = &stored.broken {
                return Err(internal(broken.clone()));
            }
            for part in &stored.parts {
                rows.extend(part.visible().filter(|row| {
                    row.first() == Some(&self.project) && window.passes(row) && test.passes(row)
                }));
            }
        }
        Ok((rows, window))
    }

    fn scan(
        &self,
        n: &Scan,
        restriction: Option<(usize, &BTreeSet<Vec<u8>>)>,
    ) -> Result<Answer, QueryError> {
        let def = table(n.table());
        let (mut rows, _) = self.rows(n.table(), n.pred())?;
        let arrival = n.arrival();
        if arrival.as_of.is_some() || arrival.since.is_some() {
            let at = index(def, "received_at")?;
            rows.retain(|row| {
                let within = |bound: Option<Timestamp>, keep: fn(Ordering) -> bool| {
                    bound.is_none_or(|bound| {
                        compare(&row[at], &Value::Time(bound)).is_some_and(keep)
                    })
                };
                within(arrival.as_of, Ordering::is_le) && within(arrival.since, Ordering::is_gt)
            });
        }
        let order = match n.order() {
            Some(order) => Some((indexes(def, order.columns())?, order.direction())),
            None => None,
        };
        if let (Some((columns, direction)), Some(cursor)) = (&order, n.cursor()) {
            let cursor: Vec<Value> = cursor.iter().map(literal).collect();
            let after = match direction {
                Direction::Asc => Ordering::Greater,
                Direction::Desc => Ordering::Less,
            };
            // A tuple comparison: the first column that differs decides.
            rows.retain(|row| {
                for (at, value) in columns.iter().zip(&cursor) {
                    match compare(&row[*at], value) {
                        Some(Ordering::Equal) => {}
                        other => return other == Some(after),
                    }
                }
                false
            });
        }
        if let Some((on, ids)) = restriction {
            rows.retain(|row| !matches!(row[on], Value::Null) && ids.contains(&key_of([&row[on]])));
        }
        let out = indexes(def, n.columns())?;
        let by = order.map(|(columns, direction)| {
            move |a: &&Cells, b: &&Cells| {
                columns
                    .iter()
                    .map(|at| sort_order(&a[*at], &b[*at], direction))
                    .find(|o| o.is_ne())
                    .unwrap_or(Ordering::Equal)
            }
        });
        Ok(finish(
            rows,
            by.as_ref().map(|by| by as By<'_, &Cells>),
            n.limit(),
            |row| Row::new(out.iter().map(|at| row[*at].clone()).collect()),
        ))
    }

    fn point_lookup(&self, n: &PointLookup) -> Result<Answer, QueryError> {
        let def = table(n.table());
        let (rows, _) = self.rows(n.table(), &Pred::all())?;
        let key_at = indexes(def, n.key_columns())?;
        let keys: BTreeSet<Vec<u8>> = n
            .keys()
            .iter()
            .map(|key| key_of(&key.iter().map(literal).collect::<Vec<_>>()))
            .collect();
        let out = indexes(def, n.columns())?;
        let found: Vec<&Cells> = rows
            .into_iter()
            .filter(|row| keys.contains(&key_of(key_at.iter().map(|at| &row[*at]))))
            .collect();
        Ok(finish(found, None, None, |row| {
            Row::new(out.iter().map(|at| row[*at].clone()).collect())
        }))
    }

    fn aggregate(
        &self,
        n: &Aggregate,
        series: Option<(usize, Step)>,
    ) -> Result<Answer, QueryError> {
        let def = table(n.table());
        let (rows, window) = self.rows(n.table(), n.pred())?;
        let group_at = indexes(def, n.group())?;
        let measures: Vec<Computed> = n
            .measures()
            .iter()
            .map(|measure| Computed::of(def, measure))
            .collect::<Result<_, _>>()?;
        let mut groups: BTreeMap<Vec<u8>, (Vec<Value>, Vec<&Cells>)> = BTreeMap::new();
        for row in rows {
            let mut dimensions: Vec<Value> = series
                .iter()
                .map(|(time, step)| bucket(&row[*time], *step))
                .collect();
            dimensions.extend(group_at.iter().map(|at| row[*at].clone()));
            groups
                .entry(key_of(&dimensions))
                .or_insert_with(|| (dimensions, Vec::new()))
                .1
                .push(row);
        }
        let mut out: Vec<Vec<Value>> = Vec::new();
        for (mut values, rows) in groups.into_values() {
            if !window.group_passes(&rows) {
                continue;
            }
            let dimensions = values.len();
            values.extend(measures.iter().map(|measure| measure.over(&rows)));
            let kept = n.having_conditions().iter().all(|condition| {
                values
                    .get(dimensions + condition.measure)
                    .and_then(|found| compare(found, &literal(&condition.value)))
                    .is_some_and(|order| holds(condition.op, order))
            });
            if kept {
                out.push(values);
            }
        }
        let offset = usize::from(series.is_some());
        let by: Vec<(usize, Direction)> = n
            .order()
            .iter()
            .map(|(output, direction)| {
                let at = match output {
                    AggOutput::Group(i) => offset + i,
                    AggOutput::Measure(i) => offset + n.group().len() + i,
                };
                (at, *direction)
            })
            .collect();
        let order = |a: &Vec<Value>, b: &Vec<Value>| {
            by.iter()
                .map(|(at, direction)| sort_order(&a[*at], &b[*at], *direction))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        };
        let ordered: Option<By<'_, Vec<Value>>> = if by.is_empty() { None } else { Some(&order) };
        Ok(finish(out, ordered, n.limit(), Row::new))
    }

    fn semi_join(&self, n: &SemiJoin) -> Result<Answer, QueryError> {
        let (ids, right_in) = match n.right() {
            Node::Aggregate(right) => (self.aggregate(right, None)?, longest_in(right.pred())),
            Node::Scan(right) => (self.scan(right, None)?, longest_in(right.pred())),
            _ => return Err(internal("a semi-join over a node that produces no ids")),
        };
        let ids: BTreeSet<Vec<u8>> = ids
            .into_rows()
            .iter()
            .filter_map(|row| row.values().first())
            .filter(|id| !matches!(id, Value::Null))
            .map(|id| key_of([id]))
            .collect();
        // The cap counts distinct ids, and admits the longest literal list of the statement.
        let cap = (n.max_ids() as usize)
            .max(right_in)
            .max(longest_in(n.left().pred()));
        if ids.len() > cap {
            return Err(QueryError::TooBroad);
        }
        let on = index(table(n.left().table()), n.on().name())?;
        self.scan(n.left(), Some((on, &ids)))
    }

    fn text_search(&self, n: &TextSearch) -> Result<Answer, QueryError> {
        let def = table(n.table());
        let (rows, _) = self.rows(n.table(), n.pred())?;
        let mut fields = Vec::new();
        for field in n.fields() {
            let text = def.indexes.iter().find_map(|i| match i.kind {
                IndexKind::Text { tokenizer, lower } if i.column == field.name() => {
                    Some((tokenizer, lower))
                }
                _ => None,
            });
            let (tokenizer, lower) =
                text.ok_or_else(|| internal(format!("{} has no text index", field.name())))?;
            // The tokens of a list are looked up as they are: the server applies the index's preprocessor to
            // the column, not to them.
            let wanted: Vec<Vec<u8>> = n
                .tokens()
                .iter()
                .map(|token| token.as_bytes().to_vec())
                .collect();
            fields.push((index(def, field.name())?, tokenizer, lower, wanted));
        }
        let matches = |row: &&Cells| {
            fields.iter().any(|(at, tokenizer, lower, wanted)| {
                let held = tokens(&row[*at], *tokenizer, *lower);
                match n.mode() {
                    // The phrase function is not available on the floor version: a phrase is all its tokens.
                    TokenMode::All | TokenMode::Phrase => wanted.iter().all(|t| held.contains(t)),
                    TokenMode::Any => wanted.iter().any(|t| held.contains(t)),
                }
            })
        };
        let out = indexes(def, &def.key_columns())?;
        let found: Vec<&Cells> = rows.into_iter().filter(matches).collect();
        Ok(finish(found, None, Some(n.limit()), |row| {
            Row::new(out.iter().map(|at| row[*at].clone()).collect())
        }))
    }

    fn time_series(&self, n: &TimeSeries) -> Result<Answer, QueryError> {
        if n.fill() == Fill::Zero {
            return Err(internal("zero-filled time series are not rendered"));
        }
        let def = table(n.aggregate().table());
        let time = index(def, n.time().name())?;
        self.aggregate(n.aggregate(), Some((time, n.step())))
    }

    fn field_range(&self, n: &FieldRange) -> Result<Answer, QueryError> {
        let def = table(n.table());
        let (rows, _) = self.rows(n.table(), &Pred::all())?;
        let key_at = indexes(def, n.key_columns())?;
        let key: Vec<Value> = n.key().iter().map(literal).collect();
        let field = index(def, n.field().name())?;
        let found: Vec<&Cells> = rows
            .into_iter()
            .filter(|row| {
                key_at
                    .iter()
                    .zip(&key)
                    .all(|(at, value)| equal(&row[*at], value))
            })
            .collect();
        // `LIMIT 1` without an order: which of several rows of one key answers is undefined.
        Ok(finish(found, None, Some(1), |row| {
            let bytes = bytes_of(&row[field]).unwrap_or_default();
            let start = usize::try_from(n.offset())
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let len = usize::try_from(n.byte_len()).unwrap_or(usize::MAX);
            let end = start.saturating_add(len).min(bytes.len());
            Row::new(vec![Value::string(bytes[start..end].to_vec())])
        }))
    }

    fn node(&self, node: &Node) -> Result<Answer, QueryError> {
        match node {
            Node::Scan(n) => self.scan(n, None),
            Node::PointLookup(n) => self.point_lookup(n),
            Node::Aggregate(n) => self.aggregate(n, None),
            Node::SemiJoin(n) => self.semi_join(n),
            Node::TextSearch(n) => self.text_search(n),
            Node::TimeSeries(n) => self.time_series(n),
            Node::FieldRange(n) => self.field_range(n),
        }
    }
}

/// Runs a plan over what the store holds. Every row is checked against `plan.output()`.
pub(crate) fn interpret(side: &ReadSide, plan: &QueryPlan) -> Result<Answer, QueryError> {
    let interp = Interp {
        side,
        project: Value::Uuid(plan.scope().project_id().uuid()),
        window: plan.window(),
    };
    let answer = interp.node(plan.root())?;
    let schema = plan.output();
    for row in answer.runs.iter().flatten() {
        schema
            .check_row(row)
            .map_err(|error| internal(error.to_string()))?;
    }
    Ok(answer)
}
