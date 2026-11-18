//! The metrics query (contract 8.8) over the `spans` source: the compiler from a `MetricsQuery` to time series
//! over `span_metrics`, with its caps.
//!
//! A query without dimensions is one plan, [`metrics_series`]. A query with dimensions is read in two steps:
//! [`metrics_ranking`] returns the dimension keys with the largest values, one more than `top_n`; the caller
//! passes the first `top_n` of them to [`metrics_series`], and, when the ranking returned more, to
//! [`metrics_other`], which is the series of everything else for the additive measures. Percentile measures
//! have no `other` series: their remainder is dropped and the response says `truncated`.
//!
//! Buckets are aligned to UTC. The `timezone` of `MetricsQuery` is not compiled yet.

use crucible_records::{ProjectScope, TimeWindow};
use crucible_schema::{Column, SpanMetricsCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::filter::{Filter, FilterTarget};
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Fill, Measure, Node, Step, TimeSeries};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::{
    Aggregation, DimensionId, METRICS_MAX_BUCKETS, METRICS_MAX_DIMENSIONS, METRICS_MAX_MEASURES,
    METRICS_MAX_POINTS, METRICS_MAX_SERIES, MeasureId, Source,
};
use crate::value::Row;

/// The bucket width of a metrics query.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Granularity {
    /// The finest step that fits the caps.
    Auto,
    /// This step, or `query_too_broad` when it does not fit.
    Step(Step),
}

/// `MetricsQuery` v1, without its window (which every builder takes) and its timezone.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetricsQuery {
    /// The source. Only `spans` is compiled.
    pub source: Source,
    /// One to [`METRICS_MAX_MEASURES`] measures, each with an aggregation it allows.
    pub measures: Vec<(MeasureId, Aggregation)>,
    /// Up to [`METRICS_MAX_DIMENSIONS`] distinct dimensions.
    pub dimensions: Vec<DimensionId>,
    /// The filter; [`Filter::all`] for none.
    pub filter: Filter,
    /// The bucket width.
    pub granularity: Granularity,
    /// What an empty bucket holds.
    pub fill: Fill,
    /// The most dimension keys to return, 1 to [`METRICS_MAX_SERIES`]; `None` is the maximum. Ignored without
    /// dimensions.
    pub top_n: Option<u32>,
}

/// Why a metrics query was not compiled.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum MetricsError {
    /// The plan could not be built; an unsupported filter field is among the causes.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A part of the query the registry does not allow (`validation_failed`). `field` is the member of
    /// `MetricsQuery` to change.
    #[error("metrics query field {field}: {reason}")]
    Invalid {
        /// The member of the query.
        field: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The query exceeds a cap (`query_too_broad`): the limit that was exceeded, and the finest granularity
    /// that fits, if one does.
    #[error("the metrics query exceeds the limit {limit} of {value}")]
    TooBroad {
        /// The name of the limit: `max_buckets` or `max_points`.
        limit: &'static str,
        /// The value of the limit.
        value: u64,
        /// The finest granularity under which the query fits.
        suggested_granularity: Option<Step>,
    },
}

/// What a metrics query compiles to, apart from its plans.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetricsShape {
    /// The granularity used.
    pub step: Step,
    /// The first instant of the first bucket and the number of buckets per series.
    pub buckets: (crucible_records::Timestamp, u64),
    /// The most dimension keys returned: `top_n`, or 0 without dimensions.
    pub keys: u32,
    /// For each measure of the query, the index of the plan measure that carries it in [`metrics_series`]
    /// (after `bucket` and the dimensions). Percentiles of one measure share one histogram merge.
    pub columns: Vec<usize>,
    /// For each measure of the query, its index among the measures of [`metrics_other`], or `None` for a
    /// percentile, which has no `other` series.
    pub other_columns: Vec<Option<usize>>,
    measures: Vec<Measure>,
    other: Vec<Measure>,
    rank: Measure,
}

impl MetricsShape {
    /// Whether the query has an `other` series: it has dimensions and at least one additive measure.
    pub fn has_other(&self) -> bool {
        self.keys > 0 && !self.other.is_empty()
    }
}

const fn invalid(field: &'static str, reason: &'static str) -> MetricsError {
    MetricsError::Invalid { field, reason }
}

/// The points a query returns under `step`: buckets, and buckets times series, where a series is one measure of
/// one dimension key (or of the `other` series).
fn points(step: Step, window: TimeWindow, series: u64) -> (u64, u64) {
    let (_, buckets) = step.buckets(window);
    (buckets, buckets.saturating_mul(series))
}

/// Checks a query against the registry and the caps of contract 8.8 and picks its granularity. `auto` picks
/// the finest step that fits [`METRICS_MAX_BUCKETS`] and [`METRICS_MAX_POINTS`]; an explicit step that does not
/// fit is refused with the step `auto` would pick.
pub fn metrics_shape(window: TimeWindow, query: &MetricsQuery) -> Result<MetricsShape, MetricsError> {
    if query.source != Source::Spans {
        return Err(invalid("source", "only the spans source has metrics"));
    }
    if query.measures.is_empty() || query.measures.len() > METRICS_MAX_MEASURES {
        return Err(invalid("measures", "one to four measures are required"));
    }
    if query.dimensions.len() > METRICS_MAX_DIMENSIONS {
        return Err(invalid("dimensions", "at most two dimensions are allowed"));
    }
    if let [first, second] = query.dimensions.as_slice()
        && first == second
    {
        return Err(invalid("dimensions", "a dimension is listed twice"));
    }
    let keys = match (query.dimensions.is_empty(), query.top_n) {
        (true, _) => 0,
        (false, None) => METRICS_MAX_SERIES,
        (false, Some(n)) if (1..=METRICS_MAX_SERIES).contains(&n) => n,
        (false, Some(_)) => return Err(invalid("top_n", "top_n must be between 1 and 20")),
    };

    let (mut measures, mut other): (Vec<Measure>, Vec<Measure>) = (Vec::new(), Vec::new());
    let (mut columns, mut other_columns) = (Vec::new(), Vec::new());
    let place = |list: &mut Vec<Measure>, measure: Measure| match list.iter().position(|m| *m == measure) {
        Some(at) => at,
        None => {
            list.push(measure);
            list.len() - 1
        }
    };
    for (id, aggregation) in &query.measures {
        if !id.aggregations().contains(aggregation) {
            return Err(invalid("measures", "the measure does not allow the aggregation"));
        }
        let measure = if id.additive() {
            Measure::Sum(id.column())
        } else {
            Measure::HistogramMerge(id.column())
        };
        columns.push(place(&mut measures, measure));
        other_columns.push(id.additive().then(|| place(&mut other, measure)));
    }
    // Rank by the first additive measure, or by the span count when every measure is a percentile.
    let rank = other
        .first()
        .copied()
        .unwrap_or(Measure::Sum(SpanMetricsCol::Count.column_ref()));

    // One series per measure and dimension key; the `other` series counts as a key.
    let per_key = query.measures.len() as u64;
    let series = match keys {
        0 => per_key,
        keys => per_key * (u64::from(keys) + u64::from(!other.is_empty())),
    };
    let fits = |step: Step| {
        let (buckets, total) = points(step, window, series);
        if buckets > METRICS_MAX_BUCKETS {
            Err(("max_buckets", METRICS_MAX_BUCKETS))
        } else if total > METRICS_MAX_POINTS {
            Err(("max_points", METRICS_MAX_POINTS))
        } else {
            Ok(())
        }
    };
    let finest = Step::ALL.into_iter().find(|step| fits(*step).is_ok());
    let step = match query.granularity {
        Granularity::Auto => finest,
        Granularity::Step(step) => Some(step),
    };
    let too_broad = |(limit, value): (&'static str, u64)| MetricsError::TooBroad {
        limit,
        value,
        suggested_granularity: finest,
    };
    // No step fits: the widest one names the limit.
    let step = step.ok_or_else(|| too_broad(fits(Step::Week).err().unwrap_or(("max_points", METRICS_MAX_POINTS))))?;
    fits(step).map_err(too_broad)?;
    Ok(MetricsShape {
        step,
        buckets: step.buckets(window),
        keys,
        columns,
        other_columns,
        measures,
        other,
        rank,
    })
}

fn dimensions(query: &MetricsQuery) -> Vec<crucible_schema::ColumnRef> {
    query.dimensions.iter().map(|d| d.column()).collect()
}

/// The dimension keys of ranking rows, as [`metrics_series`] and [`metrics_other`] take them: the first
/// `dimensions` values of each row. `None` when a value is not a dimension value.
pub fn metrics_keys(query: &MetricsQuery, rows: &[Row]) -> Option<Vec<Vec<Literal>>> {
    let columns = dimensions(query);
    rows.iter()
        .map(|row| {
            let values = row.values().get(..columns.len())?;
            columns
                .iter()
                .zip(values)
                .map(|(column, value)| Literal::of(value, column.ty()))
                .collect()
        })
        .collect()
}

/// The rows whose dimensions are one of `keys`.
fn among(query: &MetricsQuery, keys: &[Vec<Literal>]) -> Result<Pred, MetricsError> {
    let columns = dimensions(query);
    if keys.is_empty() || keys.iter().any(|key| key.len() != columns.len()) {
        return Err(invalid("keys", "each key needs one value per dimension"));
    }
    Ok(match columns.as_slice() {
        [one] => Pred::In(*one, keys.iter().flatten().cloned().collect()),
        many => Pred::Or(
            keys.iter()
                .map(|key| {
                    Pred::And(
                        many.iter()
                            .zip(key)
                            .map(|(column, value)| Pred::eq(*column, value.clone()))
                            .collect(),
                    )
                })
                .collect(),
        ),
    })
}

fn filtered(query: &MetricsQuery, extra: Option<Pred>) -> Result<Pred, MetricsError> {
    let filter = query.filter.pred(FilterTarget::SpanMetrics)?;
    Ok(match extra {
        Some(extra) if query.filter.is_all() => extra,
        Some(extra) => Pred::And(vec![filter, extra]),
        None => filter,
    })
}

/// Step 1 of a query with dimensions: the dimension keys by descending value of the ranking measure (the
/// query's first additive measure, or the span count when it has none), ties in ascending key order, at most
/// `top_n + 1` rows. The outputs are the dimensions, then the ranking measure. More than `top_n` rows means the
/// response is truncated to the first `top_n` keys.
pub fn metrics_ranking(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    query: &MetricsQuery,
) -> Result<Option<QueryPlan>, MetricsError> {
    let shape = metrics_shape(window, query)?;
    if shape.keys == 0 {
        return Err(invalid("dimensions", "a ranking needs a dimension"));
    }
    let group = dimensions(query);
    let mut order = vec![(AggOutput::Measure(0), Direction::Desc)];
    order.extend((0..group.len()).map(|i| (AggOutput::Group(i), Direction::Asc)));
    let aggregate = Aggregate::new(
        SpanMetricsCol::TABLE,
        filtered(query, None)?,
        group,
        vec![shape.rank],
    )?
    .ordered(order)?
    .limited(shape.keys + 1)?;
    Ok(clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Analytics),
        Node::Aggregate(aggregate),
    )?)
}

fn series(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    aggregate: Aggregate,
    step: Step,
    fill: Fill,
) -> Result<Option<QueryPlan>, MetricsError> {
    let series = TimeSeries::new(aggregate, SpanMetricsCol::Minute, step, fill)?;
    Ok(clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Analytics),
        Node::TimeSeries(series),
    )?)
}

/// The series of a query: `bucket`, the dimensions, then the plan measures that [`MetricsShape::columns`]
/// maps the query's measures to (a sum per additive measure, one histogram merge per percentile measure, whose
/// quantiles are a post-operator). A query with dimensions takes the keys to return, from [`metrics_ranking`];
/// a query without takes `None`.
pub fn metrics_series(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    query: &MetricsQuery,
    keys: Option<&[Vec<Literal>]>,
) -> Result<Option<QueryPlan>, MetricsError> {
    let shape = metrics_shape(window, query)?;
    let among = match (shape.keys, keys) {
        (0, None) => None,
        (0, Some(_)) => return Err(invalid("keys", "a query without dimensions takes no keys")),
        (_, None) => return Err(invalid("keys", "a query with dimensions takes the ranked keys")),
        (most, Some(keys)) if keys.len() > most as usize => {
            return Err(invalid("keys", "more keys than top_n"));
        }
        (_, Some(keys)) => Some(among(query, keys)?),
    };
    let aggregate = Aggregate::new(
        SpanMetricsCol::TABLE,
        filtered(query, among)?,
        dimensions(query),
        shape.measures,
    )?;
    series(scope, window, clamp, caller, aggregate, shape.step, query.fill)
}

/// The `other` series of a query with dimensions: `bucket`, then the sums of the additive measures
/// ([`MetricsShape::other_columns`]) over every row whose dimensions are none of `keys`.
pub fn metrics_other(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    query: &MetricsQuery,
    keys: &[Vec<Literal>],
) -> Result<Option<QueryPlan>, MetricsError> {
    let shape = metrics_shape(window, query)?;
    if !shape.has_other() {
        return Err(invalid("measures", "the query has no other series"));
    }
    let rest = Pred::Not(Box::new(among(query, keys)?));
    let aggregate = Aggregate::new(
        SpanMetricsCol::TABLE,
        filtered(query, Some(rest))?,
        [] as [SpanMetricsCol; 0],
        shape.other,
    )?;
    series(scope, window, clamp, caller, aggregate, shape.step, query.fill)
}
