//! Facts (contract 3.8 and 8.7): the effective facts of one target, the time bounds every fact of a target lies
//! in, and the decoration of trace detail and of trace list pages.
//!
//! The plans fetch candidate rows; latest-wins resolution per `(target, key)` is `crucible_records::fold_facts`,
//! the comparator the store model and the differential check use, never an SQL ordering of UUIDs.

use std::collections::BTreeMap;
use std::sync::Arc;

use crucible_plan::{
    FactTable, FactsOf, MAX_LIST, Row, TraceBounds, Value, fact_page, fact_rows, session_detail,
    trace_bounds_window,
};
use crucible_recent::RecentSpan;
use crucible_records::{
    AuthorKind, FactId, FactTargetKind, FactVersion, ScoreDataType, ScoreSource, SpanId, TableId,
    TagOp, TimeWindow, Timestamp, TraceId, Uuid, effective_tags, fold_facts,
};
use crucible_schema::{Column, SpanCoreCol};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::decode::Fields;
use crate::heads::TraceListItem;
use crate::service::{QueryService, Read, all_time};
use crate::spans::{SpanSource, TraceSpan};

/// The fact tables, for the quarantine rule.
pub(crate) const FACT_TABLES: [TableId; 4] = [
    TableId::FactScores,
    TableId::FactTags,
    TableId::FactFlags,
    TableId::FactCorrections,
];

/// The tables a list decoration reads.
pub(crate) const PAGE_FACT_TABLES: [FactTable; 3] =
    [FactTable::Scores, FactTable::Tags, FactTable::Flags];

/// What a fact is attached to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FactTarget {
    /// A trace.
    Trace(TraceId),
    /// A span of a trace.
    Span(TraceId, SpanId),
    /// A session.
    Session(Arc<str>),
}

impl FactTarget {
    /// The target kind.
    pub const fn kind(&self) -> FactTargetKind {
        match self {
            Self::Trace(_) => FactTargetKind::Trace,
            Self::Span(..) => FactTargetKind::Span,
            Self::Session(_) => FactTargetKind::Session,
        }
    }

    fn of(&self) -> FactsOf<'_> {
        match self {
            Self::Trace(trace_id) => FactsOf::Traces(core::slice::from_ref(trace_id)),
            Self::Span(trace_id, span_id) => FactsOf::Span(*trace_id, *span_id),
            Self::Session(session_id) => FactsOf::Session(session_id),
        }
    }
}

/// The row that won a key: its id, version, author and creation time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resolved {
    /// The fact's id.
    pub fact_id: FactId,
    /// The fact's version.
    pub version: u64,
    /// Who wrote it.
    pub author_kind: AuthorKind,
    /// The user or key that wrote it.
    pub author_id: Uuid,
    /// When it was written.
    pub created_at: Timestamp,
}

/// A score as it resolved. A list page carries no comment or metadata (both are empty there).
#[derive(Clone, PartialEq, Debug)]
pub struct ScoreFact {
    /// The score name.
    pub name: Arc<str>,
    /// The client's score id, when the score was written with one.
    pub score_id: Option<Arc<str>>,
    /// How to read the value.
    pub data_type: ScoreDataType,
    /// The numeric value.
    pub value_num: f64,
    /// The string value.
    pub value_str: Arc<str>,
    /// The comment.
    pub comment: Arc<str>,
    /// Where the score came from.
    pub source: ScoreSource,
    /// The metadata, JSON text.
    pub metadata: Arc<str>,
    /// The winning row.
    pub resolved: Resolved,
}

/// The latest edit of one tag.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TagEdit {
    /// The tag.
    pub tag: Arc<str>,
    /// Whether the latest edit added or removed it.
    pub op: TagOp,
    /// The winning row.
    pub resolved: Resolved,
}

/// The latest value of a flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlagFact {
    /// The value.
    pub value: bool,
    /// The winning row.
    pub resolved: Resolved,
}

/// The latest correction of a target's output.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorrectionFact {
    /// The corrected value, JSON text.
    pub corrected: Arc<str>,
    /// The note.
    pub note: Arc<str>,
    /// The winning row.
    pub resolved: Resolved,
}

/// The facts of one target after latest-wins resolution (contract 3.8): per key, the row greatest under
/// `fact_order`, and no value where that row is a retraction.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Facts {
    /// The scores, in key order.
    pub scores: Vec<ScoreFact>,
    /// The latest edit of each tag, in tag order.
    pub tags: Vec<TagEdit>,
    /// The `bookmarked` flag, when it was ever set.
    pub bookmarked: Option<FlagFact>,
    /// The `public` flag, when it was ever set.
    pub public: Option<FlagFact>,
    /// The correction of the output.
    pub correction: Option<CorrectionFact>,
}

impl Facts {
    /// The effective tags of contract 3.8: `ingest`, plus the tags whose latest edit added them, minus those
    /// whose latest edit removed them, distinct and in byte order.
    pub fn effective_tags(&self, ingest: &[Arc<str>]) -> Vec<Arc<str>> {
        let edits = self.tags.iter().map(|edit| (&*edit.tag, edit.op));
        effective_tags(ingest.iter().map(|tag| &**tag), edits)
            .into_iter()
            .map(Arc::from)
            .collect()
    }
}

/// What a trace view shows of the trace's facts.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct TraceFacts {
    /// The resolved facts of the trace.
    pub facts: Facts,
    /// The effective tags: the trace's ingest tags with the tag edits applied.
    pub tags: Vec<Arc<str>>,
}

/// The facts of one target.
#[derive(Clone, PartialEq, Debug)]
pub struct TargetFacts {
    /// The resolved facts.
    pub facts: Facts,
    /// The response metadata.
    pub meta: Meta,
}

/// Where every fact of a target lies in time: the window of the target's time bounds. The write path takes a
/// fact's `target_time` from the target's earliest known start, which only moves earlier as spans arrive, so
/// this window holds every fact written about the target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TargetBounds {
    /// The target's earliest known start: a trace's first span start, a session's first seen. `None` when
    /// the target is not known.
    pub start: Option<Timestamp>,
    /// The window of the target's facts, when the target is known.
    pub window: Option<TimeWindow>,
    /// Whether the lookup is whole and strong: only then does an unknown target not exist.
    pub meta: Meta,
}

type Decoded<T> = Result<T, QueryServiceError>;

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!(
        "the store returned a fact row without a valid {what}"
    ))
}

/// The values of a fact row, as `fact_rows` and `fact_page` order them.
struct FactRow<'a>(&'a [Value]);

impl FactRow<'_> {
    fn get(&self, at: usize, what: &str) -> Decoded<&Value> {
        self.0.get(at).ok_or_else(|| bad(what))
    }

    fn text(&self, at: usize, what: &str) -> Decoded<Arc<str>> {
        match self.get(at, what)? {
            Value::String(text) => Ok(Arc::from(text.as_str())),
            Value::Bytes(bytes) => Ok(Arc::from(String::from_utf8_lossy(bytes))),
            _ => Err(bad(what)),
        }
    }

    fn uint(&self, at: usize, what: &str) -> Decoded<u64> {
        match self.get(at, what)? {
            Value::UInt(value) => Ok(*value),
            _ => Err(bad(what)),
        }
    }

    fn enum8(&self, at: usize, what: &str) -> Decoded<u8> {
        match self.get(at, what)? {
            Value::Enum(value) => Ok(*value),
            _ => Err(bad(what)),
        }
    }

    fn uuid(&self, at: usize, what: &str) -> Decoded<Uuid> {
        match self.get(at, what)? {
            Value::Uuid(id) => Ok(*id),
            _ => Err(bad(what)),
        }
    }

    fn bytes<const N: usize>(&self, at: usize, what: &str) -> Decoded<[u8; N]> {
        match self.get(at, what)? {
            Value::Bytes(bytes) => bytes.as_slice().try_into().map_err(|_| bad(what)),
            _ => Err(bad(what)),
        }
    }

    fn trace_id(&self) -> Decoded<TraceId> {
        self.bytes::<16>(1, "trace id").map(TraceId::from_bytes)
    }

    fn key(&self) -> Decoded<Arc<str>> {
        self.text(4, "key")
    }

    fn version(&self) -> Decoded<FactVersion> {
        Ok(FactVersion {
            version: self.uint(5, "version")?,
            fact_id: FactId::from_uuid(self.uuid(6, "fact id")?),
            retracted: self.uint(7, "retracted flag")? != 0,
        })
    }

    fn resolved(&self, version: FactVersion) -> Decoded<Resolved> {
        let created_at = match self.get(10, "creation time")? {
            Value::Time(t) => *t,
            _ => return Err(bad("creation time")),
        };
        Ok(Resolved {
            fact_id: version.fact_id,
            version: version.version,
            author_kind: AuthorKind::from_value(self.enum8(8, "author kind")?)
                .ok_or_else(|| bad("author kind"))?,
            author_id: self.uuid(9, "author id")?,
            created_at,
        })
    }
}

/// One fact row read for resolution: the fact's table, its value and its place in the fold.
struct Candidate {
    table: FactTable,
    row: Row,
    key: Arc<str>,
    version: FactVersion,
}

fn candidates(table: FactTable, rows: Vec<Row>) -> Decoded<Vec<(TraceId, Candidate)>> {
    rows.into_iter()
        .map(|row| {
            let f = FactRow(row.values());
            let (trace_id, key, version) = (f.trace_id()?, f.key()?, f.version()?);
            Ok((
                trace_id,
                Candidate {
                    table,
                    row,
                    key,
                    version,
                },
            ))
        })
        .collect()
}

/// A score's key is its name, plus `#` and the client's score id when one was given.
fn score_name(key: &str) -> (Arc<str>, Option<Arc<str>>) {
    match key.split_once('#') {
        Some((name, id)) => (Arc::from(name), Some(Arc::from(id))),
        None => (Arc::from(key), None),
    }
}

/// Resolves the candidates of one target into its facts.
fn resolve(candidates: Vec<Candidate>) -> Decoded<Facts> {
    let folded = fold_facts(
        candidates
            .into_iter()
            .map(|c| ((c.table, Arc::clone(&c.key)), c.version, c)),
    );
    let mut facts = Facts::default();
    for ((table, key), c) in folded {
        let f = FactRow(c.row.values());
        let resolved = f.resolved(c.version)?;
        match table {
            FactTable::Scores => {
                let (name, score_id) = score_name(&key);
                // A list page reads no comment, metadata or config: its scores end at `source`.
                let brief = c.row.values().len() < 18;
                let text = |at, what| f.text(at, what);
                let empty = || Ok::<_, QueryServiceError>(Arc::from(""));
                facts.scores.push(ScoreFact {
                    name,
                    score_id,
                    data_type: ScoreDataType::from_value(f.enum8(11, "data type")?)
                        .ok_or_else(|| bad("data type"))?,
                    value_num: match f.get(12, "numeric value")? {
                        Value::Float(value) => *value,
                        _ => return Err(bad("numeric value")),
                    },
                    value_str: text(13, "string value")?,
                    comment: if brief {
                        empty()?
                    } else {
                        text(14, "comment")?
                    },
                    source: ScoreSource::from_value(
                        f.enum8(if brief { 14 } else { 15 }, "source")?,
                    )
                    .ok_or_else(|| bad("source"))?,
                    metadata: if brief {
                        empty()?
                    } else {
                        text(16, "metadata")?
                    },
                    resolved,
                });
            }
            FactTable::Tags => facts.tags.push(TagEdit {
                tag: key,
                op: TagOp::from_value(f.enum8(11, "tag op")?).ok_or_else(|| bad("tag op"))?,
                resolved,
            }),
            FactTable::Flags => {
                let flag = FlagFact {
                    value: f.uint(11, "flag value")? != 0,
                    resolved,
                };
                match &*key {
                    "bookmarked" => facts.bookmarked = Some(flag),
                    "public" => facts.public = Some(flag),
                    _ => {}
                }
            }
            FactTable::Corrections => {
                // A list page reads no correction body.
                if c.row.values().len() > 11 {
                    facts.correction = Some(CorrectionFact {
                        corrected: f.text(11, "corrected value")?,
                        note: f.text(12, "note")?,
                        resolved,
                    });
                }
            }
        }
    }
    Ok(facts)
}

impl Read<'_> {
    /// The resolved facts of one target, read inside `window`.
    pub(crate) async fn target_facts(
        &mut self,
        target: &FactTarget,
        window: TimeWindow,
    ) -> Result<Facts, QueryServiceError> {
        let (scope, caller) = (self.ctx.scope, self.ctx.caller);
        let mut all = Vec::new();
        for table in FactTable::ALL {
            let plan = fact_rows(scope, window, &self.clamp, caller, table, target.of());
            let rows = self.rows(plan).await?;
            all.extend(candidates(table, rows)?.into_iter().map(|(_, c)| c));
        }
        resolve(all)
    }

    /// The trace-level facts of a page of traces, read inside `window`, without the large body fields.
    pub(crate) async fn page_facts(
        &mut self,
        trace_ids: &[TraceId],
        window: TimeWindow,
    ) -> Result<BTreeMap<TraceId, Facts>, QueryServiceError> {
        let (scope, caller) = (self.ctx.scope, self.ctx.caller);
        let mut by_trace: BTreeMap<TraceId, Vec<Candidate>> = BTreeMap::new();
        for ids in trace_ids.chunks(MAX_LIST) {
            for table in PAGE_FACT_TABLES {
                let plan = fact_page(scope, window, &self.clamp, caller, table, ids);
                let rows = self.rows(plan).await?;
                for (trace_id, c) in candidates(table, rows)? {
                    by_trace.entry(trace_id).or_default().push(c);
                }
            }
        }
        by_trace
            .into_iter()
            .map(|(trace_id, candidates)| Ok((trace_id, resolve(candidates)?)))
            .collect()
    }
}

/// The window of a trace's facts from its time bounds.
pub(crate) fn trace_window(
    min_start: Timestamp,
    max_end: Timestamp,
) -> Result<TimeWindow, QueryServiceError> {
    trace_bounds_window(min_start, max_end.max(min_start)).map_err(QueryServiceError::from)
}

impl QueryService {
    /// Where every fact of `target` lies in time: for a trace or a span, the bounds of the trace from
    /// `trace_stats` and the recent index (behind the read barrier, as trace by id); for a session, its first
    /// and last seen from `session_rollup`. The write path takes a new fact's `target_time` from `start`.
    pub async fn fact_target_bounds(
        &self,
        ctx: &ReadContext,
        target: &FactTarget,
    ) -> Result<TargetBounds, QueryServiceError> {
        let mut read = self.read(ctx);
        let scope = ctx.scope;
        let (bounds, consistency, state, from_index, tables) = match target {
            FactTarget::Trace(trace_id) | FactTarget::Span(trace_id, _) => {
                let consistency = self.index.read_barrier().await;
                let (entries, state) = self.index.trace(scope, *trace_id);
                let entries = read.retained(entries)?;
                let stored = read.stored_trace(*trace_id).await?;
                let bounds = trace_extent(stored.map(|stored| stored.bounds), &entries);
                (
                    bounds,
                    consistency,
                    state,
                    !entries.is_empty(),
                    &[TableId::TraceStats],
                )
            }
            FactTarget::Session(session_id) => {
                let consistency = self.consistency();
                let state = self.index.read_state(scope);
                let plan = session_detail(scope, all_time()?, &read.clamp, ctx.caller, session_id);
                let bounds = match read.rows(plan).await?.first() {
                    // The outputs: session_id, max(last_seen), min(first_seen), ...
                    Some(row) => match (row.values().get(2), row.values().get(1)) {
                        (Some(Value::Time(first)), Some(Value::Time(last))) => {
                            Some((*first, *last))
                        }
                        _ => return Err(bad("first or last seen")),
                    },
                    None => None,
                };
                (bounds, consistency, state, false, &[TableId::SessionRollup])
            }
        };
        let meta = read.finish(read.now, consistency, state, tables, from_index)?;
        let window = bounds
            .map(|(start, end)| trace_window(start, end))
            .transpose()?;
        Ok(TargetBounds {
            start: bounds.map(|(start, _)| start),
            window,
            meta,
        })
    }

    /// The effective facts of one target (contract 3.8 and 12.6), from the fact rows whose `target_time` is in
    /// `window`: [`QueryService::fact_target_bounds`] gives the window that holds every fact of a known target.
    pub async fn facts(
        &self,
        ctx: &ReadContext,
        target: &FactTarget,
        window: TimeWindow,
    ) -> Result<TargetFacts, QueryServiceError> {
        let mut read = self.read(ctx);
        let consistency = self.consistency();
        let state = self.index.read_state(ctx.scope);
        let facts = read.target_facts(target, window).await?;
        let meta = read.finish(read.now, consistency, state, &FACT_TABLES, false)?;
        Ok(TargetFacts { facts, meta })
    }
}

/// The time bounds of a trace from its merged `trace_stats` row and its recent entries.
pub(crate) fn trace_extent(
    stored: Option<TraceBounds>,
    entries: &[RecentSpan],
) -> Option<(Timestamp, Timestamp)> {
    let mut bounds = stored.map(|b| (b.min_start, b.max_end));
    for span in entries {
        let (start, end) = (span.summary.start_time, span.summary.end_time);
        bounds = Some(match bounds {
            Some((first, last)) => (first.min(start), last.max(end)),
            None => (start, end),
        });
    }
    bounds
}

/// The ingest tags of a trace: the `trace_tags` of a root span the store returned. A root that is only in the
/// recent index has none yet, as in the trace list.
pub(crate) fn ingest_tags(spans: &[TraceSpan]) -> Vec<Arc<str>> {
    let at = SpanCoreCol::ALL
        .iter()
        .position(|c| *c == SpanCoreCol::TraceTags);
    spans
        .iter()
        .filter(|span| span.summary.is_root)
        .find_map(|span| match (&span.source, at) {
            (SpanSource::Store(row), Some(at)) => Fields(row.values()).texts(at).ok(),
            _ => None,
        })
        .unwrap_or_default()
}

impl Read<'_> {
    /// The facts of a trace for its detail view, read inside the trace's bounds.
    pub(crate) async fn trace_facts(
        &mut self,
        trace_id: TraceId,
        extent: Option<(Timestamp, Timestamp)>,
        spans: &[TraceSpan],
    ) -> Result<TraceFacts, QueryServiceError> {
        let Some((start, end)) = extent else {
            return Ok(TraceFacts::default());
        };
        let target = FactTarget::Trace(trace_id);
        let facts = self
            .target_facts(&target, trace_window(start, end)?)
            .await?;
        let tags = facts.effective_tags(&ingest_tags(spans));
        Ok(TraceFacts { facts, tags })
    }

    /// Decorates a page of the trace list with each trace's facts (contract 3.8): one read per fact table for
    /// the page's traces, inside the window that holds all of their bounds.
    pub(crate) async fn decorate(
        &mut self,
        items: &mut [TraceListItem],
    ) -> Result<(), QueryServiceError> {
        let (Some(first), Some(last)) = (
            items.iter().map(|item| item.summary.min_start).min(),
            items.iter().map(|item| item.summary.max_end).max(),
        ) else {
            return Ok(());
        };
        let mut ids: Vec<TraceId> = items.iter().map(|item| item.head.trace_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let facts = self.page_facts(&ids, trace_window(first, last)?).await?;
        for item in items {
            // A trace with several roots is listed once per root, each row with the trace's facts.
            let found = facts.get(&item.head.trace_id).cloned().unwrap_or_default();
            item.facts = TraceFacts {
                tags: found.effective_tags(&item.head.tags),
                facts: found,
            };
        }
        Ok(())
    }
}
