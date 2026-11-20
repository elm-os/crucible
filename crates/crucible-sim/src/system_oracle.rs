//! The reference side of the system simulation's oracle for the tables of milestones 2 and 3: the reference
//! aggregation that the rollups must equal exactly (invariant I7; token and cost sums leave out the usage
//! aggregates, whose flags are checked against the rule itself, run over every span that stage B was given), the
//! fact rows with their reference resolution (invariants I2 and I4), and the MCP tables: every `mcp_ops` row
//! against the exchange the generator simulated (invariant I8 and the wire contract of 9.1), and the MCP
//! aggregating tables against the reference over `mcp_ops`, per capture source (invariants I2 and I7).
//! The store's side is read from the parts of [`ChModel`], as the store holds them.
//!
//! It also holds the oracle's model of what a deletion request selects (invariant I9, contract 13.3): a linear
//! reading of the rule for one request, which shares nothing with stage B's index of deletions. Pairs of one batch
//! may land before and after a restart that knows more deletions (a replay drops more, never fewer, contract 5.3),
//! so the exact comparisons leave out the project whose data the run deletes.

use std::collections::{BTreeMap, BTreeSet};

use crucible_chfmt::{DecodedBlock, Values};
use crucible_normalize::{ProjectSalt, Tombstone, TombstoneSelector};
use crucible_records::{
    Bucket, CaptureSource, Era, FactId, FactVersion, GroupKeyKind, McpClassification, McpSide,
    TableId, Transport, fold_facts, semconv_bucket,
};
use sha2::{Digest, Sha256};
use xxhash_rust::xxh3::xxh3_64;

use crate::ch_model::{
    ChModel, bytes_at, histogram_at, i8_at, i64_at, nullable_i64_at, u8_at, u16_at, u32_at, u64_at,
    uuid_at,
};

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

/// The reference aggregation: what each rollup must hold, folded from the rows of `span_core`, without the
/// rows of the project `skip`.
fn reference(model: &ChModel, skip: Option<[u8; 16]>) -> Option<BTreeMap<TableId, Aggs>> {
    let mut out: BTreeMap<TableId, Aggs> = ROLLUPS.iter().map(|&t| (t, Aggs::new())).collect();
    for part in model.parts(TableId::SpanCore) {
        let block = &part.block;
        for row in 0..block.rows {
            let project = uuid_at(block, "project_id", row)?;
            if Some(project) == skip {
                continue;
            }
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

/// What a rollup table holds: its data rows merged by key, as a reader aggregates them, without the rows of the
/// project `skip`.
fn stored(model: &ChModel, table: TableId, skip: Option<[u8; 16]>) -> Option<Aggs> {
    let mut out = Aggs::new();
    for part in model.parts(table) {
        let block = &part.block;
        for row in 0..block.rows {
            let project = uuid_at(block, "project_id", row)?;
            if project == [0; 16] || Some(project) == skip {
                // The marker row of the block, or the project left out.
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

/// One span of a frame as stage A made it, kept or not: what the usage-aggregate rule reads of it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct FrameSpan {
    /// (project, trace, span id).
    pub(crate) key: Vec<u8>,
    /// (project, trace, parent span id); `None` for a span without a parent or one that names itself.
    pub(crate) parent: Option<Vec<u8>>,
    /// Whether the span reports usage or a cost, as the generator sent it.
    pub(crate) carries: bool,
}

impl FrameSpan {
    /// A span of `project` as the wire holds it.
    pub(crate) fn new(
        project: &[u8; 16],
        trace: &[u8],
        span: &[u8],
        parent: Option<&[u8]>,
        carries: bool,
    ) -> Self {
        Self {
            key: key(&[project, trace, span]),
            parent: parent
                .filter(|parent| parent.iter().any(|b| *b != 0) && *parent != span)
                .map(|parent| key(&[project, trace, parent])),
            carries,
        }
    }
}

/// What stage B was given, as the simulation saw it through the shipper's hook: every span of every frame it
/// admitted, the exact repeats it then dropped included, and for each admission whose rows could ship the first
/// LSN the WAL retained at that moment.
#[derive(Debug, Default)]
pub(crate) struct Admitted {
    /// The spans of each `IngestRequest` frame that stage A made records of, in wire order, by LSN.
    pub(crate) frames: BTreeMap<u64, Vec<FrameSpan>>,
    /// (LSN, first retained LSN) of every admission above the watermark of its incarnation.
    pub(crate) admissions: BTreeSet<(u64, u64)>,
    /// What the first admission of each frame above the watermark kept: the stage A ordinal of each remaining
    /// span and whether it was flagged a usage aggregate.
    pub(crate) kept: BTreeMap<u64, Vec<(u32, bool)>>,
    /// For each frame above the watermark whose admission used an earlier frame, the LSN of that frame
    /// (`Admission::relies_on`).
    pub(crate) relying: BTreeMap<u64, u64>,
}

/// The usage aggregates of the frame at `lsn` when stage B holds what the frames from `floor` on give it
/// (contract 5.3, usage descendant set): the set starts empty at `floor`; in LSN order, every span of a frame
/// that carries usage or is in the set adds its parent, to a fixed point over all the spans of the frame, the
/// dropped repeats included; a span of the frame at `lsn` is an aggregate when it is in the set then.
fn usage_aggregates(
    frames: &BTreeMap<u64, Vec<FrameSpan>>,
    floor: u64,
    targets: &BTreeSet<u64>,
) -> BTreeMap<u64, BTreeSet<Vec<u8>>> {
    let mut set: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut out = BTreeMap::new();
    let Some(&last) = targets.last() else {
        return out;
    };
    for (&lsn, spans) in frames.range(floor..=last) {
        loop {
            let before = set.len();
            for span in spans {
                if (span.carries || set.contains(&span.key))
                    && let Some(parent) = &span.parent
                {
                    set.insert(parent.clone());
                }
            }
            if set.len() == before {
                break;
            }
        }
        if targets.contains(&lsn) {
            let flagged = spans
                .iter()
                .filter(|span| set.contains(&span.key))
                .map(|span| span.key.clone())
                .collect();
            out.insert(lsn, flagged);
        }
    }
    out
}

/// I7 for `usage_aggregate`, exactly. The flag is stage B state, and stage B holds what the retained prefix of
/// the WAL holds (contract 5.3). The simulation knows every span of every frame that stage B admitted, the
/// dropped repeats included, and the first retained LSN at each admission, so it runs the rule itself:
///
/// - every `span_core` row carries the flag that the rule gives its span, over all the spans of its frame and
///   of the frames from the first retained LSN on, before any repeat is dropped;
/// - two admissions of one frame above the watermark (before and after a restart) give the same aggregates,
///   whatever garbage collection removed in between, because the WAL keeps what an unchecked frame relied on.
fn check_usage_flags(model: &ChModel, admitted: &Admitted) -> Option<Vec<String>> {
    let mut by_floor: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    for &(lsn, floor) in &admitted.admissions {
        by_floor.entry(floor).or_default().insert(lsn);
    }
    let mut wrong = Vec::new();
    let mut expected: BTreeMap<u64, BTreeSet<Vec<u8>>> = BTreeMap::new();
    let mut split: BTreeSet<u64> = BTreeSet::new();
    for (floor, targets) in &by_floor {
        for (lsn, flagged) in usage_aggregates(&admitted.frames, *floor, targets) {
            match expected.get(&lsn) {
                Some(earlier) if *earlier != flagged => {
                    split.insert(lsn);
                }
                Some(_) => {}
                None => {
                    expected.insert(lsn, flagged);
                }
            }
        }
    }
    for lsn in split {
        wrong.push(format!(
            "two admissions of frame {lsn} give different usage aggregates: the retained prefix lost a \
             frame that the first admission relied on"
        ));
    }
    for part in model.parts(TableId::SpanCore) {
        let block = &part.block;
        for row in 0..block.rows {
            let lsn = u64_at(block, "lsn", row)?;
            let project = uuid_at(block, "project_id", row)?;
            let span_key = key(&[
                &project,
                bytes_at(block, "trace_id", row)?,
                bytes_at(block, "span_id", row)?,
            ]);
            let flagged = u8_at(block, "usage_aggregate", row)? == 1;
            match expected.get(&lsn) {
                None => wrong.push(format!(
                    "a span_core row of frame {lsn}, which stage B did not admit above the watermark"
                )),
                Some(aggregates) if aggregates.contains(&span_key) != flagged => {
                    wrong.push(format!(
                        "a span of frame {lsn} is {}a usage aggregate, and the rule over the frames that \
                         stage B held says the opposite",
                        if flagged { "" } else { "not " }
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Some(wrong)
}

/// What an MCP operation is, as the generator simulated the exchange: the oracle's expectation for its
/// `mcp_ops` row, its `mcp_content` row and its tool inventory entries.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct McpSent {
    pub(crate) side: McpSide,
    pub(crate) capture_source: CaptureSource,
    pub(crate) era: Era,
    pub(crate) transport: Transport,
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) server: String,
    pub(crate) version: String,
    /// The deploy id that the operation's resource names; empty when it names none.
    pub(crate) deploy: String,
    pub(crate) instance: String,
    pub(crate) client: String,
    pub(crate) notification: bool,
    pub(crate) start_micros: i64,
    pub(crate) duration_us: u64,
    pub(crate) request_bytes: u64,
    pub(crate) response_bytes: u64,
    /// The parent span id; all zero for a span without one.
    pub(crate) parent: [u8; 8],
    /// What the shared classifier gives for the exchange (contract 9.2).
    pub(crate) outcome: McpClassification,
    pub(crate) jsonrpc_code: Option<i32>,
    pub(crate) http_status: Option<u16>,
    pub(crate) round: u16,
    pub(crate) state_in: Option<[u8; 16]>,
    pub(crate) state_out: Option<[u8; 16]>,
    /// The raw session id the sender observed, in the form it sent: hashed (`true`) or raw.
    pub(crate) session: Option<(String, bool)>,
    pub(crate) handle: String,
    /// The user's token issuer and subject, as the sender sent them raw.
    pub(crate) subject: Option<(String, String)>,
    pub(crate) user_id: String,
    /// The sizes of the captured arguments and result, when content was captured.
    pub(crate) content: Option<(u64, u64)>,
    /// The tools the operation lists: each name and the canonical JSON of its input and output schemas.
    pub(crate) tools: Vec<(String, String, String)>,
}

impl McpSent {
    /// The deploy id of the record (contract 9.7): the resource's, else the server version.
    fn deploy_id(&self) -> &str {
        if self.deploy.is_empty() {
            &self.version
        } else {
            &self.deploy
        }
    }

    /// The session key of the record (contract 9.4): in the legacy era, the first 16 bytes of SHA-256 of the
    /// session id, whether the sender hashed it or the normalizer did.
    fn session_key(&self) -> Option<[u8; 16]> {
        let (id, _) = self.session.as_ref().filter(|_| self.era == Era::Legacy)?;
        let mut out = [0u8; 16];
        out.copy_from_slice(&Sha256::digest(id.as_bytes())[..16]);
        Some(out)
    }

    /// The group key of the record (contract 9.4): the first rule that matches.
    fn group(&self, trace: &[u8]) -> (GroupKeyKind, String) {
        if let Some(key) = self.session_key() {
            (GroupKeyKind::LegacySession, hex(&key))
        } else if !self.handle.is_empty() {
            (GroupKeyKind::Handle, self.handle.clone())
        } else if self.parent != [0; 8] {
            (GroupKeyKind::Trace, hex(trace))
        } else {
            (GroupKeyKind::Process, self.instance.clone())
        }
    }

    /// The schema hash of a listed tool (contract 3.6), from the canonical JSON of its schemas.
    fn schema_hash(input: &str, output: &str) -> u64 {
        let mut bytes = input.as_bytes().to_vec();
        bytes.push(0);
        bytes.extend_from_slice(output.as_bytes());
        xxh3_64(&bytes)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether any text cell of `block` holds `needle`: every `String` and `FixedString` value, also inside arrays,
/// tuples and nullable columns. The oracle looks for the values the generator planted under keys that must never
/// be stored (contracts 3.5, 9.3 and 9.4).
pub(crate) fn holds_text(block: &DecodedBlock, needle: &[u8]) -> bool {
    fn any(values: &Values, needle: &[u8]) -> bool {
        match values {
            Values::String(cells) | Values::FixedString(_, cells) => cells
                .iter()
                .any(|cell| cell.windows(needle.len()).any(|w| w == needle)),
            Values::Nullable(_, inner) | Values::Array(_, inner) => any(inner, needle),
            Values::Tuple(items) => items.iter().any(|items| any(items, needle)),
            _ => false,
        }
    }
    !needle.is_empty() && block.columns.iter().any(|c| any(&c.values, needle))
}

fn values<'a>(block: &'a DecodedBlock, name: &str) -> Option<&'a Values> {
    block
        .columns
        .iter()
        .find(|c| c.name == name)
        .map(|c| &c.values)
}

/// A `Nullable(Int32)` or `Nullable(UInt16)` cell as an `i64`: `Some(None)` for a null.
fn nullable_at(block: &DecodedBlock, name: &str, row: usize) -> Option<Option<i64>> {
    let Values::Nullable(nulls, inner) = values(block, name)? else {
        return None;
    };
    if *nulls.get(row)? {
        return Some(None);
    }
    match &**inner {
        Values::Int32(v) => v.get(row).map(|v| Some(i64::from(*v))),
        Values::UInt16(v) => v.get(row).map(|v| Some(i64::from(*v))),
        Values::UInt32(v) => v.get(row).map(|v| Some(i64::from(*v))),
        _ => None,
    }
}

/// The elements of one row of an array column, as a range into its elements.
fn elements(ends: &[u64], row: usize) -> Option<core::ops::Range<usize>> {
    let start = if row == 0 {
        0
    } else {
        *ends.get(row - 1)? as usize
    };
    Some(start..*ends.get(row)? as usize)
}

/// A `sumMap` cell with `UInt8` or `UInt16` keys, as key to count.
fn sum_map_at(block: &DecodedBlock, name: &str, row: usize) -> Option<BTreeMap<u16, u64>> {
    let Values::Tuple(pair) = values(block, name)? else {
        return None;
    };
    let (Values::Array(key_ends, keys), Values::Array(count_ends, counts)) =
        (pair.first()?, pair.get(1)?)
    else {
        return None;
    };
    let Values::UInt64(counts) = &**counts else {
        return None;
    };
    let counts = counts.get(elements(count_ends, row)?)?;
    let range = elements(key_ends, row)?;
    let keys: Vec<u16> = match &**keys {
        Values::UInt8(keys) => keys.get(range)?.iter().map(|k| u16::from(*k)).collect(),
        Values::UInt16(keys) => keys.get(range)?.to_vec(),
        _ => return None,
    };
    Some(keys.into_iter().zip(counts.iter().copied()).collect())
}

/// An `Array(UInt8)` cell as a set.
fn u8_set_at(block: &DecodedBlock, name: &str, row: usize) -> Option<BTreeSet<u8>> {
    let Values::Array(ends, items) = values(block, name)? else {
        return None;
    };
    let Values::UInt8(items) = &**items else {
        return None;
    };
    Some(items.get(elements(ends, row)?)?.iter().copied().collect())
}

/// What differs between an `mcp_ops` row and the operation the generator sent.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct McpDiff {
    /// I8: the classification columns that are not what the shared classifier gives for the exchange.
    pub(crate) classification: Vec<&'static str>,
    /// The other columns that are not what the exchange was (contracts 9.1 and 9.3 to 9.8).
    pub(crate) fields: Vec<&'static str>,
}

/// Compares one `mcp_ops` row with the operation the generator sent. `salt` is the salt of the row's project,
/// or `None` when the run has no control store, so that every identity key must be absent (contract 9.3).
pub(crate) fn mcp_row_matches(
    block: &DecodedBlock,
    row: usize,
    sent: &McpSent,
    salt: Option<&ProjectSalt>,
) -> Option<McpDiff> {
    let mut diff = McpDiff::default();
    let text = |name: &str| bytes_at(block, name, row);
    let en = |name: &str| i8_at(block, name, row).map(|v| v as u8);
    let o = &sent.outcome;
    let classification = [
        ("status", en("status")? == o.status.value()),
        ("error_class", en("error_class")? == o.error_class.value()),
        (
            "error_subclass",
            en("error_subclass")? == o.error_subclass.value(),
        ),
        (
            "caller_fault",
            u8_at(block, "caller_fault", row)? == u8::from(o.caller_fault),
        ),
        (
            "jsonrpc_code",
            nullable_at(block, "jsonrpc_code", row)? == sent.jsonrpc_code.map(i64::from),
        ),
        (
            "http_status",
            nullable_at(block, "http_status", row)? == sent.http_status.map(i64::from),
        ),
    ];
    diff.classification = classification
        .iter()
        .filter(|(_, same)| !same)
        .map(|(name, _)| *name)
        .collect();

    let trace = text("trace_id")?;
    let (group_kind, group_key) = sent.group(trace);
    let hash = |value: Option<[u8; 16]>| value.unwrap_or_default();
    let subject_hash = match (&sent.subject, salt) {
        (Some((issuer, subject)), Some(salt)) => Some(salt.subject_hash(issuer, subject)),
        _ => None,
    };
    // Without a control store the identity keys are dropped; with one, an issuer stays with its subject.
    let user_id = if salt.is_some() { &*sent.user_id } else { "" };
    let issuer = match (&sent.subject, salt) {
        (Some((issuer, _)), Some(_)) => issuer.as_str(),
        _ => "",
    };
    let fields = [
        ("side", en("side")? == sent.side.value()),
        (
            "capture_source",
            en("capture_source")? == sent.capture_source.value(),
        ),
        ("era", en("era")? == sent.era.value()),
        ("transport", en("transport")? == sent.transport.value()),
        ("method", text("method")? == sent.method.as_bytes()),
        (
            "target_name",
            text("target_name")? == sent.target.as_bytes(),
        ),
        (
            "server_name",
            text("server_name")? == sent.server.as_bytes(),
        ),
        (
            "server_version",
            text("server_version")? == sent.version.as_bytes(),
        ),
        (
            "deploy_id",
            text("deploy_id")? == sent.deploy_id().as_bytes(),
        ),
        (
            "instance_id",
            text("instance_id")? == sent.instance.as_bytes(),
        ),
        (
            "client_name",
            text("client_name")? == sent.client.as_bytes(),
        ),
        (
            "is_notification",
            u8_at(block, "is_notification", row)? == u8::from(sent.notification),
        ),
        (
            "start_time",
            i64_at(block, "start_time", row)? == sent.start_micros,
        ),
        (
            "duration_us",
            u64_at(block, "duration_us", row)? == sent.duration_us,
        ),
        (
            "request_bytes",
            u64_at(block, "request_bytes", row)? == sent.request_bytes,
        ),
        (
            "response_bytes",
            u64_at(block, "response_bytes", row)? == sent.response_bytes,
        ),
        ("parent_span_id", text("parent_span_id")? == sent.parent),
        (
            "mrtr_round",
            u16_at(block, "mrtr_round", row)? == sent.round,
        ),
        (
            "mrtr_state_in",
            text("mrtr_state_in")? == hash(sent.state_in),
        ),
        (
            "mrtr_state_out",
            text("mrtr_state_out")? == hash(sent.state_out),
        ),
        (
            "session_key",
            text("session_key")?
                == sent
                    .session_key()
                    .map(|key| hex(&key))
                    .unwrap_or_default()
                    .as_bytes(),
        ),
        ("handle_id", text("handle_id")? == sent.handle.as_bytes()),
        (
            "group_key_kind",
            en("group_key_kind")? == group_kind.value(),
        ),
        ("group_key", text("group_key")? == group_key.as_bytes()),
        (
            "user_subject_hash",
            text("user_subject_hash")? == hash(subject_hash),
        ),
        ("user_issuer", text("user_issuer")? == issuer.as_bytes()),
        ("user_id", text("user_id")? == user_id.as_bytes()),
        (
            "content_captured",
            u8_at(block, "content_captured", row)? == u8::from(sent.content.is_some()),
        ),
    ];
    diff.fields = fields
        .iter()
        .filter(|(_, same)| !same)
        .map(|(name, _)| *name)
        .collect();
    Some(diff)
}

/// The key of an MCP operation in the ledger: (project id, trace id, span id).
pub(crate) type McpKey = ([u8; 16], [u8; 16], [u8; 8]);

/// The measures of one `mcp_tool_rollup` key.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct ToolAgg {
    count: u64,
    request_bytes: u64,
    response_bytes: u64,
    durations: BTreeMap<u16, u64>,
    semconv: BTreeMap<u16, u64>,
    last_seen: i64,
}

/// First and last seen, and for the inventory the capture sources.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Seen {
    first: i64,
    last: i64,
    sources: BTreeSet<u8>,
}

impl Seen {
    fn at(time: i64) -> Self {
        Self {
            first: time,
            last: time,
            sources: BTreeSet::new(),
        }
    }

    fn merge(&mut self, other: &Self) {
        self.first = self.first.min(other.first);
        self.last = self.last.max(other.last);
        self.sources.extend(&other.sources);
    }
}

fn differing<K: Ord + core::fmt::Debug, V: PartialEq + core::fmt::Debug>(
    table: TableId,
    expected: &BTreeMap<K, V>,
    found: &BTreeMap<K, V>,
    wrong: &mut Vec<String>,
) {
    if expected == found {
        return;
    }
    let example = expected
        .iter()
        .find(|(k, v)| found.get(*k) != Some(v))
        .map(|(_, v)| format!("expected {v:?}, found another value or none"))
        .unwrap_or_else(|| "keys that no operation explains".to_owned());
    wrong.push(format!(
        "{} differs from the reference in {} of {} keys: {example}",
        table.name(),
        expected
            .iter()
            .filter(|(k, v)| found.get(*k) != Some(v))
            .count()
            + found.keys().filter(|k| !expected.contains_key(*k)).count(),
        expected.len(),
    ));
}

/// I7 and I2 for the MCP tables, once every pair of every batch has landed. Returns the number of keys
/// compared and what differs.
///
/// - `mcp_ops` holds one row for each `span_core` row of a span with an MCP side, and `mcp_content` one row
///   for each `mcp_ops` row whose content was captured (I2).
/// - `mcp_tool_rollup`, merged by its key, equals the reference aggregation over `mcp_ops`. The capture
///   source is part of the key, so that each source has its own counts, and the rollup counts per capture
///   source what `mcp_ops` holds of that source: rows of different sources are never summed (contract 9.5).
/// - `mcp_deploys`, merged by its key, equals the deploys of the server-side rows of `mcp_ops`.
/// - `mcp_tool_inventory`, merged by its key, equals the tools that the operations in `mcp_ops` listed, as
///   the generator sent them.
///
/// The rows of the project `skip` are left out of every table.
pub(crate) fn check_mcp(
    model: &ChModel,
    sent: &BTreeMap<McpKey, McpSent>,
    skip: Option<[u8; 16]>,
) -> (u64, Vec<String>) {
    let unread = |table: TableId| (0, vec![format!("a row of {} does not read", table.name())]);
    let mut wrong = Vec::new();
    let skipped =
        |block: &DecodedBlock, row: usize| Some(Some(uuid_at(block, "project_id", row)?) == skip);

    // The rows of a plain table by (LSN, span id).
    let rows_of = |table: TableId, only: &dyn Fn(&DecodedBlock, usize) -> Option<bool>| {
        let mut rows: BTreeSet<(u64, Vec<u8>)> = BTreeSet::new();
        for part in model.parts(table) {
            let block = &part.block;
            for row in 0..block.rows {
                if !skipped(block, row)? && only(block, row)? {
                    rows.insert((
                        u64_at(block, "lsn", row)?,
                        bytes_at(block, "span_id", row)?.to_vec(),
                    ));
                }
            }
        }
        Some(rows)
    };
    let Some(mcp_spans) = rows_of(TableId::SpanCore, &|block, row| {
        Some(i8_at(block, "mcp_side", row)? != 0)
    }) else {
        return unread(TableId::SpanCore);
    };
    let Some(ops) = rows_of(TableId::McpOps, &|_, _| Some(true)) else {
        return unread(TableId::McpOps);
    };
    let Some(captured) = rows_of(TableId::McpOps, &|block, row| {
        Some(u8_at(block, "content_captured", row)? == 1)
    }) else {
        return unread(TableId::McpOps);
    };
    let Some(content) = rows_of(TableId::McpContent, &|_, _| Some(true)) else {
        return unread(TableId::McpContent);
    };
    if ops != mcp_spans {
        wrong.push(format!(
            "mcp_ops holds {} rows and span_core holds {} rows of spans with an MCP side; {} differ",
            ops.len(),
            mcp_spans.len(),
            ops.symmetric_difference(&mcp_spans).count()
        ));
    }
    if content != captured {
        wrong.push(format!(
            "mcp_content holds {} rows and mcp_ops holds {} rows with captured content",
            content.len(),
            captured.len()
        ));
    }

    // The references, folded from the rows of `mcp_ops`.
    let mut rollup: BTreeMap<Vec<u8>, ToolAgg> = BTreeMap::new();
    let mut per_source: BTreeMap<u8, u64> = BTreeMap::new();
    let mut deploys: BTreeMap<Vec<u8>, Seen> = BTreeMap::new();
    let mut inventory: BTreeMap<Vec<u8>, Seen> = BTreeMap::new();
    let folded = (|| {
        for part in model.parts(TableId::McpOps) {
            let block = &part.block;
            for row in 0..block.rows {
                let project = uuid_at(block, "project_id", row)?;
                if Some(project) == skip {
                    continue;
                }
                let side = i8_at(block, "side", row)? as u8;
                let source = i8_at(block, "capture_source", row)? as u8;
                let start = i64_at(block, "start_time", row)?;
                let minute = ((start.div_euclid(60_000_000) * 60) as u32).to_be_bytes();
                let server = bytes_at(block, "server_name", row)?;
                let deploy = bytes_at(block, "deploy_id", row)?;
                let duration = u64_at(block, "duration_us", row)?;
                let k = key(&[
                    &project,
                    &[side],
                    &minute,
                    server,
                    deploy,
                    &[source],
                    &[i8_at(block, "era", row)? as u8],
                    bytes_at(block, "method", row)?,
                    bytes_at(block, "target_name", row)?,
                    bytes_at(block, "client_name", row)?,
                    &[i8_at(block, "error_class", row)? as u8],
                    &[u8_at(block, "caller_fault", row)?],
                ]);
                let agg = rollup.entry(k).or_insert_with(|| ToolAgg {
                    last_seen: i64::MIN,
                    ..ToolAgg::default()
                });
                agg.count += 1;
                agg.request_bytes += u64_at(block, "request_bytes", row)?;
                agg.response_bytes += u64_at(block, "response_bytes", row)?;
                *agg.durations
                    .entry(Bucket::of(duration).index())
                    .or_default() += 1;
                *agg.semconv
                    .entry(u16::from(semconv_bucket(duration)))
                    .or_default() += 1;
                agg.last_seen = agg.last_seen.max(start);
                *per_source.entry(source).or_default() += 1;
                if side == McpSide::Server.value() {
                    let k = key(&[
                        &project,
                        server,
                        deploy,
                        bytes_at(block, "server_version", row)?,
                        &[source],
                    ]);
                    deploys
                        .entry(k)
                        .and_modify(|seen| seen.merge(&Seen::at(start)))
                        .or_insert_with(|| Seen::at(start));
                }
                let op: McpKey = (
                    project,
                    bytes_at(block, "trace_id", row)?.try_into().ok()?,
                    bytes_at(block, "span_id", row)?.try_into().ok()?,
                );
                for (name, input, output) in sent.get(&op).map_or(&[][..], |s| &s.tools) {
                    let hash = McpSent::schema_hash(input, output).to_be_bytes();
                    let k = key(&[&project, server, deploy, name.as_bytes(), &hash]);
                    let mut seen = Seen::at(start);
                    seen.sources.insert(source);
                    inventory
                        .entry(k)
                        .and_modify(|earlier| earlier.merge(&seen))
                        .or_insert(seen);
                }
            }
        }
        Some(())
    })();
    if folded.is_none() {
        return unread(TableId::McpOps);
    }

    // What the aggregating tables hold: their data rows merged by key, as a reader aggregates them.
    let mut stored_rollup: BTreeMap<Vec<u8>, ToolAgg> = BTreeMap::new();
    let mut stored_per_source: BTreeMap<u8, u64> = BTreeMap::new();
    let read = (|| {
        for part in model.parts(TableId::McpToolRollup) {
            let block = &part.block;
            for row in 0..block.rows {
                let project = uuid_at(block, "project_id", row)?;
                if project == [0; 16] || Some(project) == skip {
                    continue;
                }
                let source = i8_at(block, "capture_source", row)? as u8;
                let k = key(&[
                    &project,
                    &[i8_at(block, "side", row)? as u8],
                    &u32_at(block, "minute", row)?.to_be_bytes(),
                    bytes_at(block, "server_name", row)?,
                    bytes_at(block, "deploy_id", row)?,
                    &[source],
                    &[i8_at(block, "era", row)? as u8],
                    bytes_at(block, "method", row)?,
                    bytes_at(block, "target_name", row)?,
                    bytes_at(block, "client_name", row)?,
                    &[i8_at(block, "error_class", row)? as u8],
                    &[u8_at(block, "caller_fault", row)?],
                ]);
                let count = u64_at(block, "count", row)?;
                let agg = stored_rollup.entry(k).or_insert_with(|| ToolAgg {
                    last_seen: i64::MIN,
                    ..ToolAgg::default()
                });
                agg.count += count;
                agg.request_bytes += u64_at(block, "request_bytes", row)?;
                agg.response_bytes += u64_at(block, "response_bytes", row)?;
                for (bucket, n) in sum_map_at(block, "dur_hist", row)? {
                    *agg.durations.entry(bucket).or_default() += n;
                }
                for (bucket, n) in sum_map_at(block, "semconv_hist", row)? {
                    *agg.semconv.entry(bucket).or_default() += n;
                }
                agg.last_seen = agg.last_seen.max(i64_at(block, "last_seen", row)?);
                *stored_per_source.entry(source).or_default() += count;
            }
        }
        Some(())
    })();
    if read.is_none() {
        return unread(TableId::McpToolRollup);
    }
    let seen_rows = |table: TableId, keys: &[&str], with_sources: bool| {
        let mut out: BTreeMap<Vec<u8>, Seen> = BTreeMap::new();
        for part in model.parts(table) {
            let block = &part.block;
            for row in 0..block.rows {
                let project = uuid_at(block, "project_id", row)?;
                if project == [0; 16] || Some(project) == skip {
                    continue;
                }
                let mut parts: Vec<Vec<u8>> = vec![project.to_vec()];
                for name in keys {
                    parts.push(match *name {
                        "capture_source" => vec![i8_at(block, name, row)? as u8],
                        "schema_hash" => u64_at(block, name, row)?.to_be_bytes().to_vec(),
                        _ => bytes_at(block, name, row)?.to_vec(),
                    });
                }
                let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
                let seen = Seen {
                    first: i64_at(block, "first_seen", row)?,
                    last: i64_at(block, "last_seen", row)?,
                    sources: if with_sources {
                        u8_set_at(block, "capture_sources", row)?
                    } else {
                        BTreeSet::new()
                    },
                };
                out.entry(key(&parts))
                    .and_modify(|earlier| earlier.merge(&seen))
                    .or_insert(seen);
            }
        }
        Some(out)
    };
    let Some(stored_deploys) = seen_rows(
        TableId::McpDeploys,
        &[
            "server_name",
            "deploy_id",
            "server_version",
            "capture_source",
        ],
        false,
    ) else {
        return unread(TableId::McpDeploys);
    };
    let Some(stored_inventory) = seen_rows(
        TableId::McpToolInventory,
        &["server_name", "deploy_id", "tool_name", "schema_hash"],
        true,
    ) else {
        return unread(TableId::McpToolInventory);
    };

    differing(TableId::McpToolRollup, &rollup, &stored_rollup, &mut wrong);
    if per_source != stored_per_source {
        wrong.push(format!(
            "mcp_tool_rollup counts {stored_per_source:?} operations per capture source and mcp_ops holds \
             {per_source:?}: rows of different capture sources were summed"
        ));
    }
    differing(TableId::McpDeploys, &deploys, &stored_deploys, &mut wrong);
    differing(
        TableId::McpToolInventory,
        &inventory,
        &stored_inventory,
        &mut wrong,
    );
    let keys = (rollup.len() + deploys.len() + inventory.len()) as u64;
    (keys, wrong)
}

/// I7: compares every rollup with the reference aggregation over `span_core`, and the usage-aggregate flags
/// with the rule over the frames that stage B was given. Returns the number of keys
/// compared and what differs. Valid once every pair of every batch has landed. The rollups leave out the
/// project `skip`; the flags are checked for every project, because a deletion does not change them.
pub(crate) fn check_aggregates(
    model: &ChModel,
    admitted: &Admitted,
    skip: Option<[u8; 16]>,
) -> (u64, Vec<String>) {
    let Some(reference) = reference(model, skip) else {
        return (0, vec!["a span_core row does not read".to_owned()]);
    };
    let mut keys = 0;
    let Some(mut wrong) = check_usage_flags(model, admitted) else {
        return (0, vec!["a span_core row does not read".to_owned()]);
    };
    for (table, expected) in reference {
        let Some(found) = stored(model, table, skip) else {
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

/// Whether a deletion selects data of `project` that arrived at `received_at` (contract 13.3): its project's,
/// received before the deletion.
fn before(deletion: &Tombstone, project: &[u8; 16], received_at: i64) -> bool {
    deletion.project_id.to_bytes() == *project && received_at < deletion.requested_at.micros()
}

/// I9's model: whether a deletion selects a span of `project` that arrived at `received_at`, by the span's trace,
/// `user_id` and `session_id`. An empty user or session names nothing.
pub(crate) fn deletion_selects_span(
    deletion: &Tombstone,
    project: &[u8; 16],
    received_at: i64,
    trace: &[u8; 16],
    user: &str,
    session: &str,
) -> bool {
    before(deletion, project, received_at)
        && match &deletion.selector {
            TombstoneSelector::Traces(ids) => ids.iter().any(|id| id.as_bytes() == trace),
            TombstoneSelector::User { user_id, .. } => !user.is_empty() && **user_id == *user,
            TombstoneSelector::Session(id) => !session.is_empty() && **id == *session,
            TombstoneSelector::Project => true,
        }
}

/// I9's model: whether a deletion by user names an MCP operation by its own identity: its `user_id`, or the
/// subject it was hashed from, under the issuer the deletion names or under any when it names none.
pub(crate) fn deletion_names_op(
    deletion: &Tombstone,
    project: &[u8; 16],
    received_at: i64,
    op: &McpSent,
) -> bool {
    before(deletion, project, received_at)
        && match &deletion.selector {
            TombstoneSelector::User { user_id, issuer } => {
                (!op.user_id.is_empty() && **user_id == *op.user_id)
                    || op.subject.as_ref().is_some_and(|(own, subject)| {
                        **user_id == **subject && issuer.as_deref().is_none_or(|named| named == own)
                    })
            }
            _ => false,
        }
}

/// I9's model: whether a deletion selects a fact of `project` that arrived at `received_at`, by its target's
/// trace or session. A fact names no user.
pub(crate) fn deletion_selects_fact(
    deletion: &Tombstone,
    project: &[u8; 16],
    received_at: i64,
    trace: &[u8; 16],
    session: &[u8],
) -> bool {
    before(deletion, project, received_at)
        && match &deletion.selector {
            TombstoneSelector::Traces(ids) => {
                *trace != [0; 16] && ids.iter().any(|id| id.as_bytes() == trace)
            }
            TombstoneSelector::User { .. } => false,
            TombstoneSelector::Session(id) => !session.is_empty() && id.as_bytes() == session,
            TombstoneSelector::Project => true,
        }
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
