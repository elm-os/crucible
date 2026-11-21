//! The metrics query over the `mcp_ops` source (contracts 8.8 and 9.9): the alert measures `mcp.calls`,
//! `mcp.error_count`, `mcp.error_rate`, `mcp.duration_p50`, `mcp.duration_p90` and `mcp.duration_p99`, read from
//! `mcp_tool_rollup` in class `analytics`, one measure per query, optionally split by server or target.
//!
//! The query is read like any metrics query ([`super::metrics`]): with dimensions, [`mcp_metrics_ranking`]
//! first, then [`mcp_metrics_series`] for the ranked keys and [`mcp_metrics_other`] for the rest of an additive
//! measure. Every plan reads one side under the capture-source selection of contract 9.5 ([`McpRead`]) and, by
//! default, without the calls the caller caused (contract 9.2). `mcp.error_count` counts the classes other than
//! `none`; `mcp.error_rate` is grouped by `error_class` as well, and the service folds its rows into errors over
//! calls. An alert reads one value per key over its whole window with [`mcp_metrics_totals`].

use crucible_records::{ProjectScope, TimeWindow};
use crucible_schema::{Column, McpToolRollupCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::endpoints::mcp::{McpRead, and, rollup_plan};
use crate::endpoints::metrics::{
    MetricsError, MetricsQuery, among, dimensions, invalid, metrics_shape,
};
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Node, TimeSeries};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::{METRICS_MAX_DIMENSIONS, MeasureId, Source};

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
    Ok(rollup_plan(
        clamp,
        scope,
        window,
        caller.class(QueryClass::Analytics),
        root,
    )?)
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

/// The value of the one measure per dimension key over the whole of `window`, for an alert (contract 9.9,
/// class `alert`). `mcp.calls` and the percentiles are read as `crate::metrics_totals` reads the span rollup:
/// the dimensions, the measure and the calls, at most `limit` keys, by the calls in `largest_first` order for
/// `mcp.calls` and by descending calls for a percentile, ties in ascending key order. The two error measures
/// are grouped by `error_class` as well and not limited, with every call of the key counted, so that the
/// service folds each key's errors over its calls, and a key with calls and no error reads zero rather than
/// no data: the dimensions, `error_class`, then the calls.
pub fn mcp_metrics_totals(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    query: &MetricsQuery,
    read: &McpRead<'_>,
    (largest_first, limit): (bool, u32),
) -> Result<Option<QueryPlan>, MetricsError> {
    let id = measure(query)?;
    if !query
        .measures
        .iter()
        .all(|(_, a)| id.aggregations().contains(a))
    {
        return Err(invalid(
            "measures",
            "the measure does not allow the aggregation",
        ));
    }
    if query.dimensions.len() > METRICS_MAX_DIMENSIONS {
        return Err(invalid("dimensions", "at most two dimensions are allowed"));
    }
    if let [first, second] = query.dimensions.as_slice()
        && first == second
    {
        return Err(invalid("dimensions", "a dimension is listed twice"));
    }
    if query
        .dimensions
        .iter()
        .any(|d| d.source() != Source::McpOps)
    {
        return Err(invalid(
            "dimensions",
            "an MCP query is split by server_name or target_name",
        ));
    }
    let errors = matches!(id, MeasureId::McpErrorCount | MeasureId::McpErrorRate);
    let calls = MeasureId::McpCalls;
    let Some(pred) = rows(query, read, if errors { calls } else { id }, None)? else {
        return Ok(None);
    };
    let mut group = dimensions(query);
    let aggregate = if errors {
        group.push(R::ErrorClass.column_ref());
        let order = (0..group.len())
            .map(|i| (AggOutput::Group(i), Direction::Asc))
            .collect();
        Aggregate::new(R::TABLE, pred, group, vec![calls.measure()])?.ordered(order)?
    } else {
        let by = if id.additive() {
            let direction = if largest_first {
                Direction::Desc
            } else {
                Direction::Asc
            };
            (AggOutput::Measure(0), direction)
        } else {
            (AggOutput::Measure(1), Direction::Desc)
        };
        let mut order = vec![by];
        order.extend((0..group.len()).map(|i| (AggOutput::Group(i), Direction::Asc)));
        Aggregate::new(R::TABLE, pred, group, vec![id.measure(), calls.measure()])?
            .ordered(order)?
            .limited(limit)?
    };
    Ok(rollup_plan(
        clamp,
        scope,
        window,
        QueryClass::Alert,
        Node::Aggregate(aggregate),
    )?)
}
