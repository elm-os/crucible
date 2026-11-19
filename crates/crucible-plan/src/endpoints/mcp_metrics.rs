//! The metrics query over the `mcp_ops` source (contracts 8.8 and 9.9): the alert measures `mcp.calls`,
//! `mcp.error_count`, `mcp.error_rate`, `mcp.duration_p50`, `mcp.duration_p90` and `mcp.duration_p99`, read from
//! `mcp_tool_rollup` in class `analytics`, one measure per query, optionally split by server or target.
//!
//! The query is read like any metrics query ([`super::metrics`]): with dimensions, [`mcp_metrics_ranking`]
//! first, then [`mcp_metrics_series`] for the ranked keys and [`mcp_metrics_other`] for the rest of an additive
//! measure. Every plan reads one side under the capture-source selection of contract 9.5 ([`McpRead`]) and, by
//! default, without the calls the caller caused (contract 9.2). `mcp.error_count` counts the classes other than
//! `none`; `mcp.error_rate` is grouped by `error_class` as well, and the service folds its rows into errors over
//! calls.

use crucible_records::{ProjectScope, TimeWindow};
use crucible_schema::{Column, McpToolRollupCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::endpoints::mcp::{McpRead, and};
use crate::endpoints::metrics::{
    MetricsError, MetricsQuery, among, dimensions, invalid, metrics_shape,
};
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Node, TimeSeries};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::{MeasureId, Source};

type R = McpToolRollupCol;

/// The one measure of an MCP query.
fn measure(query: &MetricsQuery) -> Result<MeasureId, MetricsError> {
    match query.measures.as_slice() {
        [(id, _)] if query.source == Source::McpOps && id.source() == Source::McpOps => Ok(*id),
        _ => Err(invalid(
            "measures",
            "an MCP metrics query takes one measure of the mcp_ops source",
        )),
    }
}

/// The rows of a read and a measure: the side, the chosen capture sources and, unless included, no caller
/// faults; then the measure's own condition and `extra`. `None` when no server is read from.
fn rows(
    query: &MetricsQuery,
    read: &McpRead<'_>,
    id: MeasureId,
    extra: Option<Pred>,
) -> Result<Option<Pred>, MetricsError> {
    if !query.filter.is_all() {
        return Err(invalid("filter", "the source has no filter fields yet"));
    }
    let Some(mut parts) = read.conditions(
        R::Side.column_ref(),
        R::ServerName.column_ref(),
        R::CaptureSource.column_ref(),
    )?
    else {
        return Ok(None);
    };
    if !read.include_caller_faults {
        parts.push(Pred::eq(R::CallerFault, 0u32));
    }
    parts.extend(id.condition());
    parts.extend(extra);
    Ok(Some(and(parts)))
}

fn plan(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    root: Node,
) -> Result<Option<QueryPlan>, MetricsError> {
    Ok(clamp.plan(scope, window, caller.class(QueryClass::Analytics), root)?)
}

/// Step 1 of a query with dimensions: the dimension keys by descending value of the measure when it is
/// additive, else of the call count, ties in ascending key order, at most `top_n + 1` rows. The outputs are
/// the dimensions, then the ranking measure.
pub fn mcp_metrics_ranking(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    query: &MetricsQuery,
    read: &McpRead<'_>,
) -> Result<Option<QueryPlan>, MetricsError> {
    let shape = metrics_shape(window, query)?;
    if shape.keys == 0 {
        return Err(invalid("dimensions", "a ranking needs a dimension"));
    }
    let id = measure(query)?;
    let rank = if id.additive() {
        id
    } else {
        MeasureId::McpCalls
    };
    let Some(pred) = rows(query, read, rank, None)? else {
        return Ok(None);
    };
    let group = dimensions(query);
    let mut order = vec![(AggOutput::Measure(0), Direction::Desc)];
    order.extend((0..group.len()).map(|i| (AggOutput::Group(i), Direction::Asc)));
    let aggregate = Aggregate::new(R::TABLE, pred, group, vec![rank.measure()])?
        .ordered(order)?
        .limited(shape.keys + 1)?;
    plan(scope, window, clamp, caller, Node::Aggregate(aggregate))
}

/// The series of a query: `bucket`, the dimensions, then `error_class` for `mcp.error_rate`, then the plan
/// measure (a sum of `count`, or the merged `dur_hist` whose percentile is a post-operator). A query with
/// dimensions takes the keys to return, from [`mcp_metrics_ranking`]; a query without takes `None`.
pub fn mcp_metrics_series(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    query: &MetricsQuery,
    (read, keys): (&McpRead<'_>, Option<&[Vec<Literal>]>),
) -> Result<Option<QueryPlan>, MetricsError> {
    let shape = metrics_shape(window, query)?;
    let id = measure(query)?;
    let among = match (shape.keys, keys) {
        (0, None) => None,
        (0, Some(_)) => return Err(invalid("keys", "a query without dimensions takes no keys")),
        (_, None) => {
            return Err(invalid(
                "keys",
                "a query with dimensions takes the ranked keys",
            ));
        }
        (most, Some(keys)) if keys.len() > most as usize => {
            return Err(invalid("keys", "more keys than top_n"));
        }
        (_, Some(keys)) => Some(among(query, keys)?),
    };
    let Some(pred) = rows(query, read, id, among)? else {
        return Ok(None);
    };
    let mut group = dimensions(query);
    if id.by_error_class() {
        group.push(R::ErrorClass.column_ref());
    }
    let aggregate = Aggregate::new(R::TABLE, pred, group, vec![id.measure()])?;
    let series = TimeSeries::new(aggregate, R::Minute, shape.step, query.fill)?;
    plan(scope, window, clamp, caller, Node::TimeSeries(series))
}

/// The `other` series of an additive measure with dimensions: `bucket`, then the sum over every row whose
/// dimensions are none of `keys`.
pub fn mcp_metrics_other(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    query: &MetricsQuery,
    (read, keys): (&McpRead<'_>, &[Vec<Literal>]),
) -> Result<Option<QueryPlan>, MetricsError> {
    let shape = metrics_shape(window, query)?;
    let id = measure(query)?;
    if !shape.has_other() || !id.additive() {
        return Err(invalid("measures", "the query has no other series"));
    }
    let rest = Pred::Not(Box::new(among(query, keys)?));
    let Some(pred) = rows(query, read, id, Some(rest))? else {
        return Ok(None);
    };
    let aggregate = Aggregate::new(R::TABLE, pred, [] as [R; 0], vec![id.measure()])?;
    let series = TimeSeries::new(aggregate, R::Minute, shape.step, query.fill)?;
    plan(scope, window, clamp, caller, Node::TimeSeries(series))
}
