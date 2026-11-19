//! An in-memory `TelemetryStore` that answers typed plans from a small dataset. It interprets exactly what the
//! plan says (scope, window, predicate, order, keyset cursor, limit, arrival bounds, groups and measures) and
//! checks every row it returns against the plan's output schema, so a service that built a wrong plan gets a
//! wrong answer here too.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use crucible_chfmt::EncodedBlock;
use crucible_env::{
    InsertError, InsertSettings, MutationId, MutationStatus, QueryError, RowStream, StoreError,
    StoreFacts, StoreHealth, TelemetryStore,
};
use crucible_plan::{
    AggOutput, Aggregate, ArrivalBounds, BatchProbe, CmpOp, DeletePlan, Direction, FieldRange,
    Fill, Literal, Measure, Node, Order, Pred, QueryPlan, Row, RowBlock, Scan, TextSearch,
    TimeSeries, TokenMode, Value,
};
use crucible_records::{PartitionId, ProjectId, TableId, TimeWindow, Timestamp, Uuid};
use crucible_schema::{ColumnRef, MigrationStep, Ty, table};

const DAY_MICROS: i64 = 86_400_000_000;

/// A stored row: one value per column of the table definition, `project_id` first.
pub type StoredRow = Vec<Value>;

#[derive(Debug, Default)]
struct State {
    tables: BTreeMap<TableId, Vec<StoredRow>>,
    unavailable: bool,
    executed: Vec<QueryPlan>,
}

type Hook = Box<dyn FnOnce(&FakeStore) + Send>;

#[derive(Default)]
pub struct FakeStore {
    state: Mutex<State>,
    /// Runs once, after the rows of the next read are computed and before they are returned: what happens to
    /// the node while a store read is in flight.
    during_read: Mutex<Option<Hook>>,
}

impl std::fmt::Debug for FakeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeStore").finish_non_exhaustive()
    }
}

/// The zero value of a column type.
pub fn zero(ty: &Ty) -> Value {
    match ty {
        Ty::Uuid => Value::Uuid(Uuid::NIL),
        Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64 => Value::UInt(0),
        Ty::Int32 | Ty::Int64 => Value::Int(0),
        Ty::Float32 | Ty::Float64 => Value::Float(0.0),
        Ty::String | Ty::LowCardinalityString => Value::String(String::new()),
        Ty::FixedString(n) => Value::Bytes(vec![0; usize::from(*n)]),
        Ty::Date => Value::Date(Timestamp::EPOCH.date()),
        Ty::DateTime | Ty::DateTime64 => Value::Time(Timestamp::EPOCH),
        Ty::Enum8(kind) => Value::Enum(kind.variants()[0].1),
        Ty::Nullable(_) => Value::Null,
        Ty::Array(_) => Value::Array(Vec::new()),
        Ty::Tuple2(a, b) => Value::Tuple2(Box::new(zero(a)), Box::new(zero(b))),
        Ty::Saf(_, inner) => zero(inner),
    }
}

/// A row of `table_id` with the named columns set and every other column zero.
pub fn stored_row(table_id: TableId, set: &[(&str, Value)]) -> StoredRow {
    let def = table(table_id);
    for (name, _) in set {
        assert!(
            def.column(name).is_some(),
            "{table_id:?} has no column {name}"
        );
    }
    def.columns
        .iter()
        .map(|column| {
            let value = set.iter().find(|(name, _)| *name == column.name);
            let value = value.map_or_else(|| zero(&column.ty), |(_, value)| value.clone());
            assert!(value.fits(&column.ty), "{}.{}", def.name(), column.name);
            value
        })
        .collect()
}

fn position(table_id: TableId, name: &str) -> usize {
    table(table_id)
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("{table_id:?} has no column {name}"))
}

fn cell<'a>(row: &'a StoredRow, column: &ColumnRef) -> &'a Value {
    &row[position(column.table(), column.name())]
}

fn literal(literal: &Literal) -> Value {
    match literal {
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

/// The order of two values of one column.
fn compare(a: &Value, b: &Value) -> Ordering {
    let bytes = |v: &Value| match v {
        Value::String(text) => Some(text.as_bytes().to_vec()),
        Value::Bytes(bytes) => Some(bytes.clone()),
        _ => None,
    };
    match (a, b) {
        // Nulls sort last, as the server sorts them.
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::UInt(a), Value::UInt(b)) => a.cmp(b),
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Time(a), Value::Time(b)) => a.cmp(b),
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Enum(a), Value::Enum(b)) => a.cmp(b),
        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        _ => match (bytes(a), bytes(b)) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => panic!("the fake store cannot compare {a:?} with {b:?}"),
        },
    }
}

/// The text of a string value; the fake store's payloads are UTF-8.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("{other:?} is not text"),
    }
}

fn compare_tuple(a: &[Value], b: &[Value]) -> Ordering {
    a.iter()
        .zip(b)
        .map(|(a, b)| compare(a, b))
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}

fn holds(pred: &Pred, row: &StoredRow) -> bool {
    match pred {
        Pred::And(operands) => operands.iter().all(|p| holds(p, row)),
        Pred::Or(operands) => operands.iter().any(|p| holds(p, row)),
        Pred::Not(operand) => !holds(operand, row),
        Pred::Cmp(column, op, value) => {
            let order = compare(cell(row, column), &literal(value));
            match op {
                CmpOp::Eq => order.is_eq(),
                CmpOp::Ne => order.is_ne(),
                CmpOp::Lt => order.is_lt(),
                CmpOp::Le => order.is_le(),
                CmpOp::Gt => order.is_gt(),
                CmpOp::Ge => order.is_ge(),
            }
        }
        Pred::In(column, values) => values
            .iter()
            .any(|value| compare(cell(row, column), &literal(value)).is_eq()),
        other => panic!("the fake store does not interpret {other:?}"),
    }
}

/// Whether a row is inside the plan's window, by the column the table is windowed on (contract 8.3).
fn in_window(table_id: TableId, row: &StoredRow, window: TimeWindow) -> bool {
    let time = |name: &str| match &row[position(table_id, name)] {
        Value::Time(t) => *t,
        other => panic!("{name} is {other:?}"),
    };
    match table_id {
        TableId::SpanCore
        | TableId::TraceHeads
        | TableId::SpanPayload
        | TableId::McpOps
        | TableId::McpContent => {
            window.contains(time("start_time"))
        }
        TableId::SpanMetrics | TableId::McpToolRollup => window.contains(time("minute")),
        // Their time columns are aggregated: the window applies to each group (`group_in_window`).
        TableId::McpDeploys | TableId::McpToolInventory => true,
        TableId::FactScores | TableId::FactTags | TableId::FactFlags | TableId::FactCorrections => {
            window.contains(time("target_time"))
        }
        // A day of a session is read when the day overlaps the window.
        TableId::SessionRollup => match &row[position(table_id, "day")] {
            Value::Date(day) => {
                let start = day.start().micros();
                start < window.to().micros() && start + DAY_MICROS > window.from().micros()
            }
            other => panic!("day is {other:?}"),
        },
        TableId::TraceStats => match &row[position(table_id, "row_day")] {
            // A day's partial is read when the day overlaps the window.
            Value::Date(day) => {
                let start = day.start().micros();
                start < window.to().micros() && start + DAY_MICROS > window.from().micros()
            }
            other => panic!("row_day is {other:?}"),
        },
        other => panic!("the fake store has no window rule for {other:?}"),
    }
}

impl FakeStore {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a row. An identical row is ignored, which is how a batch that is shipped twice stays one copy.
    pub fn insert(&self, table_id: TableId, row: StoredRow) {
        let mut state = self.state();
        let rows = state.tables.entry(table_id).or_default();
        if !rows.contains(&row) {
            rows.push(row);
        }
    }

    /// Sets what happens while the next read is in flight.
    pub fn during_next_read(&self, hook: impl FnOnce(&Self) + Send + 'static) {
        *self.during_read.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn set_unavailable(&self, unavailable: bool) {
        self.state().unavailable = unavailable;
    }

    /// Every plan executed so far.
    pub fn executed(&self) -> Vec<QueryPlan> {
        self.state().executed.clone()
    }

    /// The rows of a table that the plan may read: the scope's project, inside the window.
    fn visible(&self, plan: &QueryPlan, table_id: TableId, pred: &Pred) -> Vec<StoredRow> {
        let project = Value::Uuid(plan.scope().project_id().uuid());
        let state = self.state();
        let rows = state.tables.get(&table_id).map_or(&[][..], Vec::as_slice);
        rows.iter()
            .filter(|row| row[0] == project)
            .filter(|row| in_window(table_id, row, plan.window()))
            .filter(|row| holds(pred, row))
            .cloned()
            .collect()
    }

    /// The rows inside the arrival bounds, ordered and after the cursor, cut at the limit.
    fn keyset(
        table_id: TableId,
        rows: &mut Vec<StoredRow>,
        arrival: ArrivalBounds,
        order: Option<&Order>,
        cursor: Option<&[Literal]>,
        limit: Option<u32>,
    ) {
        if arrival.as_of.is_some() || arrival.since.is_some() {
            let at = position(table_id, "received_at");
            rows.retain(|row| {
                let Value::Time(received_at) = row[at] else {
                    panic!("received_at is not a time");
                };
                arrival.as_of.is_none_or(|as_of| received_at <= as_of)
                    && arrival.since.is_none_or(|since| received_at > since)
            });
        }
        if let Some(order) = order {
            let key = |row: &StoredRow| -> Vec<Value> {
                order
                    .columns()
                    .iter()
                    .map(|c| cell(row, c).clone())
                    .collect()
            };
            let directed = |order_of: Ordering| match order.direction() {
                Direction::Asc => order_of,
                Direction::Desc => order_of.reverse(),
            };
            rows.sort_by(|a, b| directed(compare_tuple(&key(a), &key(b))));
            if let Some(cursor) = cursor {
                let cursor: Vec<Value> = cursor.iter().map(literal).collect();
                rows.retain(|row| directed(compare_tuple(&key(row), &cursor)).is_gt());
            }
        } else {
            assert!(cursor.is_none());
        }
        if let Some(limit) = limit {
            rows.truncate(limit as usize);
        }
    }

    fn scan(&self, plan: &QueryPlan, scan: &Scan) -> Vec<Row> {
        let table_id = scan.table();
        let mut rows = self.visible(plan, table_id, scan.pred());
        Self::keyset(
            table_id,
            &mut rows,
            scan.arrival(),
            scan.order(),
            scan.cursor(),
            scan.limit(),
        );
        rows.iter()
            .map(|row| {
                Row::new(
                    scan.columns()
                        .iter()
                        .map(|c| cell(row, c).clone())
                        .collect(),
                )
            })
            .collect()
    }

    /// A text search: tokens as the payload indexes split and lower them, snippets around the first needle.
    fn text_search(&self, plan: &QueryPlan, search: &TextSearch) -> Vec<Row> {
        let table_id = search.table();
        let mut rows = self.visible(plan, table_id, search.pred());
        let held = |value: &Value| -> Vec<String> {
            match value {
                Value::Array(items) => items.iter().map(text_of).collect(),
                other => text_of(other)
                    .to_ascii_lowercase()
                    .split(|c: char| c.is_ascii() && !c.is_ascii_alphanumeric())
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned)
                    .collect(),
            }
        };
        rows.retain(|row| {
            search.fields().iter().any(|field| {
                let held = held(cell(row, field));
                let mut wanted = search.tokens().iter();
                match search.mode() {
                    TokenMode::All | TokenMode::Phrase => wanted.all(|t| held.contains(t)),
                    TokenMode::Any => wanted.any(|t| held.contains(t)),
                }
            })
        });
        Self::keyset(
            table_id,
            &mut rows,
            search.arrival(),
            search.order(),
            search.cursor(),
            Some(search.limit()),
        );
        let keys = table(table_id).key_columns();
        rows.iter()
            .map(|row| {
                let mut values: Vec<Value> = keys.iter().map(|c| cell(row, c).clone()).collect();
                if let Some(snippet) = search.snippet() {
                    for field in search.snippet_fields() {
                        let text = text_of(cell(row, &field));
                        let lowered = text.to_ascii_lowercase();
                        let found = search
                            .needles()
                            .iter()
                            .filter_map(|needle| lowered.find(&needle.to_ascii_lowercase()))
                            .min()
                            .map_or(0, |at| at + 1);
                        let before = snippet.before as usize;
                        let start = if found > before {
                            found - before - 1
                        } else {
                            0
                        };
                        let end = (start + snippet.len as usize).min(text.len());
                        values.push(Value::UInt(found as u64));
                        values.push(Value::string(text.as_bytes()[start..end].to_vec()));
                    }
                }
                Row::new(values)
            })
            .collect()
    }

    /// A byte range of one field of the row with the key.
    fn field_range(&self, plan: &QueryPlan, range: &FieldRange) -> Vec<Row> {
        let rows = self.visible(plan, range.table(), &Pred::all());
        let found = rows.iter().find(|row| {
            range
                .key_columns()
                .iter()
                .zip(range.key())
                .all(|(column, value)| compare(cell(row, column), &literal(value)).is_eq())
        });
        found
            .map(|row| {
                let text = text_of(cell(row, &range.field()));
                let bytes = text.as_bytes();
                let start = usize::try_from(range.offset()).unwrap().min(bytes.len());
                let end = start
                    .saturating_add(usize::try_from(range.byte_len()).unwrap_or(usize::MAX))
                    .min(bytes.len());
                let mut values = vec![Value::string(bytes[start..end].to_vec())];
                if range.total() {
                    values.push(Value::UInt(bytes.len() as u64));
                }
                Row::new(values)
            })
            .into_iter()
            .collect()
    }

    /// Rows by key: those whose key columns equal one of the keys.
    fn point_lookup(&self, plan: &QueryPlan, lookup: &crucible_plan::PointLookup) -> Vec<Row> {
        let rows = self.visible(plan, lookup.table(), &Pred::all());
        rows.iter()
            .filter(|row| {
                lookup.keys().iter().any(|key| {
                    lookup
                        .key_columns()
                        .iter()
                        .zip(key)
                        .all(|(c, v)| compare(cell(row, c), &literal(v)).is_eq())
                })
            })
            .map(|row| {
                Row::new(
                    lookup
                        .columns()
                        .iter()
                        .map(|c| cell(row, c).clone())
                        .collect(),
                )
            })
            .collect()
    }

    fn aggregate(&self, plan: &QueryPlan, aggregate: &Aggregate) -> Vec<Row> {
        let mut rows = self.visible(plan, aggregate.table(), aggregate.pred());
        group_in_window(aggregate, &mut rows, plan.window());
        let mut out = group_rows(aggregate, rows, |row| {
            aggregate
                .group()
                .iter()
                .map(|c| cell(row, c).clone())
                .collect()
        });
        assert!(aggregate.having_conditions().is_empty() && aggregate.cursor().is_none());
        out.sort_by(|a, b| {
            aggregate
                .order()
                .iter()
                .map(|(output, direction)| {
                    let at = match output {
                        AggOutput::Group(i) => *i,
                        AggOutput::Measure(i) => aggregate.group().len() + i,
                    };
                    let order = compare(&a.values()[at], &b.values()[at]);
                    match direction {
                        Direction::Asc => order,
                        Direction::Desc => order.reverse(),
                    }
                })
                .find(|order| order.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        if let Some(limit) = aggregate.limit() {
            out.truncate(limit as usize);
        }
        out
    }

    /// A time series: the aggregate per (dimensions, bucket), zero-filled over the window when asked.
    fn time_series(&self, plan: &QueryPlan, series: &TimeSeries) -> Vec<Row> {
        let aggregate = series.aggregate();
        let rows = self.visible(plan, aggregate.table(), aggregate.pred());
        let step = series.step();
        let bucket = |row: &StoredRow| match cell(row, &series.time()) {
            Value::Time(t) => step.bucket(*t),
            other => panic!("the time column holds {other:?}"),
        };
        let grouped = group_rows(aggregate, rows, |row| {
            let mut key: Vec<Value> = aggregate
                .group()
                .iter()
                .map(|c| cell(row, c).clone())
                .collect();
            key.push(Value::Time(bucket(row)));
            key
        });
        let dims = aggregate.group().len();
        // The bucket goes first in the output.
        let mut out: Vec<Vec<Value>> = grouped
            .into_iter()
            .map(|row| {
                let mut values = row.into_values();
                let time = values.remove(dims);
                values.insert(0, time);
                values
            })
            .collect();
        if series.fill() == Fill::Zero {
            let (first, count) = step.buckets(plan.window());
            let mut keys: Vec<Vec<Value>> = out.iter().map(|v| v[1..=dims].to_vec()).collect();
            if dims == 0 {
                keys = vec![Vec::new()];
            }
            keys.dedup();
            for key in keys {
                for n in 0..count as i64 {
                    let at = first.saturating_add_micros(n * step.seconds() * 1_000_000);
                    let at = Value::Time(at);
                    if out.iter().any(|v| v[0] == at && v[1..=dims] == key[..]) {
                        continue;
                    }
                    let mut row = vec![at];
                    row.extend(key.iter().cloned());
                    row.extend(aggregate.measures().iter().map(|m| match m {
                        Measure::Count => Value::UInt(0),
                        Measure::Sum(c) if matches!(c.ty().scalar(), Ty::Int32 | Ty::Int64) => {
                            Value::Int(0)
                        }
                        Measure::Sum(_) => Value::UInt(0),
                        Measure::HistogramMerge(_) => Value::Tuple2(
                            Box::new(Value::Array(Vec::new())),
                            Box::new(Value::Array(Vec::new())),
                        ),
                        _ => Value::Null,
                    }));
                    out.push(row);
                }
            }
        }
        out.sort_by(|a, b| {
            compare_tuple(&a[1..=dims], &b[1..=dims]).then_with(|| compare(&a[0], &b[0]))
        });
        out.into_iter().map(Row::new).collect()
    }
}

/// The window of the tables whose time columns are aggregated (contract 8.9): a group is read when its
/// `max(last_seen)` is at or after `from` and its `min(first_seen)` is before `to`. Drops the rows of the
/// other groups.
fn group_in_window(aggregate: &Aggregate, rows: &mut Vec<StoredRow>, window: TimeWindow) {
    let table_id = aggregate.table();
    if !matches!(table_id, TableId::McpDeploys | TableId::McpToolInventory) {
        return;
    }
    let time = |row: &StoredRow, name: &str| match &row[position(table_id, name)] {
        Value::Time(t) => *t,
        other => panic!("{name} is {other:?}"),
    };
    let key = |row: &StoredRow| -> Vec<Value> {
        aggregate
            .group()
            .iter()
            .map(|c| cell(row, c).clone())
            .collect()
    };
    let snapshot = rows.clone();
    rows.retain(|row| {
        let members = snapshot
            .iter()
            .filter(|other| compare_tuple(&key(other), &key(row)).is_eq());
        let (mut first, mut last) = (Timestamp::MAX_EXCLUSIVE, Timestamp::EPOCH);
        for member in members {
            first = first.min(time(member, "first_seen"));
            last = last.max(time(member, "last_seen"));
        }
        last >= window.from() && first < window.to()
    });
}

/// The rows of an aggregate grouped by `key`, each group's key then its measures, in first-seen order.
fn group_rows(
    aggregate: &Aggregate,
    rows: Vec<StoredRow>,
    key: impl Fn(&StoredRow) -> Vec<Value>,
) -> Vec<Row> {
    let mut groups: Vec<(Vec<Value>, Vec<StoredRow>)> = Vec::new();
    for row in rows {
        let key = key(&row);
        match groups
            .iter_mut()
            .find(|(k, _)| compare_tuple(k, &key).is_eq())
        {
            Some((_, members)) => members.push(row),
            None => groups.push((key, vec![row])),
        }
    }
    groups
        .into_iter()
        .map(|(mut values, members)| {
            let column = |c: &ColumnRef| -> std::vec::IntoIter<Value> {
                let values: Vec<Value> = members.iter().map(|row| cell(row, c).clone()).collect();
                values.into_iter()
            };
            values.extend(aggregate.measures().iter().map(|measure| {
                match measure {
                    Measure::Count => Value::UInt(members.len() as u64),
                    Measure::Sum(c) => column(c)
                        .reduce(|a, b| match (a, b) {
                            (Value::UInt(a), Value::UInt(b)) => Value::UInt(a + b),
                            (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
                            other => panic!("cannot sum {other:?}"),
                        })
                        .unwrap(),
                    Measure::Uniq(c) => {
                        let mut seen: Vec<Value> = column(c).collect();
                        seen.sort_by(compare);
                        seen.dedup();
                        Value::UInt(seen.len() as u64)
                    }
                    Measure::Min(c) => column(c).min_by(compare).unwrap(),
                    Measure::Max(c) | Measure::PickMerge(c) => column(c).max_by(compare).unwrap(),
                    Measure::HistogramMerge(c) => merge_histograms(column(c)),
                    Measure::SetMerge(c) => {
                        let mut union: Vec<Value> = column(c)
                            .flat_map(|set| match set {
                                Value::Array(items) => items,
                                other => panic!("a set is an array, not {other:?}"),
                            })
                            .collect();
                        union.sort_by(compare);
                        union.dedup();
                        Value::Array(union)
                    }
                    other => panic!("the fake store does not compute {other:?}"),
                }
            }));
            Row::new(values)
        })
        .collect()
}

/// The `sumMap` of histogram partials: keys ascending, without zero counts.
fn merge_histograms(partials: impl Iterator<Item = Value>) -> Value {
    let mut sums: BTreeMap<u64, u64> = BTreeMap::new();
    for partial in partials {
        let Value::Tuple2(keys, counts) = partial else {
            panic!("a histogram is a tuple")
        };
        let (Value::Array(keys), Value::Array(counts)) = (*keys, *counts) else {
            panic!("a histogram holds two arrays")
        };
        for (k, n) in keys.into_iter().zip(counts) {
            let (Value::UInt(k), Value::UInt(n)) = (k, n) else {
                panic!("histogram entries are unsigned")
            };
            *sums.entry(k).or_default() += n;
        }
    }
    sums.retain(|_, n| *n > 0);
    let array = |values: Vec<u64>| Value::Array(values.into_iter().map(Value::UInt).collect());
    Value::Tuple2(
        Box::new(array(sums.keys().copied().collect())),
        Box::new(array(sums.values().copied().collect())),
    )
}

fn no_writes<T>() -> Result<T, StoreError> {
    Err(StoreError::Internal("the fake store only reads".to_owned()))
}

#[async_trait]
impl TelemetryStore for FakeStore {
    async fn insert_block(&self, _: EncodedBlock, _: InsertSettings) -> Result<(), InsertError> {
        Err(InsertError::FatalConfig {
            code: 0,
            message: "the fake store only reads".to_owned(),
        })
    }

    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError> {
        self.state().executed.push(plan.clone());
        if self.state().unavailable {
            return Err(QueryError::Unavailable("the store is down".to_owned()));
        }
        let rows = match plan.root() {
            Node::Scan(scan) => self.scan(&plan, scan),
            Node::Aggregate(aggregate) => self.aggregate(&plan, aggregate),
            Node::TimeSeries(series) => self.time_series(&plan, series),
            Node::TextSearch(search) => self.text_search(&plan, search),
            Node::FieldRange(range) => self.field_range(&plan, range),
            Node::PointLookup(lookup) => self.point_lookup(&plan, lookup),
            other => panic!("the fake store does not run {other:?}"),
        };
        let hook = self.during_read.lock().unwrap().take();
        if let Some(hook) = hook {
            hook(self);
        }
        let output = plan.output();
        for row in &rows {
            if let Err(shape) = output.check_row(row) {
                return Err(QueryError::Internal(shape.to_string()));
            }
        }
        let blocks = RowBlock::split(rows).into_iter().map(Ok);
        Ok(RowStream::new(
            Uuid::from_bytes([7; 16]),
            futures::stream::iter(blocks),
        ))
    }

    async fn probe(&self, _: BatchProbe) -> Result<bool, StoreError> {
        no_writes()
    }

    async fn delete(&self, _: DeletePlan) -> Result<MutationId, StoreError> {
        no_writes()
    }

    async fn drop_partition(&self, _: TableId, _: PartitionId) -> Result<(), StoreError> {
        no_writes()
    }

    async fn apply_deleted_mask(
        &self,
        _: TableId,
        _: PartitionId,
    ) -> Result<MutationId, StoreError> {
        no_writes()
    }

    async fn mutation_status(&self, _: &MutationId) -> Result<MutationStatus, StoreError> {
        no_writes()
    }

    async fn partition_projects(
        &self,
        _: TableId,
        _: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        no_writes()
    }

    async fn masked_parts(&self, _: TableId, _: PartitionId) -> Result<u64, StoreError> {
        no_writes()
    }

    async fn apply_migration(&self, _: &MigrationStep) -> Result<(), StoreError> {
        no_writes()
    }

    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        no_writes()
    }

    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        no_writes()
    }

    async fn kill(&self, _: Uuid) -> Result<(), StoreError> {
        Ok(())
    }

    async fn health(&self) -> StoreHealth {
        StoreHealth::Healthy
    }
}
