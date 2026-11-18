//! Builders for the plans of the first endpoints (contract sections 8.7 and 8.8): trace by id, the trace list,
//! paged span children and data bounds.
//!
//! Every builder takes a `ProjectScope`, a `TimeWindow` (or trace bounds) and a [`RetentionClamp`]. The clamp is
//! applied here, so a plan that reads expired rows cannot be built (contract sections 8.3 and 13.1): the builders
//! are the only public way to make a [`QueryPlan`], and each returns `None` when the whole window has expired.

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
/// The most spans a paged trace detail returns: the root spans and their direct children.
pub const TRACE_PAGED_SPAN_LIMIT: u32 = 1_000;
/// The largest page of the trace list.
pub const TRACE_LIST_MAX_LIMIT: u32 = 200;
/// The trace list reads this many times the page size, because hidden provisional heads are dropped afterwards.
pub const TRACE_LIST_OVER_READ: u32 = 3;
/// The default page of span children.
pub const SPAN_CHILDREN_DEFAULT_LIMIT: u32 = 500;
/// The largest page of span children.
pub const SPAN_CHILDREN_MAX_LIMIT: u32 = 2_000;

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

/// A project's retention settings at one instant: what every builder clamps its window with.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RetentionClamp {
    settings: RetentionSettings,
    now: Timestamp,
}

impl RetentionClamp {
    /// The clamp of a project with these settings at `now` (`Clock::wall_now`).
    pub const fn new(settings: RetentionSettings, now: Timestamp) -> Self {
        Self { settings, now }
    }

    /// [`effective_window`] for a table.
    pub fn window(
        &self,
        table_id: TableId,
        window: TimeWindow,
    ) -> Result<Option<TimeWindow>, PlanError> {
        effective_window(table_id, window, &self.settings, self.now)
            .map_err(|_| PlanError::Retention)
    }

    /// The first instant of a table that is not expired, or `None` when its rows are kept forever. Rows that
    /// never reached the store (the recent-span index) are filtered with the same cut-off.
    pub fn cutoff(&self, table_id: TableId) -> Result<Option<Timestamp>, PlanError> {
        let cutoff = self
            .settings
            .cutoff(table_id, self.now)
            .map_err(|_| PlanError::Retention)?;
        Ok(cutoff.map(|day| day.start()))
    }

    /// The plan of `root` over `window` clamped for every table the node reads.
    fn plan(
        &self,
        scope: ProjectScope,
        window: TimeWindow,
        class: QueryClass,
        root: Node,
    ) -> Result<Option<QueryPlan>, PlanError> {
        let mut clamped = Some(window);
        for table_id in root.tables() {
            clamped = match clamped {
                Some(window) => self.window(table_id, window)?,
                None => None,
            };
        }
        Ok(clamped.map(|window| QueryPlan::new(scope, window, class, root)))
    }
}

/// The window that holds every span of a trace whose merged `trace_stats` row gives `min_start` and `max_end`.
/// A span starts no later than the trace ends, so the window is `[min_start, max_end]` made half-open by one
/// microsecond: a span that starts exactly at `max_end` (a zero-duration span, or a trace of one instant) is
/// inside it.
pub fn trace_bounds_window(
    min_start: Timestamp,
    max_end: Timestamp,
) -> Result<TimeWindow, PlanError> {
    let to = max_end.checked_add_micros(1).ok_or(PlanError::Bounds)?;
    TimeWindow::new(min_start, to).map_err(|_| PlanError::Bounds)
}

fn in_trace(trace_id: TraceId) -> [Pred; 2] {
    [
        Pred::eq(SpanCoreCol::TraceHash, trace_id.trace_hash()),
        Pred::eq(SpanCoreCol::TraceId, trace_id),
    ]
}

fn span_order() -> Order {
    Order::by(
        [SpanCoreCol::StartTime, SpanCoreCol::SpanId],
        Direction::Asc,
    )
}

fn span_ids(ids: &[SpanId]) -> Vec<Literal> {
    ids.iter().copied().map(Literal::from).collect()
}

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

/// The trace a read of spans belongs to, with the time bounds from its merged `trace_stats` row.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TraceBounds {
    /// The trace.
    pub trace_id: TraceId,
    /// `min(min_start)`.
    pub min_start: Timestamp,
    /// `max(max_end)`.
    pub max_end: Timestamp,
}

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

/// The two plans of the data-bounds endpoint. Each returns `min(minute)` and `max(minute)` of a rollup; a plan
/// is `None` when the rollup's whole window has expired.
#[derive(Clone, PartialEq, Debug)]
pub struct DataBoundsPlans {
    /// Over `span_metrics`: one row, or none when the window holds no data.
    pub spans: Option<QueryPlan>,
    /// Over `mcp_tool_rollup`, grouped by `side`: one row per side.
    pub mcp: Option<QueryPlan>,
}

/// Data bounds: the first and last minute with data, from the minute rollups of the registry's sources.
pub fn data_bounds(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
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
        clamp.plan(scope, window, class, Node::Aggregate(aggregate))
    };
    Ok(DataBoundsPlans {
        spans: bounds(Source::Spans, false)?,
        mcp: bounds(Source::McpOps, true)?,
    })
}
