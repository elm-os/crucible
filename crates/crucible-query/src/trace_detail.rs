//! Trace by id (contract 8.7 and 8.8): every span of a trace, or above `TRACE_INLINE_SPAN_LIMIT` the root spans
//! and their direct children.

use std::collections::BTreeSet;

use crucible_plan::{
    TRACE_INLINE_SPAN_LIMIT, TRACE_PAGED_SPAN_LIMIT, TraceBounds, spans_of_parents, trace_roots,
    trace_spans,
};
use crucible_recent::RecentSpan;
use crucible_records::{SpanId, TableId, TraceId};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::facts::{FactTarget, TraceFacts, ingest_tags, resolve_rows, trace_extent, trace_window};
use crate::service::{QueryService, Read};
use crate::spans::{SPAN_TABLES, TraceSpan, ids, merge, recent, stored};

/// The tables trace by id depends on, for the quarantine rule: its spans, its stats and its facts.
const DETAIL_TABLES: [TableId; 6] = [
    SPAN_TABLES[0],
    SPAN_TABLES[1],
    TableId::FactScores,
    TableId::FactTags,
    TableId::FactFlags,
    TableId::FactCorrections,
];

/// A trace.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceDetail {
    /// The trace.
    pub trace_id: TraceId,
    /// The number of spans. Exact when `paged` is false. On a paged response it is the store's count plus the
    /// recent entries above the checkpoint watermark, so it runs ahead by the spans that are in the store
    /// while the watermark has not passed them yet.
    pub span_count: u64,
    /// False: `spans` holds every span. True (above `TRACE_INLINE_SPAN_LIMIT`): `spans` holds the root spans
    /// and their direct children, at most `TRACE_PAGED_SPAN_LIMIT`, and each carries `child_count`.
    pub paged: bool,
    /// The spans in `(start_time, span_id)` order, without duplicates.
    pub spans: Vec<TraceSpan>,
    /// The trace's facts and its effective tags (contract 3.8).
    pub facts: TraceFacts,
    /// The response metadata.
    pub meta: Meta,
}

impl QueryService {
    /// Trace by id (contract 8.7): the recent index behind the read barrier, the time bounds from
    /// `trace_stats`, then, together, the spans from `span_core` inside them, merged and de-duplicated by
    /// `span_id`, and the trace's facts.
    ///
    /// The index is read when the barrier resolves, never after a store read has returned, so an entry cannot
    /// be evicted between the two: a span this node acknowledged is in the response, or the response says why
    /// it may not be (`partial`, or `consistency: lagging`). A trace that neither source knows is `not_found`
    /// only when the response would be whole and strong: with an open gap, a quarantined pair or an expired
    /// barrier the answer is an empty trace that carries the marker, because the trace may exist.
    pub async fn trace_by_id(
        &self,
        ctx: &ReadContext,
        trace_id: TraceId,
    ) -> Result<TraceDetail, QueryServiceError> {
        let mut read = self.read(ctx, "trace_by_id");
        let (scope, caller) = (ctx.scope, ctx.caller);
        // The index read (barrier, then snapshot) runs in parallel with the first store read. It is polled
        // first, so a node that is caught up snapshots the index before it touches the store. A node that is
        // behind snapshots at most one read barrier later, which is safe because the barrier is shorter than
        // the visibility grace: an entry cannot be applied, checkpointed and evicted inside it.
        let index_read = async {
            let waiting = self.clock.now();
            let consistency = self.index.read_barrier().await;
            let waited = self.clock.now().saturating_duration_since(waiting);
            let (entries, state) = self.index.trace(scope, trace_id);
            (consistency, entries, state, waited)
        };
        let ((consistency, entries, state, waited), trace) =
            futures::join!(index_read, read.stored_trace(trace_id));
        read.barrier_waited(waited);
        let trace = trace?;
        let entries = read.retained(entries)?;
        let unconfirmed = entries
            .iter()
            .filter(|span| span.lsn > state.watermark)
            .count() as u64;
        let stored_count = trace.as_ref().map_or(0, |trace| trace.span_count);
        let inline_limit = u64::from(TRACE_INLINE_SPAN_LIMIT);
        let bounds = trace.as_ref().map(|trace| trace.bounds);
        // The second wave, inside the bounds that the first wave and the index snapshot give: the spans and the
        // trace's facts, sent together. The store's count alone decides whether to read the whole trace: adding
        // the unconfirmed entries would count a span twice while it is in the store and its checkpoint is still
        // to come. The de-duplicated merge then gives the exact count.
        let span_plan = match bounds {
            Some(bounds) if stored_count <= inline_limit => trace_spans(
                scope,
                bounds.min_start,
                bounds.max_end,
                &read.clamp,
                caller,
                trace_id,
            ),
            _ => Ok(None),
        };
        let fact_window = trace_extent(bounds, &entries)
            .map(|(start, end)| trace_window(start, end))
            .transpose()?;
        let mut plans = vec![span_plan];
        if let Some(window) = fact_window {
            plans.extend(read.fact_plans(&FactTarget::Trace(trace_id), window));
        }
        let mut answers = read.rows_together(plans).await?.into_iter();
        let span_rows = answers.next().unwrap_or_default();
        let facts = fact_window
            .map(|_| resolve_rows(answers.collect()))
            .transpose()?;
        let mut inline = None;
        if stored_count <= inline_limit {
            let spans = merge(stored(span_rows)?, entries.iter().map(recent));
            if spans.len() as u64 <= inline_limit {
                inline = Some(spans);
            }
        }
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
        let facts = facts.map_or_else(TraceFacts::default, |facts| TraceFacts {
            tags: facts.effective_tags(&ingest_tags(&spans)),
            facts,
        });
        let meta = read.finish(
            read.now,
            consistency,
            state,
            &DETAIL_TABLES,
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
            facts,
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
