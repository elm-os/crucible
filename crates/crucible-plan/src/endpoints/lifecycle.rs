//! The reads of the retention and deletion executors (contract 13.2 and 13.3), and the columns their deletes
//! name.
//!
//! These are the only plans that are not clamped to a project's retention: the executors read exactly the rows
//! that reads hide, to find the partitions that hold them and to verify that they are gone. They take no
//! `RetentionClamp` and no caller, and run in the `analytics_raw` class.

use crucible_records::{ProjectScope, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{
    Column, ColumnRef, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol,
    McpDeploysCol, McpOpsCol, McpToolInventoryCol, McpToolRollupCol, McpToolRollupHourlyCol,
    SessionRollupCol, SpanCoreCol, SpanMetricsCol, SpanPayloadCol, TraceHeadsCol, TraceStatsCol,
    Ty, UserRollupCol, table,
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
        TableId::McpToolRollupHourly => named::<McpToolRollupHourlyCol>(name),
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
    /// The traces with an MCP operation of this `user_id`, or of one of these `user_subject_hash` values (at
    /// most [`TRACE_ID_LIST_CHUNK`]): the MCP rows that a deletion by user reaches by the user's own columns,
    /// whether or not a span of the trace names the user.
    McpUser(&'a str, &'a [[u8; 16]]),
}

/// `user_id = <user_id>`, or `user_subject_hash IN (<hashes>)` as well when there are hashes: the rows of an MCP
/// table that a deletion by user selects by the user's own columns (contract 13.3).
pub fn mcp_user(table_id: TableId, user_id: &str, hashes: &[[u8; 16]]) -> Result<Pred, PlanError> {
    let column = |name: &str| lifecycle_column(table_id, name).ok_or(PlanError::Table(table_id));
    let by_id = Pred::Cmp(column("user_id")?, CmpOp::Eq, Literal::from(user_id));
    if hashes.is_empty() {
        return Ok(by_id);
    }
    let hashes = hashes
        .iter()
        .map(|hash| Literal::Bytes(hash.to_vec()))
        .collect();
    Ok(Pred::Or(vec![
        by_id,
        Pred::In(column("user_subject_hash")?, hashes),
    ]))
}

/// One page of the traces a deletion selects, with the span of time their spans start in: the outputs are
/// `trace_id`, `min(start_time)` and `max(start_time)`, ordered by `trace_id`, at most `limit` rows after the
/// trace `after`. Only rows received before `requested_at` count, because a deletion keeps later data. The
/// spans of `span_core` are read, or the operations of `mcp_ops` for [`TraceSelector::McpUser`].
pub fn deletion_traces(
    scope: ProjectScope,
    selector: TraceSelector<'_>,
    requested_at: Timestamp,
    after: Option<TraceId>,
    limit: u32,
) -> Result<QueryPlan, PlanError> {
    let (table_id, selected) = match selector {
        TraceSelector::Traces(ids) => (
            TableId::SpanCore,
            trace_id_in(SpanCoreCol::TraceId.column_ref(), ids),
        ),
        TraceSelector::User(user_id) => (TableId::SpanCore, Pred::eq(SpanCoreCol::UserId, user_id)),
        TraceSelector::Session(session_id) => (
            TableId::SpanCore,
            Pred::eq(SpanCoreCol::SessionId, session_id),
        ),
        TraceSelector::McpUser(user_id, hashes) => {
            (TableId::McpOps, mcp_user(TableId::McpOps, user_id, hashes)?)
        }
    };
    let column = |name: &str| lifecycle_column(table_id, name).ok_or(PlanError::Table(table_id));
    let pred = Pred::And(vec![
        selected,
        Pred::Cmp(
            column("received_at")?,
            CmpOp::Lt,
            Literal::Time(requested_at),
        ),
    ]);
    let start = column("start_time")?;
    let mut aggregate = Aggregate::new(
        table_id,
        pred,
        [column("trace_id")?],
        vec![Measure::Min(start), Measure::Max(start)],
    )?
    .ordered(vec![(AggOutput::Group(0), Direction::Asc)])?
    .limited(limit)?;
    if let Some(after) = after {
        aggregate = aggregate.after(vec![Literal::from(after)])?;
    }
    Ok(QueryPlan::new(
        scope,
        every_instant()?,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    ))
}

fn every_instant() -> Result<TimeWindow, PlanError> {
    TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE).map_err(|_| PlanError::Bounds)
}

/// One page of the issuers of the project's MCP operations received before `requested_at`: the outputs are
/// `user_issuer` and the number of its operations, ordered by issuer, at most `limit` rows after the issuer
/// `after`. A deletion by user that names no issuer hashes the user under every one of them (contract 13.3).
pub fn deletion_issuers(
    scope: ProjectScope,
    requested_at: Timestamp,
    after: Option<&str>,
    limit: u32,
) -> Result<QueryPlan, PlanError> {
    let pred = Pred::And(vec![
        Pred::cmp(McpOpsCol::UserIssuer, CmpOp::Ne, ""),
        Pred::cmp(McpOpsCol::ReceivedAt, CmpOp::Lt, requested_at),
    ]);
    let mut aggregate = Aggregate::new(
        TableId::McpOps,
        pred,
        [McpOpsCol::UserIssuer],
        vec![Measure::Count],
    )?
    .ordered(vec![(AggOutput::Group(0), Direction::Asc)])?
    .limited(limit)?;
    if let Some(after) = after {
        aggregate = aggregate.after(vec![Literal::from(after)])?;
    }
    Ok(QueryPlan::new(
        scope,
        every_instant()?,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    ))
}
