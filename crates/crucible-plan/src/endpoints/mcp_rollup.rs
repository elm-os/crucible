//! The MCP analytics over the aggregating tables (contract 9.9): servers, tools, one tool, the error classes,
//! clients by name, deploys and compare. These are the reads the 300 ms p95 dashboard target binds, so every
//! plan here reads `mcp_tool_rollup`, `mcp_deploys` or `mcp_tool_inventory` and nothing else, in class
//! `analytics`.
//!
//! Rollup plans group by `error_class` and `caller_fault`, both key columns: the service folds the groups into
//! calls, errors and the caller-fault calls it reports apart (contract 9.2), and merges their histograms.

use crucible_records::{ProjectScope, TimeWindow};
use crucible_schema::{Column, ColumnRef, McpDeploysCol, McpToolInventoryCol, McpToolRollupCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::endpoints::mcp::{McpRead, TOOLS_CALL, and};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Aggregate, Fill, Measure, Node, Step, TimeSeries};
use crate::plan::QueryPlan;
use crate::pred::Pred;

type R = McpToolRollupCol;

fn rollup_conditions(read: &McpRead<'_>) -> Result<Option<Vec<Pred>>, PlanError> {
    read.conditions(
        R::Side.column_ref(),
        R::ServerName.column_ref(),
        R::CaptureSource.column_ref(),
    )
}

fn sum(column: R) -> Measure {
    Measure::Sum(column.column_ref())
}

/// The rollup rows of a read grouped by `group` (then `error_class` and `caller_fault`) with `measures`, in
/// class `analytics`. `None` when no server is read from.
fn grouped(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    (read, extra): (&McpRead<'_>, Vec<Pred>),
    group: &[R],
    measures: Vec<Measure>,
) -> Result<Option<QueryPlan>, PlanError> {
    let Some(mut parts) = rollup_conditions(read)? else {
        return Ok(None);
    };
    parts.extend(extra);
    let mut columns: Vec<ColumnRef> = group.iter().map(|c| c.column_ref()).collect();
    columns.extend([R::ErrorClass.column_ref(), R::CallerFault.column_ref()]);
    let aggregate = Aggregate::new(R::TABLE, and(parts), columns, measures)?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Analytics),
        Node::Aggregate(aggregate),
    )
}

/// The servers of a read: `server_name`, `client_name`, `error_class`, `caller_fault`, then `sum(count)`, the
/// merged `dur_hist` and `max(last_seen)`. The service folds the clients into a distinct count per server.
pub fn mcp_servers(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
) -> Result<Option<QueryPlan>, PlanError> {
    grouped(
        scope,
        window,
        clamp,
        caller,
        (read, Vec::new()),
        &[R::ServerName, R::ClientName],
        vec![
            sum(R::Count),
            Measure::HistogramMerge(R::DurHist.column_ref()),
            Measure::Max(R::LastSeen.column_ref()),
        ],
    )
}

/// The tools of a read (the rows of `tools/call`): `server_name`, `target_name`, `error_class`,
/// `caller_fault`, then `sum(count)`, the merged `dur_hist`, `sum(request_bytes)`, `sum(response_bytes)` and
/// `max(last_seen)`.
pub fn mcp_tools(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
) -> Result<Option<QueryPlan>, PlanError> {
    grouped(
        scope,
        window,
        clamp,
        caller,
        (read, vec![Pred::eq(R::Method, TOOLS_CALL)]),
        &[R::ServerName, R::TargetName],
        vec![
            sum(R::Count),
            Measure::HistogramMerge(R::DurHist.column_ref()),
            sum(R::RequestBytes),
            sum(R::ResponseBytes),
            Measure::Max(R::LastSeen.column_ref()),
        ],
    )
}

fn tool_calls(name: &str) -> Vec<Pred> {
    vec![
        Pred::eq(R::Method, TOOLS_CALL),
        Pred::eq(R::TargetName, name),
    ]
}

/// The calls of one tool over time: `bucket`, `error_class`, `caller_fault`, then `sum(count)` and the merged
/// `dur_hist`, in buckets of `step` (see `mcp_series_step`); empty buckets are left out.
pub fn mcp_tool_series(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    name: &str,
    step: Step,
) -> Result<Option<QueryPlan>, PlanError> {
    let Some(mut parts) = rollup_conditions(read)? else {
        return Ok(None);
    };
    parts.extend(tool_calls(name));
    let aggregate = Aggregate::new(
        R::TABLE,
        and(parts),
        [R::ErrorClass, R::CallerFault],
        vec![
            sum(R::Count),
            Measure::HistogramMerge(R::DurHist.column_ref()),
        ],
    )?;
    let series = TimeSeries::new(aggregate, R::Minute, step, Fill::None)?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Analytics),
        Node::TimeSeries(series),
    )
}

/// The calls of one tool per server and client: `server_name`, `client_name`, `error_class`, `caller_fault`,
/// then `sum(count)`, the merged `dur_hist`, `sum(request_bytes)`, `sum(response_bytes)` and `max(last_seen)`.
/// The service folds them into the tool's totals, its error classes, its top clients and its servers.
pub fn mcp_tool_clients(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    name: &str,
) -> Result<Option<QueryPlan>, PlanError> {
    grouped(
        scope,
        window,
        clamp,
        caller,
        (read, tool_calls(name)),
        &[R::ServerName, R::ClientName],
        vec![
            sum(R::Count),
            Measure::HistogramMerge(R::DurHist.column_ref()),
            sum(R::RequestBytes),
            sum(R::ResponseBytes),
            Measure::Max(R::LastSeen.column_ref()),
        ],
    )
}

/// The error classes of a read: `method`, `target_name`, `error_class`, `caller_fault`, then `sum(count)`. The
/// groups of class `none` give the calls without an error, so the service has the totals too.
pub fn mcp_error_classes(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
) -> Result<Option<QueryPlan>, PlanError> {
    grouped(
        scope,
        window,
        clamp,
        caller,
        (read, Vec::new()),
        &[R::Method, R::TargetName],
        vec![sum(R::Count)],
    )
}

/// The clients of a read by self-reported name: `client_name`, `error_class`, `caller_fault`, then
/// `sum(count)`, the merged `dur_hist` and `max(last_seen)`.
pub fn mcp_clients(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
) -> Result<Option<QueryPlan>, PlanError> {
    grouped(
        scope,
        window,
        clamp,
        caller,
        (read, Vec::new()),
        &[R::ClientName],
        vec![
            sum(R::Count),
            Measure::HistogramMerge(R::DurHist.column_ref()),
            Measure::Max(R::LastSeen.column_ref()),
        ],
    )
}

/// The tools of one server per deploy, for compare: `deploy_id`, `target_name`, `error_class`,
/// `caller_fault`, then `sum(count)` and the merged `dur_hist`. With `deploys`, those deploys only; the read
/// must name its server.
pub fn mcp_compare_tools(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    deploys: &[String],
) -> Result<Option<QueryPlan>, PlanError> {
    if read.server.is_none() {
        return Err(PlanError::UnsupportedFilter {
            field: "server",
            target: "a compare, which reads one server",
        });
    }
    let mut extra = vec![Pred::eq(R::Method, TOOLS_CALL)];
    if !deploys.is_empty() {
        let ids = deploys.iter().cloned().map(Literal::String).collect();
        extra.push(Pred::In(R::DeployId.column_ref(), ids));
    }
    grouped(
        scope,
        window,
        clamp,
        caller,
        (read, extra),
        &[R::DeployId, R::TargetName],
        vec![
            sum(R::Count),
            Measure::HistogramMerge(R::DurHist.column_ref()),
        ],
    )
}

/// The deploys seen in the window (contract 9.7): `server_name`, `deploy_id`, `server_version`,
/// `capture_source`, then `min(first_seen)` and `max(last_seen)`. Deploys are recorded from server-side rows,
/// so the read takes no side, and the service picks each server's capture source among the deploys' own. The
/// window applies after grouping: a deploy last seen before it, or first seen after it, is left out.
pub fn mcp_deploys(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    server: Option<&str>,
) -> Result<Option<QueryPlan>, PlanError> {
    let pred = match server {
        Some(name) => Pred::eq(McpDeploysCol::ServerName, name),
        None => Pred::all(),
    };
    let aggregate = Aggregate::new(
        McpDeploysCol::TABLE,
        pred,
        [
            McpDeploysCol::ServerName,
            McpDeploysCol::DeployId,
            McpDeploysCol::ServerVersion,
            McpDeploysCol::CaptureSource,
        ],
        vec![
            Measure::Min(McpDeploysCol::FirstSeen.column_ref()),
            Measure::Max(McpDeploysCol::LastSeen.column_ref()),
        ],
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Analytics),
        Node::Aggregate(aggregate),
    )
}

/// The tool inventory seen in the window (contract 3.6): `server_name`, `deploy_id`, `tool_name`,
/// `schema_hash`, then `min(first_seen)` and `max(last_seen)`. With `server`, that server only; with
/// `deploys`, those deploys only. The inventory has no side and is read whoever observed it.
pub fn mcp_inventory(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    server: Option<&str>,
    deploys: &[String],
) -> Result<Option<QueryPlan>, PlanError> {
    let mut parts = Vec::new();
    if let Some(name) = server {
        parts.push(Pred::eq(McpToolInventoryCol::ServerName, name));
    }
    if !deploys.is_empty() {
        let ids = deploys.iter().cloned().map(Literal::String).collect();
        parts.push(Pred::In(McpToolInventoryCol::DeployId.column_ref(), ids));
    }
    let aggregate = Aggregate::new(
        McpToolInventoryCol::TABLE,
        Pred::And(parts),
        [
            McpToolInventoryCol::ServerName,
            McpToolInventoryCol::DeployId,
            McpToolInventoryCol::ToolName,
            McpToolInventoryCol::SchemaHash,
        ],
        vec![
            Measure::Min(McpToolInventoryCol::FirstSeen.column_ref()),
            Measure::Max(McpToolInventoryCol::LastSeen.column_ref()),
        ],
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Analytics),
        Node::Aggregate(aggregate),
    )
}
