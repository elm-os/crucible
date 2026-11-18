//! Span children (contract 8.8): one page of the direct children of a span, and the `child_count` of several
//! spans of one trace.

use crucible_records::{ProjectScope, SpanId, TableId, Timestamp};
use crucible_schema::{Column, SpanCoreCol};

use crate::build::{
    RetentionClamp, TraceBounds, in_trace, span_ids, span_order, trace_bounds_window,
};
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Aggregate, Measure, Node, Scan};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// The default page of span children.
pub const SPAN_CHILDREN_DEFAULT_LIMIT: u32 = 500;
/// The largest page of span children.
pub const SPAN_CHILDREN_MAX_LIMIT: u32 = 2_000;

/// One page of the direct children of a span, in `(start_time, span_id)` order, inside the trace's time bounds
/// ([`trace_bounds_window`], clamped). `limit` defaults to [`SPAN_CHILDREN_DEFAULT_LIMIT`]; the scan reads one
/// row more than the page, which tells the caller whether another page exists.
pub fn span_children(
    scope: ProjectScope,
    trace: TraceBounds,
    clamp: &RetentionClamp,
    caller: Caller,
    parent: SpanId,
    cursor: Option<(Timestamp, SpanId)>,
    limit: Option<u32>,
) -> Result<Option<QueryPlan>, PlanError> {
    let bounds = trace_bounds_window(trace.min_start, trace.max_end)?;
    let trace_id = trace.trace_id;
    let limit = limit.unwrap_or(SPAN_CHILDREN_DEFAULT_LIMIT);
    if limit == 0 || limit > SPAN_CHILDREN_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let [hash, id] = in_trace(trace_id);
    let mut scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        Pred::And(vec![hash, id, Pred::eq(SpanCoreCol::ParentSpanId, parent)]),
    )?
    .ordered(span_order())?
    .limited(limit + 1)?;
    if let Some((start_time, span_id)) = cursor {
        scan = scan.after(vec![start_time.into(), Literal::from(span_id)])?;
    }
    clamp.plan(
        scope,
        bounds,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    )
}

/// The `child_count` of several spans of one trace: `parent_span_id` and `count()`, one row per span that has
/// at least one child.
pub fn child_counts(
    scope: ProjectScope,
    trace: TraceBounds,
    clamp: &RetentionClamp,
    caller: Caller,
    parents: &[SpanId],
) -> Result<Option<QueryPlan>, PlanError> {
    let [hash, id] = in_trace(trace.trace_id);
    let children = Pred::is_in(SpanCoreCol::ParentSpanId, span_ids(parents));
    let aggregate = Aggregate::new(
        TableId::SpanCore,
        Pred::And(vec![hash, id, children]),
        [SpanCoreCol::ParentSpanId],
        vec![Measure::Count],
    )?;
    clamp.plan(
        scope,
        trace_bounds_window(trace.min_start, trace.max_end)?,
        caller.class(QueryClass::Point),
        Node::Aggregate(aggregate),
    )
}
