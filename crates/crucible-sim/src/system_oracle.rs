//! The reference side of the system simulation's oracle for the tables of milestone 2: the reference
//! aggregation that the rollups must equal exactly (invariant I7; token and cost sums leave out the usage
//! aggregates, whose flags are checked against a model of the rule over the LSN stream) and the fact rows with their reference
//! resolution (invariants I2 and I4). Everything is read from the parts of [`ChModel`], as the store holds
//! them.

use std::collections::{BTreeMap, BTreeSet};

use crucible_records::{Bucket, FactId, FactVersion, TableId, fold_facts};

use crate::ch_model::{
    ChModel, bytes_at, histogram_at, i8_at, i64_at, nullable_i64_at, u8_at, u16_at, u32_at, u64_at,
    uuid_at,
};
use crucible_chfmt::DecodedBlock;

/// What a span reports, as the client sent it: the oracle's expectation for its `span_core` row.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Sent {
    pub(crate) input: u64,
    pub(crate) output: u64,
    /// The provided cost; zero when the span reports none.
    pub(crate) cost_nusd: i64,
    pub(crate) error: bool,
}

const STATUS_ERROR: i8 = 2;
const LEVEL_ERROR: i8 = 3;
const GENERATION: i8 = 1;

/// Whether a `span_core` row carries exactly what its span reported: tokens, cost in nano-USD and the error
/// status (I7 at the core tier).
pub(crate) fn span_matches(block: &DecodedBlock, row: usize, sent: &Sent) -> bool {
    let error = i8_at(block, "status_code", row) == Some(STATUS_ERROR)
        || i8_at(block, "level", row) == Some(LEVEL_ERROR);
    u64_at(block, "input_tokens", row) == Some(sent.input)
        && u64_at(block, "output_tokens", row) == Some(sent.output)
        && u64_at(block, "total_tokens", row) == Some(sent.input + sent.output)
        && i64_at(block, "total_cost_nusd", row) == Some(sent.cost_nusd)
        && error == sent.error
}

/// The measures of one rollup key. Each table compares the fields it has; the others stay at their defaults
/// on both sides.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Agg {
    traces: u64,
    spans: u64,
    errors: u64,
    input: u64,
    output: u64,
    total: u64,
    cost: i64,
    first: i64,
    last: i64,
    durations: BTreeMap<u16, u64>,
    first_tokens: BTreeMap<u16, u64>,
}

impl Default for Agg {
    fn default() -> Self {
        Self {
            traces: 0,
            spans: 0,
            errors: 0,
            input: 0,
            output: 0,
            total: 0,
            cost: 0,
            first: i64::MAX,
            last: i64::MIN,
            durations: BTreeMap::new(),
            first_tokens: BTreeMap::new(),
        }
    }
}

impl Agg {
    fn merge(&mut self, other: &Self) {
        self.traces += other.traces;
        self.spans += other.spans;
        self.errors += other.errors;
        self.input += other.input;
        self.output += other.output;
        self.total += other.total;
        self.cost += other.cost;
        self.first = self.first.min(other.first);
        self.last = self.last.max(other.last);
        for (bucket, count) in &other.durations {
            *self.durations.entry(*bucket).or_default() += count;
        }
        for (bucket, count) in &other.first_tokens {
            *self.first_tokens.entry(*bucket).or_default() += count;
        }
    }
}

type Aggs = BTreeMap<Vec<u8>, Agg>;

/// A key of byte strings, each with its length, so that no two keys run together.
fn key(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(&(part.len() as u32).to_be_bytes());
        out.extend_from_slice(part);
    }
    out
}

fn add(aggs: &mut Aggs, key: Vec<u8>, agg: &Agg) {
    aggs.entry(key).or_default().merge(agg);
}

const ROLLUPS: [TableId; 4] = [
    TableId::TraceStats,
    TableId::SessionRollup,
    TableId::UserRollup,
    TableId::SpanMetrics,
];

/// The reference aggregation: what each rollup must hold, folded from the rows of `span_core`.
fn reference(model: &ChModel) -> Option<BTreeMap<TableId, Aggs>> {
    let mut out: BTreeMap<TableId, Aggs> = ROLLUPS.iter().map(|&t| (t, Aggs::new())).collect();
    for part in model.parts(TableId::SpanCore) {
        let block = &part.block;
        for row in 0..block.rows {
            let project = uuid_at(block, "project_id", row)?;
            let start = i64_at(block, "start_time", row)?;
            let end = i64_at(block, "end_time", row)?;
            let error = i8_at(block, "status_code", row)? == STATUS_ERROR
                || i8_at(block, "level", row)? == LEVEL_ERROR;
            let kind = i8_at(block, "observation_type", row)?;
            // Token and cost sums count every span that is not a usage aggregate, whatever its type.
            let counted = u8_at(block, "usage_aggregate", row)? == 0;
            let of = |value: u64| if counted { value } else { 0 };
            let sums = Agg {
                spans: 1,
                errors: u64::from(error),
                input: of(u64_at(block, "input_tokens", row)?),
                output: of(u64_at(block, "output_tokens", row)?),
                total: of(u64_at(block, "total_tokens", row)?),
                cost: if counted {
                    i64_at(block, "total_cost_nusd", row)?
                } else {
                    0
                },
                ..Agg::default()
            };
            let day = (start.div_euclid(86_400_000_000) as u16).to_be_bytes();
            let seen = Agg {
                traces: u64::from(u8_at(block, "is_root", row)? == 1),
                first: start,
                last: end,
                ..sums.clone()
            };

            let trace = bytes_at(block, "trace_id", row)?;
            let stats = Agg {
                first: start,
                last: end,
                ..sums.clone()
            };
            add(
                out.get_mut(&TableId::TraceStats)?,
                key(&[&project, trace, &day]),
                &stats,
            );

            let session = bytes_at(block, "session_id", row)?;
            if !session.is_empty() {
                let k = key(&[&project, &day, session]);
                add(out.get_mut(&TableId::SessionRollup)?, k, &seen);
            }
            let user = bytes_at(block, "user_id", row)?;
            if !user.is_empty() {
                add(
                    out.get_mut(&TableId::UserRollup)?,
                    key(&[&project, &day, user]),
                    &seen,
                );
            }

            let level = i8_at(block, "level", row)?;
            let minute = ((start.div_euclid(60_000_000) * 60) as u32).to_be_bytes();
            let mut metrics = sums;
            let duration = u64_at(block, "duration_us", row)?;
            metrics.durations.insert(Bucket::of(duration).index(), 1);
            if let (GENERATION, Some(first)) =
                (kind, nullable_i64_at(block, "completion_start_time", row)?)
            {
                let wait = u64::try_from(first.saturating_sub(start)).unwrap_or(0);
                metrics.first_tokens.insert(Bucket::of(wait).index(), 1);
            }
            let k = key(&[
                &project,
                &minute,
                bytes_at(block, "environment", row)?,
                &[kind as u8],
                bytes_at(block, "model_response", row)?,
                &[level as u8],
                bytes_at(block, "release", row)?,
            ]);
            add(out.get_mut(&TableId::SpanMetrics)?, k, &metrics);
        }
    }
    Some(out)
}

/// What a rollup table holds: its data rows merged by key, as a reader aggregates them.
fn stored(model: &ChModel, table: TableId) -> Option<Aggs> {
    let mut out = Aggs::new();
    for part in model.parts(table) {
        let block = &part.block;
        for row in 0..block.rows {
            let project = uuid_at(block, "project_id", row)?;
            if project == [0; 16] {
                // The marker row of the block.
                continue;
            }
            let count = if table == TableId::SpanMetrics {
                "count"
            } else {
                "span_count"
            };
            let mut agg = Agg {
                spans: u64_at(block, count, row)?,
                errors: u64_at(block, "error_count", row)?,
                input: u64_at(block, "input_tokens", row)?,
                output: u64_at(block, "output_tokens", row)?,
                total: u64_at(block, "total_tokens", row)?,
                cost: i64_at(block, "total_cost_nusd", row)?,
                ..Agg::default()
            };
            let day = |name| u16_at(block, name, row).map(u16::to_be_bytes);
            let k = match table {
                TableId::TraceStats => {
                    agg.first = i64_at(block, "min_start", row)?;
                    agg.last = i64_at(block, "max_end", row)?;
                    key(&[
                        &project,
                        bytes_at(block, "trace_id", row)?,
                        &day("row_day")?,
                    ])
                }
                TableId::SessionRollup | TableId::UserRollup => {
                    agg.traces = u64_at(block, "trace_count", row)?;
                    agg.first = i64_at(block, "first_seen", row)?;
                    agg.last = i64_at(block, "last_seen", row)?;
                    let id = if table == TableId::SessionRollup {
                        "session_id"
                    } else {
                        "user_id"
                    };
                    key(&[&project, &day("day")?, bytes_at(block, id, row)?])
                }
                _ => {
                    agg.durations = histogram_at(block, "dur_hist", row)?;
                    agg.first_tokens = histogram_at(block, "ttft_hist", row)?;
                    key(&[
                        &project,
                        &u32_at(block, "minute", row)?.to_be_bytes(),
                        bytes_at(block, "environment", row)?,
                        &[i8_at(block, "observation_type", row)? as u8],
                        bytes_at(block, "model_response", row)?,
                        &[i8_at(block, "level", row)? as u8],
                        bytes_at(block, "release", row)?,
                    ])
                }
            };
            add(&mut out, k, &agg);
        }
    }
    Some(out)
}

/// One `span_core` row as the usage-aggregate rule reads it.
struct UsageRow {
    lsn: u64,
    /// (project, trace, span id).
    key: Vec<u8>,
    /// (project, trace, parent span id); `None` for a span without a parent.
    parent: Option<Vec<u8>>,
    carries: bool,
    flagged: bool,
}

/// Runs the usage-aggregate rule over `rows` on top of `set`: the keys with a descendant that carries usage.
fn usage_parents(rows: &[&UsageRow], set: &mut BTreeSet<Vec<u8>>) {
    // A fixed point: a parent joins when a child carries usage or is in the set itself.
    loop {
        let before = set.len();
        for row in rows {
            if (row.carries || set.contains(&row.key))
                && let Some(parent) = &row.parent
            {
                set.insert(parent.clone());
            }
        }
        if set.len() == before {
            return;
        }
    }
}

/// I7 for `usage_aggregate`, from the rows of `span_core` alone. The flag is stage B state, rebuilt from the
/// retained WAL prefix after a restart, so a restart may forget an earlier frame's descendants. Stage B takes the
/// mark over every span of a frame before it drops the exact repeats, and a dropped repeat leaves no row in its
/// frame: it is a span whose equal row an earlier frame holds. What holds in every run:
///
/// - **never flagged without cause**: a flagged span has a descendant that carries usage or cost in its own
///   frame or in an earlier one, where a row of an earlier frame may stand in any later frame as a dropped
///   repeat and pass the mark on to its parent (the rule over the whole LSN stream, with every earlier row a
///   possible repeat);
/// - **always flagged within a frame**: a span with a descendant that carries usage or cost among the kept
///   spans of its own frame is flagged, whatever a restart forgot, and whatever the wire order.
fn check_usage_flags(model: &ChModel) -> Option<Vec<String>> {
    let mut rows = Vec::new();
    for part in model.parts(TableId::SpanCore) {
        let block = &part.block;
        for row in 0..block.rows {
            let project = uuid_at(block, "project_id", row)?;
            let trace = bytes_at(block, "trace_id", row)?;
            let parent = bytes_at(block, "parent_span_id", row)?;
            let span = bytes_at(block, "span_id", row)?;
            rows.push(UsageRow {
                lsn: u64_at(block, "lsn", row)?,
                key: key(&[&project, trace, span]),
                // A span that names itself as its parent is not its own descendant.
                parent: (parent.iter().any(|b| *b != 0) && parent != span)
                    .then(|| key(&[&project, trace, parent])),
                carries: u64_at(block, "input_tokens", row)? > 0
                    || u64_at(block, "output_tokens", row)? > 0
                    || u64_at(block, "total_tokens", row)? > 0
                    || i64_at(block, "total_cost_nusd", row)? > 0,
                flagged: u8_at(block, "usage_aggregate", row)? == 1,
            });
        }
    }
    rows.sort_by_key(|row| row.lsn);
    let mut wrong = Vec::new();
    let mut stream = BTreeSet::new();
    let mut seen: Vec<&UsageRow> = Vec::new();
    for frame in rows.chunk_by(|a, b| a.lsn == b.lsn) {
        let frame: Vec<&UsageRow> = frame.iter().collect();
        let mut alone = BTreeSet::new();
        usage_parents(&frame, &mut alone);
        // Every row up to this frame: the frame's own, and the earlier ones as its possible dropped repeats.
        seen.extend(&frame);
        usage_parents(&seen, &mut stream);
        for row in &frame {
            if row.flagged && !stream.contains(&row.key) {
                wrong.push(format!(
                    "a span of frame {} is a usage aggregate without a descendant that carries usage",
                    row.lsn
                ));
            }
            if !row.flagged && alone.contains(&row.key) {
                wrong.push(format!(
                    "a span of frame {} has a descendant in its frame that carries usage and is not a usage \
                     aggregate",
                    row.lsn
                ));
            }
        }
    }
    Some(wrong)
}

/// I7: compares every rollup with the reference aggregation over `span_core`, and the usage-aggregate flags
/// with the rule. Returns the number of keys
/// compared and what differs. Valid once every pair of every batch has landed.
pub(crate) fn check_aggregates(model: &ChModel) -> (u64, Vec<String>) {
    let Some(reference) = reference(model) else {
        return (0, vec!["a span_core row does not read".to_owned()]);
    };
    let mut keys = 0;
    let Some(mut wrong) = check_usage_flags(model) else {
        return (0, vec!["a span_core row does not read".to_owned()]);
    };
    for (table, expected) in reference {
        let Some(found) = stored(model, table) else {
            wrong.push(format!("a row of {} does not read", table.name()));
            continue;
        };
        keys += expected.len() as u64;
        if found == expected {
            continue;
        }
        let differing = expected
            .iter()
            .find(|(k, agg)| found.get(*k) != Some(agg))
            .map(|(k, agg)| format!("expected {agg:?}, found {:?}", found.get(k)))
            .unwrap_or_else(|| {
                format!(
                    "{} keys that no span explains",
                    found.len().saturating_sub(expected.len())
                )
            });
        wrong.push(format!(
            "{} differs from the reference over span_core in {} of {} keys: {differing}",
            table.name(),
            expected
                .iter()
                .filter(|(k, agg)| found.get(*k) != Some(agg))
                .count()
                + found.keys().filter(|k| !expected.contains_key(*k)).count(),
            expected.len(),
        ));
    }
    (keys, wrong)
}

/// The fact tables.
pub(crate) const FACT_TABLES: [TableId; 4] = [
    TableId::FactScores,
    TableId::FactTags,
    TableId::FactFlags,
    TableId::FactCorrections,
];

/// What resolution reads of one fact row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct FactRow {
    pub(crate) table: TableId,
    pub(crate) project: [u8; 16],
    pub(crate) fact_id: [u8; 16],
    pub(crate) version: u64,
    pub(crate) retracted: bool,
    pub(crate) key: Vec<u8>,
    pub(crate) trace: [u8; 16],
    pub(crate) span: [u8; 8],
    pub(crate) session: Vec<u8>,
}

/// Every row of the fact tables, in part order. `None` when a row does not read.
pub(crate) fn fact_rows(model: &ChModel) -> Option<Vec<FactRow>> {
    let mut rows = Vec::new();
    for table in FACT_TABLES {
        for part in model.parts(table) {
            let block = &part.block;
            for row in 0..block.rows {
                rows.push(FactRow {
                    table,
                    project: uuid_at(block, "project_id", row)?,
                    fact_id: uuid_at(block, "fact_id", row)?,
                    version: u64_at(block, "version", row)?,
                    retracted: u8_at(block, "retracted", row)? == 1,
                    key: bytes_at(block, "key", row)?.to_vec(),
                    trace: bytes_at(block, "trace_id", row)?.try_into().ok()?,
                    span: bytes_at(block, "span_id", row)?.try_into().ok()?,
                    session: bytes_at(block, "session_id", row)?.to_vec(),
                });
            }
        }
    }
    Some(rows)
}

/// The reference fold of invariant I4 over fact rows: per (table, target, key) the id of the fact that holds
/// the value, through `crucible_records::fold_facts`.
pub(crate) fn resolve<'a>(
    rows: impl IntoIterator<Item = &'a FactRow>,
) -> BTreeMap<(TableId, Vec<u8>), [u8; 16]> {
    fold_facts(rows.into_iter().map(|row| {
        let target = key(&[&row.project, &row.trace, &row.span, &row.session, &row.key]);
        let version = FactVersion {
            version: row.version,
            fact_id: FactId::from_bytes(row.fact_id),
            retracted: row.retracted,
        };
        ((row.table, target), version, row.fact_id)
    }))
}
