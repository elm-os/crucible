//! The metrics query and the filter-values lookup (contract 8.8): the series from the span rollup with the
//! `other` series and quantiles, the trace count from the root heads, and the values of a filter field.

use crucible_plan::{
    Aggregation, DimensionId, Fill, Filter, FilterField, FilterValuesRequest, Granularity,
    MeasureId, MetricsQuery, Source, Step, TextField, Value,
};
use crucible_query::{
    FilterValuesQuery, MetricValue, MetricsRequest, MetricsResult, QueryServiceError,
};
use crucible_records::{HeadKind, Histogram, Level, Lsn, TableId, TimeWindow};

use crate::store::stored_row;
use crate::world::{NOW, World, ctx, project, run, span, trace_head_row};

const MINUTE: i64 = 60_000_000;

/// The hour that ends at [`NOW`], which is on a whole minute.
fn hour() -> TimeWindow {
    TimeWindow::new(NOW.saturating_add_micros(-60 * MINUTE), NOW).unwrap()
}

fn histogram(values: &[u64]) -> Value {
    let mut h = Histogram::new();
    for v in values {
        h.record(*v);
    }
    let (keys, counts): (Vec<Value>, Vec<Value>) = h
        .iter()
        .map(|(b, c)| (Value::UInt(u64::from(b.index())), Value::UInt(c)))
        .unzip();
    Value::Tuple2(Box::new(Value::Array(keys)), Box::new(Value::Array(counts)))
}

/// One partial of `span_metrics`.
fn partial(
    world: &World,
    minutes_ago: i64,
    environment: &str,
    level: Level,
    count: u64,
    durations: &[u64],
) {
    let minute = NOW.saturating_add_micros(-minutes_ago * MINUTE);
    let row = stored_row(
        TableId::SpanMetrics,
        &[
            ("project_id", Value::Uuid(project(1).uuid())),
            ("minute", Value::Time(minute)),
            ("environment", Value::String(environment.to_owned())),
            ("level", Value::Enum(level.value())),
            ("release", Value::String(format!("v{count}"))),
            ("count", Value::UInt(count)),
            (
                "error_count",
                Value::UInt(u64::from(level == Level::Error) * count),
            ),
            ("total_cost_nusd", Value::Int(count as i64 * 1_000)),
            ("dur_hist", histogram(durations)),
        ],
    );
    world.store.insert(TableId::SpanMetrics, row);
}

fn rollup() -> World {
    let world = World::new(10);
    partial(&world, 50, "prod", Level::Default, 5, &[100, 200]);
    partial(&world, 50, "dev", Level::Error, 2, &[4_000]);
    partial(&world, 10, "prod", Level::Default, 3, &[300]);
    partial(&world, 10, "staging", Level::Default, 1, &[7]);
    world
}

fn query(measures: Vec<(MeasureId, Aggregation)>, dimensions: Vec<DimensionId>) -> MetricsQuery {
    MetricsQuery {
        source: Source::Spans,
        measures,
        dimensions,
        filter: Filter::all(),
        granularity: Granularity::Step(Step::FifteenMinutes),
        fill: Fill::Zero,
        top_n: Some(1),
    }
}

fn metrics(world: &World, query: MetricsQuery) -> MetricsResult {
    run(world.service.metrics(
        &ctx(1),
        MetricsRequest {
            window: hour(),
            query,
        },
    ))
    .unwrap()
}

fn total(points: &[crucible_query::MetricsPoint]) -> u64 {
    points
        .iter()
        .map(|p| match p.value {
            Some(MetricValue::UInt(v)) => v,
            Some(MetricValue::Int(v)) => v.unsigned_abs(),
            None => 0,
        })
        .sum()
}

#[test]
fn the_top_key_comes_first_and_the_rest_is_one_other_series() {
    let world = rollup();
    let by_env = query(
        vec![
            (MeasureId::Count, Aggregation::Sum),
            (MeasureId::TotalCost, Aggregation::Sum),
            (MeasureId::Duration, Aggregation::P50),
        ],
        vec![DimensionId::Environment],
    );
    let result = metrics(&world, by_env);
    assert_eq!(result.step, Step::FifteenMinutes);
    let shape: Vec<(MeasureId, Vec<&str>, bool, u64)> = result
        .series
        .iter()
        .map(|s| {
            (
                s.measure,
                s.key.iter().map(|k| &**k).collect(),
                s.other,
                total(&s.points),
            )
        })
        .collect();
    // prod has 8 spans; dev and staging (3 spans, 3,000 nano-USD) form the other series. Every series has
    // the four buckets of the hour; the percentile of `prod` is the one of its histogram.
    assert_eq!(
        shape[..2],
        [
            (MeasureId::Count, vec!["prod"], false, 8),
            (MeasureId::TotalCost, vec!["prod"], false, 8_000),
        ]
    );
    assert_eq!(shape[2].0, MeasureId::Duration);
    assert_eq!(
        shape[3..],
        [
            (MeasureId::Count, vec![], true, 3),
            (MeasureId::TotalCost, vec![], true, 3_000),
        ]
    );
    assert!(result.series.iter().all(|s| s.points.len() == 4));
    // A percentile has no `other` series: the keys it dropped make the answer truncated.
    assert!(result.truncated);
    let p50: Vec<Option<MetricValue>> = result.series[2].points.iter().map(|p| p.value).collect();
    let mut expected = Histogram::new();
    expected.record(100);
    expected.record(200);
    assert_eq!(
        p50.iter().flatten().next(),
        Some(&MetricValue::UInt(expected.quantile(500_000).unwrap()))
    );
    assert!(
        p50.contains(&None),
        "an empty bucket of a percentile is null"
    );
    assert!(!result.partial_last_bucket);
    assert_eq!(result.meta.partial_reason, None);
}

#[test]
fn the_traces_source_counts_root_heads() {
    let world = World::new(10);
    let entry = |n: u64, minutes_ago: i64| {
        span(
            n,
            n * 10,
            0,
            NOW.saturating_add_micros(-minutes_ago * MINUTE),
        )
    };
    for (n, kind) in [
        (1, HeadKind::Root),
        (2, HeadKind::Root),
        (3, HeadKind::Provisional),
    ] {
        let row = trace_head_row(&entry(n, 20), kind, Lsn::new(n), NOW);
        world.store.insert(TableId::TraceHeads, row);
    }
    let traces = MetricsQuery {
        source: Source::Traces,
        measures: vec![(MeasureId::Traces, Aggregation::Sum)],
        dimensions: Vec::new(),
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: None,
    };
    let result = metrics(&world, traces);
    assert_eq!(result.tier, crucible_plan::Tier::Core);
    assert_eq!(result.series.len(), 1);
    assert_eq!(total(&result.series[0].points), 2);
    assert_eq!(
        result.series[0].points.len() as u64,
        result.step.buckets(hour()).1
    );
}

#[test]
fn filter_values_are_counted_and_cut() {
    let world = rollup();
    let lookup = |field, prefix: Option<&str>, limit| {
        let query = FilterValuesQuery {
            window: hour(),
            request: FilterValuesRequest {
                source: Source::Spans,
                field,
                prefix: prefix.map(str::to_owned),
                limit,
            },
        };
        run(world.service.filter_values(&ctx(1), query)).unwrap()
    };
    let environments = lookup(FilterField::Text(TextField::Environment), None, 2);
    let values: Vec<(&str, u64)> = environments.data.iter().map(|(v, c)| (&**v, *c)).collect();
    assert_eq!(values, [("prod", 8), ("dev", 2)]);
    assert!(environments.truncated);
    let levels = lookup(FilterField::Level, Some("err"), 5);
    let values: Vec<(&str, u64)> = levels.data.iter().map(|(v, c)| (&**v, *c)).collect();
    assert_eq!(values, [("error", 2)]);
    assert!(!levels.truncated);

    world.store.set_unavailable(true);
    let query = FilterValuesQuery {
        window: hour(),
        request: FilterValuesRequest {
            source: Source::Spans,
            field: FilterField::Level,
            prefix: None,
            limit: 5,
        },
    };
    let down = run(world.service.filter_values(&ctx(1), query));
    assert!(matches!(down, Err(QueryServiceError::Store(_))));
}
