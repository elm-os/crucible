//! Trace bounds (contract 8.7): the merged `trace_stats` row of one trace. Trace by id and the children of a span
//! both start with it.

use crucible_records::{ProjectScope, TableId, TimeWindow, TraceId};
use crucible_schema::{Column, TraceStatsCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::node::{Aggregate, Measure, Node};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// Trace by id, step 1: the trace's merged `trace_stats` row, which gives the time bounds for the span read and
/// the `span_count`. The outputs are, in order: `min(min_start)`, `max(max_end)`, `sum(span_count)`,
/// `max(has_root)`, `min(min_head_start)`.
pub fn trace_time_bounds(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    trace_id: TraceId,
) -> Result<Option<QueryPlan>, PlanError> {
    let aggregate = Aggregate::new(
        TableId::TraceStats,
        Pred::eq(TraceStatsCol::TraceId, trace_id),
        [TraceStatsCol::TraceId],
        vec![
            Measure::Min(TraceStatsCol::MinStart.column_ref()),
            Measure::Max(TraceStatsCol::MaxEnd.column_ref()),
            Measure::Sum(TraceStatsCol::SpanCount.column_ref()),
            Measure::Max(TraceStatsCol::HasRoot.column_ref()),
            Measure::Min(TraceStatsCol::MinHeadStart.column_ref()),
        ],
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Point),
        Node::Aggregate(aggregate),
    )
}
