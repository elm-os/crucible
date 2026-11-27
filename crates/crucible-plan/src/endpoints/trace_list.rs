//! The trace list (contract 8.7): a keyset scan of `trace_heads`, then the merged `trace_stats` rows of the
//! page's traces. Under a list filter, the conditions on fields that heads carry filter the scan, and the
//! conditions on fields that only spans carry restrict it to the traces that have a matching span.

use crucible_records::{ProjectScope, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{Column, SpanCoreCol, SpanPayloadCol, TraceHeadsCol, TraceStatsCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::filter::{Filter, FilterTarget, TRACE_LIST_MAX_VALUES};
use crate::literal::Literal;
use crate::node::{
    Aggregate, ArrivalBounds, DEFAULT_MAX_IDS, Direction, Measure, Node, Order, Scan, SemiJoin,
};
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
    let scan = heads(request)?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Scan(scan),
    )
}

/// The keyset scan of [`trace_list`].
fn heads(request: TraceListRequest) -> Result<Scan, PlanError> {
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
    Ok(scan)
}

/// The trace list under a list filter (contract 8.8): [`trace_list`] with `filter` added to the request's own
/// predicate (which a live-tail read uses for the heads its cursor had not reflected).
///
/// `filter` is split at its top-level conjunction. The conjuncts that `trace_heads` can evaluate filter the
/// heads, so they test the head span's values (contract 3.3). The others test fields that only spans carry
/// (`observation_type`, `metadata`): they restrict the list to the traces with at least one span of the window
/// that satisfies all of them, through a semi-join into the trace ids of `span_core`, or of `span_payload` for
/// metadata conditions. One table must evaluate all of them, so metadata cannot be combined with
/// `observation_type` (or with any field but `user_id` and `session_id` under `or` or `not`); such a filter is
/// refused. The semi-join is refused as too broad when more than [`DEFAULT_MAX_IDS`] traces of the window match
/// the span conditions, and a filter of more than [`TRACE_LIST_MAX_VALUES`] values is refused. The outputs are
/// those of [`trace_list`].
pub fn filtered_trace_list(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: TraceListRequest,
    filter: &Filter,
) -> Result<Option<QueryPlan>, PlanError> {
    if filter.values() > TRACE_LIST_MAX_VALUES {
        return Err(PlanError::ListLength("filter"));
    }
    let (on_heads, on_spans) = filter.split(FilterTarget::TraceHeads);
    let pred = match (
        on_heads.pred(FilterTarget::TraceHeads)?,
        request.filter.clone(),
    ) {
        (heads, extra) if extra == Pred::all() => heads,
        (heads, extra) if heads == Pred::all() => extra,
        (heads, extra) => Pred::And(vec![heads, extra]),
    };
    let scan = heads(TraceListRequest {
        filter: pred,
        ..request
    })?;
    if on_spans.is_all() {
        return clamp.plan(
            scope,
            window,
            caller.class(QueryClass::List),
            Node::Scan(scan),
        );
    }
    let (table, target, trace_id) = if on_spans.unsupported(FilterTarget::SpanCore).is_none() {
        (
            TableId::SpanCore,
            FilterTarget::SpanCore,
            SpanCoreCol::TraceId.column_ref(),
        )
    } else if let Some(field) = on_spans.unsupported(FilterTarget::SpanPayload) {
        return Err(PlanError::UnsupportedFilter {
            field: field.as_str(),
            target: "a trace list filter with a metadata condition",
        });
    } else {
        (
            TableId::SpanPayload,
            FilterTarget::SpanPayload,
            SpanPayloadCol::TraceId.column_ref(),
        )
    };
    let ids = Aggregate::new(
        table,
        on_spans.pred(target)?,
        [trace_id],
        vec![Measure::Count],
    )?;
    let root = SemiJoin::new(
        scan,
        TraceHeadsCol::TraceId,
        Node::Aggregate(ids),
        DEFAULT_MAX_IDS,
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::SemiJoin(root),
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
