//! The reads of the retention and deletion executors (contract 13.2 and 13.3), and the columns their deletes
//! name.
//!
//! These are the only plans that are not clamped to a project's retention: the executors read exactly the rows
//! that reads hide, to find the partitions that hold them and to verify that they are gone. They take no
//! `RetentionClamp` and no caller, and run in the `analytics_raw` class.

use crucible_records::{ProjectScope, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{
    Column, ColumnRef, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol,
    McpDeploysCol, McpOpsCol, McpToolInventoryCol, McpToolRollupCol, SessionRollupCol, SpanCoreCol,
    SpanMetricsCol, SpanPayloadCol, TraceHeadsCol, TraceStatsCol, Ty, UserRollupCol, table,
};

use crate::class::QueryClass;
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Measure, Node};
use crate::plan::QueryPlan;
use crate::pred::{CmpOp, Pred};

/// The most traces one page of [`deletion_traces`] returns.
pub const DELETION_TRACES_PAGE: u32 = 500;

/// The most trace ids one `IN` list of a lifecycle predicate holds. Every id is a parameter of its statement
/// (the renderer pads a list to the next power of two), and a ClickHouse server refuses a request with more
/// than 1,000 parameters, or with a statement text above 128 KiB.
pub const TRACE_ID_LIST_CHUNK: usize = 500;

/// `column IN (ids)` for a list of any length: one `IN` list per
/// [`TRACE_ID_LIST_CHUNK`] ids, joined with `OR`.
pub fn trace_id_in(column: ColumnRef, ids: &[TraceId]) -> Pred {
    let mut lists: Vec<Pred> = ids
        .chunks(TRACE_ID_LIST_CHUNK)
        .map(|chunk| Pred::In(column, chunk.iter().copied().map(Literal::from).collect()))
        .collect();
    if lists.len() == 1 {
        lists.remove(0)
    } else {
        Pred::Or(lists)
    }
}

fn named<C: Column>(name: &str) -> Option<ColumnRef> {
    C::ALL
        .iter()
        .find(|column| column.name() == name)
        .map(|column| column.column_ref())
}

/// The column `name` of a table, for the predicate of a lifecycle delete. `None` for a column the table does
/// not have, for `project_id`, and for the two service tables.
pub fn lifecycle_column(table_id: TableId, name: &str) -> Option<ColumnRef> {
    match table_id {
        TableId::SpanCore => named::<SpanCoreCol>(name),
        TableId::SpanPayload => named::<SpanPayloadCol>(name),
        TableId::McpOps => named::<McpOpsCol>(name),
        TableId::McpContent => named::<McpContentCol>(name),
        TableId::McpToolInventory => named::<McpToolInventoryCol>(name),
        TableId::FactScores => named::<FactScoresCol>(name),
        TableId::FactTags => named::<FactTagsCol>(name),
        TableId::FactFlags => named::<FactFlagsCol>(name),
        TableId::FactCorrections => named::<FactCorrectionsCol>(name),
        TableId::TraceHeads => named::<TraceHeadsCol>(name),
        TableId::TraceStats => named::<TraceStatsCol>(name),
        TableId::SessionRollup => named::<SessionRollupCol>(name),
        TableId::UserRollup => named::<UserRollupCol>(name),
        TableId::SpanMetrics => named::<SpanMetricsCol>(name),
        TableId::McpToolRollup => named::<McpToolRollupCol>(name),
        TableId::McpDeploys => named::<McpDeploysCol>(name),
        TableId::DedupSelftest | TableId::SqlProbe => None,
    }
}

/// The column a table's retention is evaluated on, when a predicate can name it: every time column except the
/// aggregated `last_seen` of the two unpartitioned tables.
pub fn lifecycle_time_column(table_id: TableId) -> Option<ColumnRef> {
    lifecycle_column(table_id, table(table_id).retention.time_column)
        .filter(|column| column.ty().agg_fn().is_none())
}

/// `<time column> < cutoff`: the rows of a table that a retention cut-off has expired, for the delete of a
/// partition that straddles the cut-off (contract 13.2). `cutoff` is the start of a UTC day.
pub fn expired_before(table_id: TableId, cutoff: Timestamp) -> Result<Pred, PlanError> {
    let column = lifecycle_time_column(table_id).ok_or(PlanError::Table(table_id))?;
    let value = match column.ty() {
        Ty::Date => Literal::Date(cutoff.date()),
        _ => Literal::Time(cutoff),
    };
    Ok(Pred::Cmp(column, CmpOp::Lt, value))
}

/// The oldest row of a table that matches `pred` inside `window`, expired or not: one row holding the minimum
/// of the table's time column, or no row when nothing matches. The retention executor finds its partitions
/// with it, and both executors verify with it that a delete left nothing behind.
pub fn lifecycle_oldest(
    scope: ProjectScope,
    table_id: TableId,
    window: TimeWindow,
    pred: Pred,
) -> Result<QueryPlan, PlanError> {
    let column = lifecycle_time_column(table_id).ok_or(PlanError::Table(table_id))?;
    let aggregate = Aggregate::new(
        table_id,
        pred,
        None::<ColumnRef>,
        vec![Measure::Min(column)],
    )?;
    Ok(QueryPlan::new(
        scope,
        window,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    ))
}

/// Whose traces a deletion request resolves (contract 13.3, selector resolution).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceSelector<'a> {
    /// These traces.
    Traces(&'a [TraceId]),
    /// The traces with a span of this `user_id`.
    User(&'a str),
    /// The traces with a span of this `session_id`.
    Session(&'a str),
}

/// One page of the traces a deletion selects, with the span of time their spans start in: the outputs are
/// `trace_id`, `min(start_time)` and `max(start_time)`, ordered by `trace_id`, at most `limit` rows after the
/// trace `after`. Only spans received before `requested_at` count, because a deletion keeps later data.
pub fn deletion_traces(
    scope: ProjectScope,
    selector: TraceSelector<'_>,
    requested_at: Timestamp,
    after: Option<TraceId>,
    limit: u32,
) -> Result<QueryPlan, PlanError> {
    let selected = match selector {
        TraceSelector::Traces(ids) => trace_id_in(SpanCoreCol::TraceId.column_ref(), ids),
        TraceSelector::User(user_id) => Pred::eq(SpanCoreCol::UserId, user_id),
        TraceSelector::Session(session_id) => Pred::eq(SpanCoreCol::SessionId, session_id),
    };
    let pred = Pred::And(vec![
        selected,
        Pred::cmp(SpanCoreCol::ReceivedAt, CmpOp::Lt, requested_at),
    ]);
    let start = SpanCoreCol::StartTime.column_ref();
    let mut aggregate = Aggregate::new(
        TableId::SpanCore,
        pred,
        [SpanCoreCol::TraceId],
        vec![Measure::Min(start), Measure::Max(start)],
    )?
    .ordered(vec![(AggOutput::Group(0), Direction::Asc)])?
    .limited(limit)?;
    if let Some(after) = after {
        aggregate = aggregate.after(vec![Literal::from(after)])?;
    }
    let window = TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE)
        .map_err(|_| PlanError::Bounds)?;
    Ok(QueryPlan::new(
        scope,
        window,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    ))
}
