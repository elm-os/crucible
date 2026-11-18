//! Trace by id and the paged children of a span (contract 8.7 and 8.8).

use std::collections::{BTreeMap, BTreeSet};

use crucible_plan::{
    Row, SPAN_CHILDREN_DEFAULT_LIMIT, SPAN_CHILDREN_MAX_LIMIT, TRACE_INLINE_SPAN_LIMIT,
    TRACE_PAGED_SPAN_LIMIT, TraceBounds, child_counts, span_children, spans_of_parents,
    trace_roots, trace_spans, trace_time_bounds,
};
use crucible_recent::{ReadState, RecentSpan, SpanSummary, WalLocator};
use crucible_records::{SpanId, TableId, Timestamp, TraceId};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::ChildCursor;
use crate::decode::{self, Fields};
use crate::service::{QueryService, Read, all_time};

/// The tables a read of a trace's spans depends on, for the quarantine rule.
const SPAN_TABLES: [TableId; 2] = [TableId::SpanCore, TableId::TraceStats];

/// Where a returned span came from.
#[derive(Clone, PartialEq, Debug)]
pub enum SpanSource {
    /// The store: the full `span_core` row, in `SpanCoreCol::ALL` order.
    Store(Row),
    /// The recent index, which holds no previews: the full record is re-read from the WAL at the locator
    /// (`RecentIndex::span_record`).
    Recent(WalLocator),
}

/// One span of a trace.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceSpan {
    /// The list-and-tree view of the span.
    pub summary: SpanSummary,
    /// Where it came from.
    pub source: SpanSource,
    /// The number of direct children, on paged responses.
    pub child_count: Option<u64>,
}

impl TraceSpan {
    fn key(&self) -> (Timestamp, SpanId) {
        (self.summary.start_time, self.summary.span_id)
    }
}

/// A trace.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceDetail {
    /// The trace.
    pub trace_id: TraceId,
    /// The number of spans. Exact when `paged` is false; on a paged response it is the store's count plus the
    /// recent entries above the checkpoint watermark.
    pub span_count: u64,
    /// False: `spans` holds every span. True (above `TRACE_INLINE_SPAN_LIMIT`): `spans` holds the root spans
    /// and their direct children, at most `TRACE_PAGED_SPAN_LIMIT`, and each carries `child_count`.
    pub paged: bool,
    /// The spans in `(start_time, span_id)` order, without duplicates.
    pub spans: Vec<TraceSpan>,
    /// The response metadata.
    pub meta: Meta,
}

/// A request for one page of the direct children of a span.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanChildrenQuery {
    /// The trace.
    pub trace_id: TraceId,
    /// The parent span.
    pub parent: SpanId,
    /// The cursor of the previous page.
    pub cursor: Option<ChildCursor>,
    /// The page size; default 500, at most 2,000.
    pub limit: Option<u32>,
}

/// One page of the direct children of a span, in `(start_time, span_id)` order. Each carries `child_count`.
#[derive(Clone, PartialEq, Debug)]
pub struct SpanChildrenPage {
    /// The children.
    pub data: Vec<TraceSpan>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<ChildCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

/// The store's merged `trace_stats` row of a trace.
struct StoredTrace {
    bounds: TraceBounds,
    span_count: u64,
}

fn stored(rows: Vec<Row>) -> Result<Vec<TraceSpan>, QueryServiceError> {
    rows.into_iter()
        .map(|row| {
            let summary = decode::span(&row)?;
            Ok(TraceSpan {
                summary,
                source: SpanSource::Store(row),
                child_count: None,
            })
        })
        .collect()
}

fn recent(span: &RecentSpan) -> TraceSpan {
    TraceSpan {
        summary: span.summary.clone(),
        source: SpanSource::Recent(span.locator),
        child_count: None,
    }
}

/// The post-operator of contract 8.2: merges the store's spans with the recent entries, de-duplicates by
/// `span_id` (the store's row wins, because it is the full row) and orders by `(start_time, span_id)`.
fn merge(store: Vec<TraceSpan>, recent: impl IntoIterator<Item = TraceSpan>) -> Vec<TraceSpan> {
    let mut by_id: BTreeMap<SpanId, TraceSpan> = BTreeMap::new();
    for span in store.into_iter().chain(recent) {
        by_id.entry(span.summary.span_id).or_insert(span);
    }
    let mut spans: Vec<TraceSpan> = by_id.into_values().collect();
    spans.sort_by_key(TraceSpan::key);
    spans
}

fn ids(spans: &[TraceSpan]) -> Vec<SpanId> {
    spans.iter().map(|span| span.summary.span_id).collect()
}

impl Read<'_> {
    async fn stored_trace(
        &mut self,
        trace_id: TraceId,
    ) -> Result<Option<StoredTrace>, QueryServiceError> {
        let (scope, caller) = (self.ctx.scope, self.ctx.caller);
        let plan = trace_time_bounds(scope, all_time()?, &self.clamp, caller, trace_id);
        let Some(row) = self.rows(plan).await?.into_iter().next() else {
            return Ok(None);
        };
        // The outputs: trace_id, min(min_start), max(max_end), sum(span_count), max(has_root), ...
        let f = Fields(row.values());
        Ok(Some(StoredTrace {
            bounds: TraceBounds {
                trace_id,
                min_start: f.time(1)?,
                max_end: f.time(2)?,
            },
            span_count: f.uint(3)?,
        }))
    }

    /// Sets `child_count` on each span: the store's count of its children plus the recent children above the
    /// watermark, which the store's count cannot hold yet.
    async fn count_children(
        &mut self,
        trace: Option<TraceBounds>,
        spans: &mut [TraceSpan],
        recent: &[RecentSpan],
        state: ReadState,
    ) -> Result<(), QueryServiceError> {
        let mut counts: BTreeMap<SpanId, u64> = BTreeMap::new();
        if let (Some(trace), false) = (trace, spans.is_empty()) {
            let (scope, caller) = (self.ctx.scope, self.ctx.caller);
            let plan = child_counts(scope, trace, &self.clamp, caller, &ids(spans));
            for row in self.rows(plan).await? {
                let f = Fields(row.values());
                counts.insert(f.span_id(0)?, f.uint(1)?);
            }
        }
        for child in recent.iter().filter(|span| span.lsn > state.watermark) {
            *counts.entry(child.summary.parent_span_id).or_default() += 1;
        }
        for span in spans {
            span.child_count = Some(counts.get(&span.summary.span_id).copied().unwrap_or(0));
        }
        Ok(())
    }
}

impl QueryService {
    /// Trace by id (contract 8.7): the recent index behind the read barrier, the time bounds from
    /// `trace_stats`, then the spans from `span_core` inside them, merged and de-duplicated by `span_id`.
    ///
    /// The index is read when the barrier resolves and before any store read, so an entry cannot be evicted
    /// between the two: a span this node acknowledged is in the response, or the response says why it may not
    /// be (`partial`, or `consistency: lagging`). A trace that neither source knows is `not_found` only when
    /// the response would be whole and strong: with an open gap, a quarantined pair or an expired barrier the
    /// answer is an empty trace that carries the marker, because the trace may exist.
    pub async fn trace_by_id(
        &self,
        ctx: &ReadContext,
        trace_id: TraceId,
    ) -> Result<TraceDetail, QueryServiceError> {
        let mut read = self.read(ctx);
        let (scope, caller) = (ctx.scope, ctx.caller);
        let consistency = self.index.read_barrier().await;
        let (entries, state) = self.index.trace(scope, trace_id);
        let entries = read.retained(entries)?;
        let unconfirmed = entries
            .iter()
            .filter(|span| span.lsn > state.watermark)
            .count() as u64;

        let trace = read.stored_trace(trace_id).await?;
        let stored_count = trace.as_ref().map_or(0, |trace| trace.span_count);
        let inline_limit = u64::from(TRACE_INLINE_SPAN_LIMIT);
        let mut inline = None;
        if stored_count.saturating_add(unconfirmed) <= inline_limit {
            let rows = match &trace {
                Some(StoredTrace { bounds, .. }) => {
                    let (from, to) = (bounds.min_start, bounds.max_end);
                    let plan = trace_spans(scope, from, to, &read.clamp, caller, trace_id);
                    read.rows(plan).await?
                }
                None => Vec::new(),
            };
            let spans = merge(stored(rows)?, entries.iter().map(recent));
            if spans.len() as u64 <= inline_limit {
                inline = Some(spans);
            }
        }
        let bounds = trace.as_ref().map(|trace| trace.bounds);
        let (spans, span_count, paged) = match inline {
            Some(spans) => {
                let count = spans.len() as u64;
                (spans, count, false)
            }
            None => {
                let mut spans = read.outline(bounds, &entries).await?;
                read.count_children(bounds, &mut spans, &entries, state)
                    .await?;
                (spans, stored_count.saturating_add(unconfirmed), true)
            }
        };
        let meta = read.finish(
            read.now,
            consistency,
            state,
            &SPAN_TABLES,
            !entries.is_empty(),
        )?;
        if spans.is_empty() && meta.is_whole() {
            return Err(QueryServiceError::NotFound);
        }
        Ok(TraceDetail {
            trace_id,
            span_count,
            paged,
            spans,
            meta,
        })
    }

    /// One page of the direct children of a span (contract 8.8), merged with the recent index and
    /// de-duplicated by `span_id`.
    pub async fn span_children(
        &self,
        ctx: &ReadContext,
        query: SpanChildrenQuery,
    ) -> Result<SpanChildrenPage, QueryServiceError> {
        let mut read = self.read(ctx);
        let (scope, caller) = (ctx.scope, ctx.caller);
        let SpanChildrenQuery {
            trace_id, parent, ..
        } = query;
        let limit = query.limit.unwrap_or(SPAN_CHILDREN_DEFAULT_LIMIT);
        if limit == 0 || limit > SPAN_CHILDREN_MAX_LIMIT {
            return Err(QueryServiceError::Validation("limit"));
        }
        let after = match query.cursor {
            Some(cursor) => Some(cursor.check(scope, trace_id, parent)?),
            None => None,
        };
        let consistency = self.consistency();
        let (entries, state) = self.index.trace(scope, trace_id);
        let entries = read.retained(entries)?;
        let known = !entries.is_empty();
        let children = entries
            .iter()
            .filter(|span| span.summary.parent_span_id == parent)
            .map(recent);

        let bounds = read.stored_trace(trace_id).await?.map(|trace| trace.bounds);
        let rows = match bounds {
            Some(trace) => {
                let plan = span_children(
                    scope,
                    trace,
                    &read.clamp,
                    caller,
                    parent,
                    after,
                    Some(limit),
                );
                read.rows(plan).await?
            }
            None => Vec::new(),
        };
        // The store's rows are its first `limit + 1` after the cursor, so the first `limit` of the merge are
        // the first `limit` of the whole.
        let mut data = merge(stored(rows)?, children);
        data.retain(|span| after.is_none_or(|after| span.key() > after));
        let has_more = data.len() > limit as usize;
        data.truncate(limit as usize);
        read.count_children(bounds, &mut data, &entries, state)
            .await?;
        let meta = read.finish(read.now, consistency, state, &SPAN_TABLES, known)?;
        if bounds.is_none() && !known && meta.is_whole() {
            return Err(QueryServiceError::NotFound);
        }
        let next_cursor = match data.last() {
            Some(last) if has_more => Some(ChildCursor::from_parts(
                scope.project_id(),
                trace_id,
                parent,
                last.key(),
            )),
            _ => None,
        };
        Ok(SpanChildrenPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }
}

impl Read<'_> {
    /// The paged form of a trace: the root spans and their direct children, at most
    /// [`TRACE_PAGED_SPAN_LIMIT`], in `(start_time, span_id)` order.
    async fn outline(
        &mut self,
        trace: Option<TraceBounds>,
        entries: &[RecentSpan],
    ) -> Result<Vec<TraceSpan>, QueryServiceError> {
        let (scope, caller) = (self.ctx.scope, self.ctx.caller);
        let limit = TRACE_PAGED_SPAN_LIMIT as usize;
        let rows = match trace {
            Some(trace) => {
                self.rows(trace_roots(scope, trace, &self.clamp, caller))
                    .await?
            }
            None => Vec::new(),
        };
        let recent_roots = entries
            .iter()
            .filter(|span| span.summary.is_root)
            .map(recent);
        let mut roots = merge(stored(rows)?, recent_roots);
        roots.truncate(limit);
        let room = limit - roots.len();
        if room == 0 || roots.is_empty() {
            return Ok(roots);
        }
        let root_ids: BTreeSet<SpanId> = ids(&roots).into_iter().collect();
        let rows = match trace {
            Some(trace) => {
                let parents: Vec<SpanId> = root_ids.iter().copied().collect();
                let plan =
                    spans_of_parents(scope, trace, &self.clamp, caller, &parents, room as u32);
                self.rows(plan).await?
            }
            None => Vec::new(),
        };
        let recent_children = entries
            .iter()
            .filter(|span| root_ids.contains(&span.summary.parent_span_id))
            .map(recent);
        let mut children = merge(stored(rows)?, recent_children);
        children.retain(|span| !root_ids.contains(&span.summary.span_id));
        children.truncate(room);
        Ok(merge(roots, children))
    }
}
