//! Trace spans (contract 8.7 and 8.8): the spans of one trace from `span_core`, every span up to
//! [`TRACE_INLINE_SPAN_LIMIT`], else the root spans and their direct children.

use crucible_records::{ProjectScope, SpanId, TableId, Timestamp, TraceId};
use crucible_schema::{Column, SpanCoreCol};

use crate::build::{
    RetentionClamp, TraceBounds, in_trace, span_ids, span_order, trace_bounds_window,
};
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::node::{Node, Scan};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// At or below this many spans a trace is returned whole; above it the detail response is paged.
pub const TRACE_INLINE_SPAN_LIMIT: u32 = 5_000;
/// The most spans a paged trace detail returns: the root spans and their direct children.
pub const TRACE_PAGED_SPAN_LIMIT: u32 = 1_000;

/// Trace by id, step 2: the trace's spans from `span_core`, read by key inside the time bounds that step 1
/// returned (`min_start`, `max_end`), in `(start_time, span_id)` order. The scan reads one row more than
/// [`TRACE_INLINE_SPAN_LIMIT`], which tells the caller that the trace is too large to return whole. The plan's
/// window is [`trace_bounds_window`], clamped.
pub fn trace_spans(
    scope: ProjectScope,
    min_start: Timestamp,
    max_end: Timestamp,
    clamp: &RetentionClamp,
    caller: Caller,
    trace_id: TraceId,
) -> Result<Option<QueryPlan>, PlanError> {
    let bounds = trace_bounds_window(min_start, max_end)?;
    let scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        Pred::And(in_trace(trace_id).into()),
    )?
    .ordered(span_order())?
    .limited(TRACE_INLINE_SPAN_LIMIT + 1)?;
    clamp.plan(
        scope,
        bounds,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    )
}

/// Paged trace detail, step 1: the root spans of a trace in `(start_time, span_id)` order, at most
/// [`TRACE_PAGED_SPAN_LIMIT`].
pub fn trace_roots(
    scope: ProjectScope,
    trace: TraceBounds,
    clamp: &RetentionClamp,
    caller: Caller,
) -> Result<Option<QueryPlan>, PlanError> {
    let [hash, id] = in_trace(trace.trace_id);
    let scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        Pred::And(vec![hash, id, Pred::eq(SpanCoreCol::IsRoot, 1u32)]),
    )?
    .ordered(span_order())?
    .limited(TRACE_PAGED_SPAN_LIMIT)?;
    clamp.plan(
        scope,
        trace_bounds_window(trace.min_start, trace.max_end)?,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    )
}

/// Paged trace detail, step 2: the direct children of several spans of one trace in `(start_time, span_id)`
/// order, at most `limit` (1 to [`TRACE_PAGED_SPAN_LIMIT`]).
pub fn spans_of_parents(
    scope: ProjectScope,
    trace: TraceBounds,
    clamp: &RetentionClamp,
    caller: Caller,
    parents: &[SpanId],
    limit: u32,
) -> Result<Option<QueryPlan>, PlanError> {
    if limit > TRACE_PAGED_SPAN_LIMIT {
        return Err(PlanError::Limit);
    }
    let [hash, id] = in_trace(trace.trace_id);
    let children = Pred::is_in(SpanCoreCol::ParentSpanId, span_ids(parents));
    let scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        Pred::And(vec![hash, id, children]),
    )?
    .ordered(span_order())?
    .limited(limit)?;
    clamp.plan(
        scope,
        trace_bounds_window(trace.min_start, trace.max_end)?,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    )
}
