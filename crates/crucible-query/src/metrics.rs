//! The metrics query (contract 8.8): the series of a `MetricsQuery` from the plans of `crucible-plan`, with the
//! histogram quantiles as a post-operator (contract 6.5), the `other` series of additive measures and the
//! `truncated` flag of percentiles.

use std::collections::BTreeMap;
use std::sync::Arc;

use crucible_plan::{
    Aggregation, DimensionId, DimensionType, MeasureId, MetricsQuery, Row, Source, Step, Tier,
    Value, metrics_keys, metrics_other, metrics_ranking, metrics_series, metrics_shape,
};
use crucible_records::{Histogram, Level, ObservationType, TableId, TimeWindow, Timestamp};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::service::QueryService;

/// A metrics request: the query and the window it covers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetricsRequest {
    /// The window over the buckets' time.
    pub window: TimeWindow,
    /// The query, which `crucible_plan::metrics_shape` accepts for the window.
    pub query: MetricsQuery,
}

/// A value of a measure in one bucket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetricValue {
    /// A count, a token sum or a percentile in microseconds.
    UInt(u64),
    /// A cost sum in nano-USD.
    Int(i64),
}

/// One bucket of a series. `value` is `None` for a percentile of a bucket without spans.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MetricsPoint {
    /// The bucket's first instant.
    pub bucket: Timestamp,
    /// The value.
    pub value: Option<MetricValue>,
}

/// One series: one measure of one dimension key, or of the `other` series.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetricsSeries {
    /// The measure.
    pub measure: MeasureId,
    /// How it is aggregated.
    pub aggregation: Aggregation,
    /// The dimension values of the key, in the query's dimension order: wire names for enums. Empty for a
    /// query without dimensions and for the `other` series.
    pub key: Vec<Arc<str>>,
    /// Whether this is the `other` series: every key the ranking did not return.
    pub other: bool,
    /// The buckets in time order.
    pub points: Vec<MetricsPoint>,
}

/// The answer to a metrics query.
#[derive(Clone, PartialEq, Debug)]
pub struct MetricsResult {
    /// The granularity used.
    pub step: Step,
    /// The tier the measures were read from.
    pub tier: Tier,
    /// The series: per dimension key in ranking order (or the one series of a query without dimensions), one
    /// per measure in query order; then the `other` series of the additive measures.
    pub series: Vec<MetricsSeries>,
    /// Whether keys beyond `top_n` were dropped for a percentile measure, which has no `other` series.
    pub truncated: bool,
    /// Whether the last bucket ends after `meta.as_of`, so it may still grow.
    pub partial_last_bucket: bool,
    /// The response metadata.
    pub meta: Meta,
}

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!(
        "the store returned a metrics row without a valid {what}"
    ))
}

/// A dimension value as the wire names it.
fn dimension_value(dimension: DimensionId, value: &Value) -> Result<Arc<str>, QueryServiceError> {
    let name = match (dimension.ty(), value) {
        (DimensionType::String, Value::String(text)) => Arc::from(text.as_str()),
        (DimensionType::String, Value::Bytes(bytes)) => Arc::from(String::from_utf8_lossy(bytes)),
        (DimensionType::Enum, Value::Enum(v)) => match dimension {
            DimensionId::Level => Level::from_value(*v).map(|l| l.as_str()),
            _ => ObservationType::from_value(*v).map(|o| o.as_str()),
        }
        .map(Arc::from)
        .ok_or_else(|| bad("dimension value"))?,
        _ => return Err(bad("dimension value")),
    };
    Ok(name)
}

/// The value of a measure: a sum as it is, a percentile through the histogram kernel of contract 6.5.
fn measure_value(
    aggregation: Aggregation,
    value: &Value,
) -> Result<Option<MetricValue>, QueryServiceError> {
    match (aggregation.quantile_ppm(), value) {
        (None, Value::UInt(v)) => Ok(Some(MetricValue::UInt(*v))),
        (None, Value::Int(v)) => Ok(Some(MetricValue::Int(*v))),
        (Some(ppm), Value::Tuple2(keys, counts)) => {
            let (Value::Array(keys), Value::Array(counts)) = (&**keys, &**counts) else {
                return Err(bad("histogram"));
            };
            let keys: Option<Vec<u16>> = keys
                .iter()
                .map(|k| match k {
                    Value::UInt(k) => u16::try_from(*k).ok(),
                    _ => None,
                })
                .collect();
            let counts: Option<Vec<u64>> = counts
                .iter()
                .map(|c| match c {
                    Value::UInt(c) => Some(*c),
                    _ => None,
                })
                .collect();
            let histogram = keys
                .zip(counts)
                .and_then(|(keys, counts)| Histogram::from_sum_map(&keys, &counts))
                .ok_or_else(|| bad("histogram"))?;
            Ok(histogram.quantile(ppm).map(MetricValue::UInt))
        }
        _ => Err(bad("measure")),
    }
}

/// The buckets of one series key: each bucket's first instant with the plan's measures.
type Buckets = Vec<(Timestamp, Vec<Value>)>;

/// The series of a plan: each dimension key with its buckets.
type Keyed = Vec<(Vec<Arc<str>>, Buckets)>;

/// Splits the rows of a series plan into the buckets of each dimension key, in the order the keys first appear.
fn by_key(rows: Vec<Row>, dimensions: &[DimensionId]) -> Result<Keyed, QueryServiceError> {
    let mut keys: Vec<Vec<Arc<str>>> = Vec::new();
    let mut buckets: BTreeMap<usize, Buckets> = BTreeMap::new();
    for row in rows {
        let mut values = row.into_values().into_iter();
        let Some(Value::Time(bucket)) = values.next() else {
            return Err(bad("bucket"));
        };
        let key = dimensions
            .iter()
            .map(|d| dimension_value(*d, &values.next().ok_or_else(|| bad("dimension"))?))
            .collect::<Result<Vec<_>, _>>()?;
        let at = match keys.iter().position(|k| *k == key) {
            Some(at) => at,
            None => {
                keys.push(key);
                keys.len() - 1
            }
        };
        buckets
            .entry(at)
            .or_default()
            .push((bucket, values.collect()));
    }
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(at, key)| {
            let mut points = buckets.remove(&at).unwrap_or_default();
            points.sort_by_key(|(bucket, _)| *bucket);
            (key, points)
        })
        .collect())
}

impl QueryService {
    /// The series of a metrics query (contract 8.8). A query with dimensions reads the ranking first, then
    /// the series of the first `top_n` keys and, when the ranking returned more and an additive measure is
    /// asked for, the `other` series.
    pub async fn metrics(
        &self,
        ctx: &ReadContext,
        request: MetricsRequest,
    ) -> Result<MetricsResult, QueryServiceError> {
        let MetricsRequest { window, query } = request;
        let shape = metrics_shape(window, &query)
            .map_err(|error| QueryServiceError::Internal(error.to_string()))?;
        let mut read = self.read(ctx, "metrics");
        let (scope, caller) = (ctx.scope, ctx.caller);
        let consistency = self.consistency();
        let state = self.index.read_state(scope);
        let plan_error = |error: crucible_plan::MetricsError| match error {
            crucible_plan::MetricsError::Plan(error) => QueryServiceError::from(error),
            other => QueryServiceError::Internal(other.to_string()),
        };

        let mut cut = false;
        let mut keys = None;
        if shape.keys > 0 {
            let plan = metrics_ranking(scope, window, &read.clamp, caller, &query);
            let rows = read.rows(Ok(plan.map_err(plan_error)?)).await?;
            let mut ranked = metrics_keys(&query, &rows).ok_or_else(|| bad("dimension key"))?;
            cut = ranked.len() > shape.keys as usize;
            ranked.truncate(shape.keys as usize);
            keys = Some(ranked);
        }
        let mut series = Vec::new();
        let mut series_rows = Vec::new();
        let mut other_rows = Vec::new();
        if keys.as_ref().is_none_or(|keys| !keys.is_empty()) {
            let plan = metrics_series(scope, window, &read.clamp, caller, &query, keys.as_deref());
            series_rows = read.rows(Ok(plan.map_err(plan_error)?)).await?;
            if let (true, Some(keys)) = (cut && shape.has_other(), &keys) {
                let plan = metrics_other(scope, window, &read.clamp, caller, &query, keys);
                other_rows = read.rows(Ok(plan.map_err(plan_error)?)).await?;
            }
        }
        let point = |bucket: Timestamp, values: &[Value], column: usize, aggregation| {
            let value = values.get(column).ok_or_else(|| bad("measure"))?;
            Ok::<_, QueryServiceError>(MetricsPoint {
                bucket,
                value: measure_value(aggregation, value)?,
            })
        };
        for (key, buckets) in by_key(series_rows, &query.dimensions)? {
            for (i, (measure, aggregation)) in query.measures.iter().enumerate() {
                let points = buckets
                    .iter()
                    .map(|(bucket, values)| point(*bucket, values, shape.columns[i], *aggregation))
                    .collect::<Result<_, _>>()?;
                series.push(MetricsSeries {
                    measure: *measure,
                    aggregation: *aggregation,
                    key: key.clone(),
                    other: false,
                    points,
                });
            }
        }
        for (_, buckets) in by_key(other_rows, &[])? {
            for (i, (measure, aggregation)) in query.measures.iter().enumerate() {
                let Some(column) = shape.other_columns[i] else {
                    continue;
                };
                let points = buckets
                    .iter()
                    .map(|(bucket, values)| point(*bucket, values, column, *aggregation))
                    .collect::<Result<_, _>>()?;
                series.push(MetricsSeries {
                    measure: *measure,
                    aggregation: *aggregation,
                    key: Vec::new(),
                    other: true,
                    points,
                });
            }
        }
        let percentile = query.measures.iter().any(|(m, _)| !m.additive());
        let (first, count) = shape.buckets;
        let end = first.saturating_add_micros(
            i64::try_from(count)
                .unwrap_or(i64::MAX)
                .saturating_mul(shape.step.seconds())
                .saturating_mul(1_000_000),
        );
        let table = match query.source {
            Source::Traces => TableId::TraceHeads,
            Source::Spans | Source::McpOps => TableId::SpanMetrics,
        };
        let meta = read.finish(read.now, consistency, state, &[table], false)?;
        Ok(MetricsResult {
            step: shape.step,
            tier: query
                .measures
                .first()
                .map_or(Tier::Rollup, |(m, _)| m.tier()),
            series,
            truncated: cut && percentile,
            partial_last_bucket: end > meta.as_of,
            meta,
        })
    }
}
