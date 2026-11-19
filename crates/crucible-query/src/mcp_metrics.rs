//! The metrics query over the `mcp_ops` source (contracts 8.8 and 9.9): the series of one alert measure of the
//! registry, read from `mcp_tool_rollup` for one side under the capture-source selection of contract 9.5, by
//! default without the calls the caller caused (contract 9.2), and the totals an alert's evaluation reads.

use std::sync::Arc;

use crucible_plan::{
    Aggregation, McpRead, MeasureId, MetricsQuery, QueryClass, SourceChoice, Tier, Value,
    mcp_metrics_other, mcp_metrics_ranking, mcp_metrics_series, mcp_metrics_totals, metrics_keys,
    metrics_shape,
};
use crucible_records::{ErrorClass, McpSide, TableId, TimeWindow, Timestamp};

use crate::context::{QueryServiceError, ReadContext};
use crate::decode::Fields;
use crate::mcp::McpQuery;
use crate::metrics::{
    MetricValue, MetricsPoint, MetricsResult, MetricsSeries, MetricsTotal, MetricsTotals, by_key,
    measure_value,
};
use crate::service::QueryService;

/// A metrics request over the `mcp_ops` source.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpMetricsRequest {
    /// The window over the buckets' time.
    pub window: TimeWindow,
    /// The query: one measure of the `mcp_ops` source, which `crucible_plan::metrics_shape` accepts.
    pub query: MetricsQuery,
    /// The side read.
    pub side: McpSide,
    /// `auto` or one capture-source class; `auto` is resolved for the request's window.
    pub source: SourceChoice,
    /// Whether caller-fault calls count; false for the alert measures.
    pub include_caller_faults: bool,
}

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!(
        "the store returned an MCP metrics row without a valid {what}"
    ))
}

const PPM: u64 = 1_000_000;

/// The value of a bucket of `mcp.error_rate` from its rows per error class (`error_class`, `sum(count)`):
/// failed calls over all calls in parts per million, rounded down; `None` without calls.
fn rate(rows: &[&Vec<Value>]) -> Result<Option<MetricValue>, QueryServiceError> {
    let (mut calls, mut errors) = (0u64, 0u64);
    for values in rows {
        let (Some(Value::Enum(class)), Some(Value::UInt(count))) = (values.first(), values.get(1))
        else {
            return Err(bad("error class"));
        };
        calls = calls.saturating_add(*count);
        if *class != ErrorClass::None.value() {
            errors = errors.saturating_add(*count);
        }
    }
    Ok((calls > 0).then(|| per_million(errors, calls)))
}

/// `errors` over `calls` in parts per million, rounded down.
fn per_million(errors: u64, calls: u64) -> MetricValue {
    let ppm = u128::from(errors) * u128::from(PPM) / u128::from(calls.max(1));
    MetricValue::UInt(u64::try_from(ppm).unwrap_or(u64::MAX))
}

/// The totals of an error measure from the rows of `crucible_plan::mcp_metrics_totals` (the dimensions,
/// `error_class`, the calls, in key order): per key, its errors for `mcp.error_count`, or its errors over its
/// calls for `mcp.error_rate`.
fn error_totals(
    rows: &[crucible_plan::Row],
    dimensions: usize,
    measure: MeasureId,
) -> Result<Vec<MetricsTotal>, QueryServiceError> {
    let mut keys: Vec<(Vec<Arc<str>>, u64, u64)> = Vec::new();
    for row in rows {
        let f = Fields(row.values());
        let key = (0..dimensions)
            .map(|i| f.text(i))
            .collect::<Result<Vec<_>, _>>()?;
        let failed = f.enum8(dimensions)? != ErrorClass::None.value();
        let count = f.uint(dimensions + 1)?;
        let errors = if failed { count } else { 0 };
        match keys.last_mut() {
            Some((last, calls, failures)) if *last == key => {
                *calls = calls.saturating_add(count);
                *failures = failures.saturating_add(errors);
            }
            _ => keys.push((key, count, errors)),
        }
    }
    Ok(keys
        .into_iter()
        .map(|(key, calls, errors)| MetricsTotal {
            key,
            value: match measure {
                MeasureId::McpErrorRate => (calls > 0).then(|| per_million(errors, calls)),
                _ => Some(MetricValue::UInt(errors)),
            },
        })
        .collect())
}

/// At most `limit` of `totals`, the nearest to a breach first: by value in `largest_first` order, a key
/// without a value last, ties in ascending key order.
fn nearest(mut totals: Vec<MetricsTotal>, largest_first: bool, limit: u32) -> MetricsTotals {
    let number = |value: &Option<MetricValue>| match value {
        Some(MetricValue::UInt(n)) => Some(i128::from(*n)),
        Some(MetricValue::Int(n)) => Some(i128::from(*n)),
        None => None,
    };
    totals.sort_by(|a, b| {
        let by_value = match (number(&a.value), number(&b.value)) {
            (Some(x), Some(y)) if largest_first => y.cmp(&x),
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_value.then_with(|| a.key.cmp(&b.key))
    });
    let truncated = totals.len() > limit as usize;
    totals.truncate(limit as usize);
    MetricsTotals {
        series: totals,
        truncated,
    }
}

/// The points of one key: one per bucket, folding the rows of a bucket for the rate.
fn points(
    buckets: &[(Timestamp, Vec<Value>)],
    aggregation: Aggregation,
    by_error_class: bool,
) -> Result<Vec<MetricsPoint>, QueryServiceError> {
    let mut points: Vec<MetricsPoint> = Vec::new();
    let mut at = 0;
    while let Some((bucket, values)) = buckets.get(at) {
        let mut end = at + 1;
        while buckets.get(end).is_some_and(|(b, _)| b == bucket) {
            end += 1;
        }
        let value = if by_error_class {
            let rows: Vec<&Vec<Value>> = buckets[at..end].iter().map(|(_, v)| v).collect();
            rate(&rows)?
        } else {
            measure_value(aggregation, values.first().ok_or_else(|| bad("measure"))?)?
        };
        points.push(MetricsPoint {
            bucket: *bucket,
            value,
        });
        at = end;
    }
    Ok(points)
}

impl QueryService {
    /// The series of an `mcp_ops` metrics query (contracts 8.8 and 9.9): the capture-source selection of the
    /// side in the window first, then the ranking when the query has dimensions, the series of the first
    /// `top_n` keys and, for an additive measure cut by `top_n`, the `other` series.
    pub async fn mcp_metrics(
        &self,
        ctx: &ReadContext,
        request: McpMetricsRequest,
    ) -> Result<MetricsResult, QueryServiceError> {
        let McpMetricsRequest {
            window,
            query,
            side,
            source,
            include_caller_faults,
        } = request;
        let shape = metrics_shape(window, &query)
            .map_err(|error| QueryServiceError::Internal(error.to_string()))?;
        let [(measure, aggregation)] = query.measures.as_slice() else {
            return Err(QueryServiceError::Validation("measures"));
        };
        let (measure, aggregation) = (*measure, *aggregation);
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let selection = read
            .mcp_selection(&McpQuery {
                window,
                side,
                source,
                include_caller_faults,
                server: None,
            })
            .await?;
        let mcp = McpRead {
            side,
            selection: &selection,
            include_caller_faults,
            server: None,
        };
        let (scope, caller) = (ctx.scope, ctx.caller);
        let plan_error = |error: crucible_plan::MetricsError| match error {
            crucible_plan::MetricsError::Plan(error) => QueryServiceError::from(error),
            other => QueryServiceError::Internal(other.to_string()),
        };

        let (mut cut, mut keys) = (false, None);
        if shape.keys > 0 {
            let plan = mcp_metrics_ranking(scope, window, &read.clamp, caller, &query, &mcp);
            let rows = read.rows(Ok(plan.map_err(plan_error)?)).await?;
            let mut ranked = metrics_keys(&query, &rows).ok_or_else(|| bad("dimension key"))?;
            cut = ranked.len() > shape.keys as usize;
            ranked.truncate(shape.keys as usize);
            keys = Some(ranked);
        }
        let (mut series_rows, mut other_rows) = (Vec::new(), Vec::new());
        if keys.as_ref().is_none_or(|keys| !keys.is_empty()) {
            let plan = mcp_metrics_series(
                scope,
                window,
                &read.clamp,
                caller,
                &query,
                (&mcp, keys.as_deref()),
            );
            series_rows = read.rows(Ok(plan.map_err(plan_error)?)).await?;
            if let (true, true, Some(keys)) = (cut, measure.additive(), &keys) {
                let plan =
                    mcp_metrics_other(scope, window, &read.clamp, caller, &query, (&mcp, keys));
                other_rows = read.rows(Ok(plan.map_err(plan_error)?)).await?;
            }
        }
        let mut series = Vec::new();
        for (key, buckets) in by_key(series_rows, &query.dimensions)? {
            series.push(MetricsSeries {
                measure,
                aggregation,
                key,
                other: false,
                points: points(&buckets, aggregation, measure.by_error_class())?,
            });
        }
        // Series come in ranking order, as the keys were ranked.
        if let Some(keys) = &keys {
            let rank = |key: &[std::sync::Arc<str>]| {
                keys.iter().position(|ranked| {
                    ranked.len() == key.len()
                        && ranked.iter().zip(key).all(|(literal, value)| {
                            matches!(literal, crucible_plan::Literal::String(s) if s.as_str() == &**value)
                        })
                })
            };
            series.sort_by_key(|s| rank(&s.key).unwrap_or(usize::MAX));
        }
        for (_, buckets) in by_key(other_rows, &[])? {
            series.push(MetricsSeries {
                measure,
                aggregation,
                key: Vec::new(),
                other: true,
                points: points(&buckets, aggregation, false)?,
            });
        }
        let (first, count) = shape.buckets;
        let end = first.saturating_add_micros(
            i64::try_from(count)
                .unwrap_or(i64::MAX)
                .saturating_mul(shape.step.seconds())
                .saturating_mul(1_000_000),
        );
        let state = self.index.read_state(scope);
        let meta = read.finish(
            read.now,
            consistency,
            state,
            &[TableId::McpToolRollup],
            false,
        )?;
        Ok(MetricsResult {
            step: shape.step,
            tier: Tier::Rollup,
            series,
            truncated: cut && !measure.additive(),
            partial_last_bucket: end > meta.as_of,
            meta,
        })
    }
}

impl QueryService {
    /// The value of one `mcp_ops` measure per dimension key over the whole of `window`, for an alert's
    /// evaluation (contracts 9.5 and 9.9, class `alert`). The capture-source selection is resolved for this
    /// window, so `auto` follows the sources each evaluation sees. As in [`QueryService::metrics_totals`], a
    /// store that does not answer is an error, never an empty answer, and at most `limit` keys are kept: in the
    /// order of `crucible_plan::mcp_metrics_totals` for `mcp.calls` and the percentiles, and for the error
    /// measures, which are read whole, the nearest to a breach.
    pub async fn mcp_metrics_totals(
        &self,
        ctx: &ReadContext,
        request: McpMetricsRequest,
        largest_first: bool,
        limit: u32,
    ) -> Result<MetricsTotals, QueryServiceError> {
        let McpMetricsRequest {
            window,
            query,
            side,
            source,
            include_caller_faults,
        } = request;
        let [(measure, aggregation)] = query.measures.as_slice() else {
            return Err(QueryServiceError::Validation("measures"));
        };
        let (measure, aggregation) = (*measure, *aggregation);
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let selection = read
            .mcp_selection_in(
                &McpQuery {
                    window,
                    side,
                    source,
                    include_caller_faults,
                    server: None,
                },
                QueryClass::Alert,
            )
            .await?;
        let mcp = McpRead {
            side,
            selection: &selection,
            include_caller_faults,
            server: None,
        };
        let plan = mcp_metrics_totals(
            ctx.scope,
            window,
            &read.clamp,
            &query,
            &mcp,
            (largest_first, limit.saturating_add(1)),
        )
        .map_err(|error| match error {
            crucible_plan::MetricsError::Plan(error) => QueryServiceError::from(error),
            other => QueryServiceError::Internal(other.to_string()),
        })?;
        let rows = read.rows(Ok(plan)).await?;
        let state = self.index.read_state(ctx.scope);
        read.finish(
            read.now,
            consistency,
            state,
            &[TableId::McpToolRollup],
            false,
        )?;
        let dimensions = query.dimensions.len();
        if matches!(measure, MeasureId::McpErrorCount | MeasureId::McpErrorRate) {
            let totals = error_totals(&rows, dimensions, measure)?;
            return Ok(nearest(totals, largest_first, limit));
        }
        let mut series = Vec::new();
        for row in &rows {
            let f = Fields(row.values());
            let key = (0..dimensions)
                .map(|i| f.text(i))
                .collect::<Result<Vec<_>, _>>()?;
            let value = match f.uint(dimensions + 1)? {
                0 => None,
                _ => {
                    let total = row.values().get(dimensions).ok_or_else(|| bad("measure"))?;
                    measure_value(aggregation, total)?
                }
            };
            series.push(MetricsTotal { key, value });
        }
        let truncated = series.len() > limit as usize;
        series.truncate(limit as usize);
        Ok(MetricsTotals { series, truncated })
    }
}
