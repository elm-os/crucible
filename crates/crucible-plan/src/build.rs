//! What the plan builders of every endpoint share (contract sections 8.3, 8.7, 8.8 and 13.1): the retention
//! clamp, the window of a trace's spans, and the predicate, order and id list of a read of a trace's spans. The
//! builders themselves are in `endpoints`, one module per endpoint.
//!
//! Every builder takes a `ProjectScope`, a `TimeWindow` (or trace bounds) and a [`RetentionClamp`]. The clamp is
//! applied in the builders, so a plan that reads expired rows cannot be built (contract sections 8.3 and 13.1):
//! the builders are the only public way to make a [`QueryPlan`], and each returns `None` when the whole window
//! has expired.

use crucible_records::{ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{OutOfRange, RetentionSettings, SpanCoreCol, clamp_from};

use crate::class::QueryClass;
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Direction, Node, Order};
use crate::plan::QueryPlan;
use crate::pred::Pred;

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
    pub(crate) fn plan(
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

pub(crate) fn in_trace(trace_id: TraceId) -> [Pred; 2] {
    [
        Pred::eq(SpanCoreCol::TraceHash, trace_id.trace_hash()),
        Pred::eq(SpanCoreCol::TraceId, trace_id),
    ]
}

pub(crate) fn span_order() -> Order {
    Order::by(
        [SpanCoreCol::StartTime, SpanCoreCol::SpanId],
        Direction::Asc,
    )
}

pub(crate) fn span_ids(ids: &[SpanId]) -> Vec<Literal> {
    ids.iter().copied().map(Literal::from).collect()
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
