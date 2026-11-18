//! The spans of one trace, as trace by id and the children of a span read them (contract 8.2 and 8.8): the
//! post-operator that merges the store's spans with the recent index and de-duplicates them by `span_id`, the
//! trace's bounds from `trace_stats`, and the `child_count` of each span.

use std::collections::BTreeMap;

use crucible_plan::{Row, TraceBounds, child_counts, trace_time_bounds};
use crucible_recent::{ReadState, RecentSpan, SpanSummary, WalLocator};
use crucible_records::{SpanId, TableId, Timestamp, TraceId};

use crate::context::QueryServiceError;
use crate::decode::{self, Fields};
use crate::service::{Read, all_time};

/// The tables a read of a trace's spans depends on, for the quarantine rule.
pub(crate) const SPAN_TABLES: [TableId; 2] = [TableId::SpanCore, TableId::TraceStats];

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
    pub(crate) fn key(&self) -> (Timestamp, SpanId) {
        (self.summary.start_time, self.summary.span_id)
    }
}

/// The store's merged `trace_stats` row of a trace.
pub(crate) struct StoredTrace {
    pub(crate) bounds: TraceBounds,
    pub(crate) span_count: u64,
}

pub(crate) fn stored(rows: Vec<Row>) -> Result<Vec<TraceSpan>, QueryServiceError> {
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

pub(crate) fn recent(span: &RecentSpan) -> TraceSpan {
    TraceSpan {
        summary: span.summary.clone(),
        source: SpanSource::Recent(span.locator),
        child_count: None,
    }
}

/// The post-operator of contract 8.2: merges the store's spans with the recent entries, de-duplicates by
/// `span_id` (the store's row wins, because it is the full row) and orders by `(start_time, span_id)`.
pub(crate) fn merge(
    store: Vec<TraceSpan>,
    recent: impl IntoIterator<Item = TraceSpan>,
) -> Vec<TraceSpan> {
    let mut by_id: BTreeMap<SpanId, TraceSpan> = BTreeMap::new();
    for span in store.into_iter().chain(recent) {
        by_id.entry(span.summary.span_id).or_insert(span);
    }
    let mut spans: Vec<TraceSpan> = by_id.into_values().collect();
    spans.sort_by_key(TraceSpan::key);
    spans
}

pub(crate) fn ids(spans: &[TraceSpan]) -> Vec<SpanId> {
    spans.iter().map(|span| span.summary.span_id).collect()
}

impl Read<'_> {
    pub(crate) async fn stored_trace(
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
    pub(crate) async fn count_children(
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
