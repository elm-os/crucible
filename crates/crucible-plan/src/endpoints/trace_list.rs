//! The trace list (contract 8.7): a keyset scan of `trace_heads`, then the merged `trace_stats` rows of the
//! page's traces.

use crucible_records::{ProjectScope, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{Column, TraceHeadsCol, TraceStatsCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Aggregate, ArrivalBounds, Direction, Measure, Node, Order, Scan};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// The largest page of the trace list.
pub const TRACE_LIST_MAX_LIMIT: u32 = 200;
/// The trace list reads this many times the page size, because hidden provisional heads are dropped afterwards.
pub const TRACE_LIST_OVER_READ: u32 = 3;

/// One page of the trace list.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceListRequest {
    /// The filter over `trace_heads` columns; `Pred::all()` for none.
    pub filter: Pred,
    /// The page size, 1 to [`TRACE_LIST_MAX_LIMIT`].
    pub limit: u32,
    /// The sort tuple `(start_time, trace_hash, trace_id)` of the last row of the previous page.
    pub cursor: Option<(Timestamp, u32, TraceId)>,
    /// Fixed by the first page: only heads with `received_at <= as_of` are read, so pages do not shift.
    pub as_of: Timestamp,
    /// For a live-tail request: only heads with `received_at > since`.
    pub since: Option<Timestamp>,
}

/// The trace list: a keyset scan of `trace_heads` in key order, newest first, over-reading
/// [`TRACE_LIST_OVER_READ`] times the page size.
pub fn trace_list(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: TraceListRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    if request.limit == 0 || request.limit > TRACE_LIST_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let mut scan = Scan::new(
        TableId::TraceHeads,
        TraceHeadsCol::ALL.iter().copied(),
        request.filter,
    )?
    .ordered(Order::key(TableId::TraceHeads, Direction::Desc))?
    .limited(request.limit * TRACE_LIST_OVER_READ)?
    .arrived(ArrivalBounds {
        as_of: Some(request.as_of),
        since: request.since,
    })?;
    if let Some((start_time, hash, trace_id)) = request.cursor {
        scan = scan.after(vec![start_time.into(), hash.into(), trace_id.into()])?;
    }
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Scan(scan),
    )
}

/// The trace list, step 2: the merged `trace_stats` rows of a page of traces, one row per trace that has one.
/// The outputs are, in order: `trace_id`, `min(min_start)`, `max(max_end)`, `max(has_root)`,
/// `min(min_head_start)`, `sum(span_count)`, `sum(error_count)`, `sum(total_cost_nusd)`, `sum(input_tokens)`,
/// `sum(output_tokens)`, `sum(total_tokens)`, and the merged picks of `trace_name`, `user_id` and `session_id`.
pub fn trace_page_stats(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    trace_ids: &[TraceId],
) -> Result<Option<QueryPlan>, PlanError> {
    let ids = trace_ids.iter().copied().map(Literal::from).collect();
    let column = |c: TraceStatsCol| c.column_ref();
    let aggregate = Aggregate::new(
        TableId::TraceStats,
        Pred::is_in(TraceStatsCol::TraceId, ids),
        [TraceStatsCol::TraceId],
        vec![
            Measure::Min(column(TraceStatsCol::MinStart)),
            Measure::Max(column(TraceStatsCol::MaxEnd)),
            Measure::Max(column(TraceStatsCol::HasRoot)),
            Measure::Min(column(TraceStatsCol::MinHeadStart)),
            Measure::Sum(column(TraceStatsCol::SpanCount)),
            Measure::Sum(column(TraceStatsCol::ErrorCount)),
            Measure::Sum(column(TraceStatsCol::TotalCostNusd)),
            Measure::Sum(column(TraceStatsCol::InputTokens)),
            Measure::Sum(column(TraceStatsCol::OutputTokens)),
            Measure::Sum(column(TraceStatsCol::TotalTokens)),
            Measure::PickMerge(column(TraceStatsCol::TraceNamePick)),
            Measure::PickMerge(column(TraceStatsCol::UserIdPick)),
            Measure::PickMerge(column(TraceStatsCol::SessionIdPick)),
        ],
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}
