//! Builders for the plans of the first endpoints (contract sections 8.7 and 8.8): trace by id, the trace list,
//! paged span children and data bounds. Every builder takes a `ProjectScope` and a `TimeWindow`.

use crucible_records::{ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{
    Column, McpToolRollupCol, OutOfRange, RetentionSettings, SpanCoreCol, TraceHeadsCol,
    TraceStatsCol, clamp_from,
};

use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Aggregate, ArrivalBounds, Direction, Measure, Node, Order, Scan};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::Source;

/// At or below this many spans a trace is returned whole; above it the detail response is paged.
pub const TRACE_INLINE_SPAN_LIMIT: u32 = 5_000;
/// The largest page of the trace list.
pub const TRACE_LIST_MAX_LIMIT: u32 = 200;
/// The trace list reads this many times the page size, because hidden provisional heads are dropped afterwards.
pub const TRACE_LIST_OVER_READ: u32 = 3;
/// The default page of span children.
pub const SPAN_CHILDREN_DEFAULT_LIMIT: u32 = 500;
/// The largest page of span children.
pub const SPAN_CHILDREN_MAX_LIMIT: u32 = 2_000;

/// The `trace_hash` column value of a trace: the low 32 bits of xxh3-64 of the 16 raw id bytes (contract
/// section 6.1).
pub fn trace_hash(trace_id: TraceId) -> u32 {
    // Truncation to the low 32 bits is the definition.
    xxhash_rust::xxh3::xxh3_64(trace_id.as_bytes()) as u32
}

/// The window a table can still serve: `from` clamped to the table's retention cut-off at `now`, so expired rows
/// are hidden at once (contract section 13.1). `None` when the whole window has expired.
pub fn effective_window(
    table_id: TableId,
    window: TimeWindow,
    retention: &RetentionSettings,
    now: Timestamp,
) -> Result<Option<TimeWindow>, OutOfRange> {
    Ok(match retention.cutoff(table_id, now)? {
        Some(cutoff) => TimeWindow::new(clamp_from(window.from(), cutoff), window.to()).ok(),
        None => Some(window),
    })
}

fn in_trace(trace_id: TraceId) -> [Pred; 2] {
    [
        Pred::eq(SpanCoreCol::TraceHash, trace_hash(trace_id)),
        Pred::eq(SpanCoreCol::TraceId, trace_id),
    ]
}

/// Trace by id, step 1: the trace's merged `trace_stats` row, which gives the time bounds for the span read and
/// the `span_count`. The outputs are, in order: `min(min_start)`, `max(max_end)`, `sum(span_count)`,
/// `max(has_root)`, `min(min_head_start)`.
pub fn trace_time_bounds(
    scope: ProjectScope,
    window: TimeWindow,
    caller: Caller,
    trace_id: TraceId,
) -> Result<QueryPlan, PlanError> {
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
    Ok(QueryPlan::new(
        scope,
        window,
        caller.class(QueryClass::Point),
        Node::Aggregate(aggregate),
    ))
}

/// Trace by id, step 2: the trace's spans from `span_core`, read by key inside the time bounds that step 1
/// returned (`bounds`), in `(start_time, span_id)` order, at most [`TRACE_INLINE_SPAN_LIMIT`] rows.
pub fn trace_spans(
    scope: ProjectScope,
    bounds: TimeWindow,
    caller: Caller,
    trace_id: TraceId,
) -> Result<QueryPlan, PlanError> {
    let scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        Pred::And(in_trace(trace_id).into()),
    )?
    .ordered(Order::by(
        [SpanCoreCol::StartTime, SpanCoreCol::SpanId],
        Direction::Asc,
    ))?
    .limited(TRACE_INLINE_SPAN_LIMIT)?;
    Ok(QueryPlan::new(
        scope,
        bounds,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    ))
}

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
    caller: Caller,
    request: TraceListRequest,
) -> Result<QueryPlan, PlanError> {
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
    Ok(QueryPlan::new(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Scan(scan),
    ))
}

/// One page of the direct children of a span, in `(start_time, span_id)` order, inside the trace's time bounds.
/// `limit` defaults to [`SPAN_CHILDREN_DEFAULT_LIMIT`]; the scan reads one row more than the page, which tells
/// the caller whether another page exists.
pub fn span_children(
    scope: ProjectScope,
    bounds: TimeWindow,
    caller: Caller,
    trace_id: TraceId,
    parent: SpanId,
    cursor: Option<(Timestamp, SpanId)>,
    limit: Option<u32>,
) -> Result<QueryPlan, PlanError> {
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
    .ordered(Order::by(
        [SpanCoreCol::StartTime, SpanCoreCol::SpanId],
        Direction::Asc,
    ))?
    .limited(limit + 1)?;
    if let Some((start_time, span_id)) = cursor {
        scan = scan.after(vec![start_time.into(), Literal::from(span_id)])?;
    }
    Ok(QueryPlan::new(
        scope,
        bounds,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    ))
}

/// The two plans of the data-bounds endpoint. Each returns `min(minute)` and `max(minute)` of a rollup.
#[derive(Clone, PartialEq, Debug)]
pub struct DataBoundsPlans {
    /// Over `span_metrics`: one row.
    pub spans: QueryPlan,
    /// Over `mcp_tool_rollup`, grouped by `side`: one row per side.
    pub mcp: QueryPlan,
}

/// Data bounds: the first and last minute with data, from the minute rollups of the registry's sources.
pub fn data_bounds(
    scope: ProjectScope,
    window: TimeWindow,
    caller: Caller,
) -> Result<DataBoundsPlans, PlanError> {
    let class = caller.class(QueryClass::Analytics);
    let bounds = |source: Source, by_side: bool| {
        let rollup = source.rollup().ok_or(PlanError::Empty)?;
        let group = by_side.then(|| McpToolRollupCol::Side.column_ref());
        let aggregate = Aggregate::new(
            rollup.table,
            Pred::all(),
            group,
            vec![Measure::Min(rollup.minute), Measure::Max(rollup.minute)],
        )?;
        Ok(QueryPlan::new(
            scope,
            window,
            class,
            Node::Aggregate(aggregate),
        ))
    };
    Ok(DataBoundsPlans {
        spans: bounds(Source::Spans, false)?,
        mcp: bounds(Source::McpOps, true)?,
    })
}
