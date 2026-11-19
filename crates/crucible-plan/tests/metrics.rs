//! The metrics compiler and its caps (contract 8.8): at most 500 buckets per series, at most 20 dimension keys
//! with an `other` series for additive measures, at most 10,000 points, `auto` granularity, and
//! `query_too_broad` with a suggested granularity.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    Aggregation, Caller, DimensionId, Fill, Filter, FilterField, Granularity, Literal,
    METRICS_MAX_BUCKETS, METRICS_MAX_POINTS, METRICS_MAX_SERIES, Measure, MeasureId, MetricsError,
    MetricsQuery, Node, PlanError, Pred, QueryClass, RetentionClamp, Row, Source, Step, TextField,
    Value, metrics_keys, metrics_other, metrics_ranking, metrics_schema, metrics_series,
    metrics_shape,
};
use crucible_records::{CivilDate, ProjectId, ProjectScope, TimeWindow, Timestamp};
use crucible_schema::RetentionSettings;
use proptest::prelude::*;

const MINUTE: i64 = 60_000_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// 2026-11-17T09:30:00Z, a Tuesday.
const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

fn scope() -> ProjectScope {
    test_support::scope(ProjectId::from_bytes([9; 16])).unwrap()
}

fn keep() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), T0)
}

fn window(micros: i64) -> TimeWindow {
    TimeWindow::new(T0, T0.saturating_add_micros(micros)).unwrap()
}

fn query(measures: usize, dimensions: usize, granularity: Granularity) -> MetricsQuery {
    let all = [
        (MeasureId::Count, Aggregation::Sum),
        (MeasureId::TotalCost, Aggregation::Sum),
        (MeasureId::Duration, Aggregation::P95),
        (MeasureId::Ttft, Aggregation::P50),
    ];
    MetricsQuery {
        source: Source::Spans,
        measures: all[..measures].to_vec(),
        dimensions: [DimensionId::Environment, DimensionId::Model][..dimensions].to_vec(),
        filter: Filter::all(),
        granularity,
        fill: Fill::Zero,
        top_n: None,
    }
}

fn too_broad(limit: &'static str, value: u64, suggested: Option<Step>) -> MetricsError {
    MetricsError::TooBroad {
        limit,
        value,
        suggested_granularity: suggested,
    }
}

#[test]
fn buckets_are_counted_from_the_first_to_the_last_one_the_window_overlaps() {
    let at = |micros: i64| T0.saturating_add_micros(micros);
    let window = |from: i64, to: i64| TimeWindow::new(at(from), at(to)).unwrap();
    // 09:30:00 to 09:31:00 is one minute bucket; one microsecond more reaches the next.
    assert_eq!(Step::Minute.buckets(window(0, MINUTE)), (T0, 1));
    assert_eq!(Step::Minute.buckets(window(0, MINUTE + 1)), (T0, 2));
    // An unaligned hour overlaps two hour buckets, although it is one hour long.
    assert_eq!(Step::Hour.buckets(window(0, HOUR)), (at(-30 * MINUTE), 2));
    assert_eq!(Step::Hour.buckets(window(30 * MINUTE, 90 * MINUTE)).1, 1);
    assert_eq!(Step::FiveMinutes.bucket(at(4 * MINUTE)), T0);
    assert_eq!(Step::FifteenMinutes.bucket(at(14 * MINUTE)), T0);
    assert_eq!(
        Step::Day.bucket(T0),
        CivilDate::new(2026, 11, 17).unwrap().start()
    );
    // Weeks start on Monday, before the epoch too.
    let monday = CivilDate::new(2026, 11, 16).unwrap().start();
    assert_eq!(Step::Week.bucket(T0), monday);
    assert_eq!(Step::Week.bucket(monday), monday);
    assert_eq!(
        Step::Week.bucket(monday.saturating_add_micros(-1)),
        CivilDate::new(2026, 11, 9).unwrap().start()
    );
    assert_eq!(
        Step::Week.bucket(Timestamp::from_micros(-1)),
        CivilDate::new(1969, 12, 29).unwrap().start()
    );
    assert_eq!(Step::Week.buckets(window(-DAY, 6 * DAY)), (monday, 2));
}

#[test]
fn a_series_holds_at_most_500_buckets() {
    // From 09:30, 500 minutes end at 17:50: exactly 500 minute buckets.
    let fits = window(500 * MINUTE);
    let shape = metrics_shape(fits, &query(1, 0, Granularity::Step(Step::Minute))).unwrap();
    assert_eq!((shape.step, shape.buckets), (Step::Minute, (T0, 500)));
    // One microsecond more is 501 buckets: refused, with the granularity `auto` picks.
    let over = window(500 * MINUTE + 1);
    assert_eq!(
        metrics_shape(over, &query(1, 0, Granularity::Step(Step::Minute))),
        Err(too_broad(
            "max_buckets",
            METRICS_MAX_BUCKETS,
            Some(Step::FiveMinutes)
        ))
    );
    let auto = metrics_shape(over, &query(1, 0, Granularity::Auto)).unwrap();
    assert_eq!((auto.step, auto.buckets.1), (Step::FiveMinutes, 101));
    // A coarser explicit step than `auto` would pick is taken as asked.
    let day = metrics_shape(over, &query(1, 0, Granularity::Step(Step::Day))).unwrap();
    assert_eq!((day.step, day.buckets.1), (Step::Day, 1));
}

#[test]
fn a_query_returns_at_most_10_000_points() {
    // Two measures over 20 keys and the `other` series are 42 series: 238 buckets fit, 239 do not.
    let two = |granularity| query(2, 1, granularity);
    let fits = window(238 * MINUTE);
    let shape = metrics_shape(fits, &two(Granularity::Step(Step::Minute))).unwrap();
    assert_eq!((shape.buckets.1, shape.keys), (238, 20));
    const { assert!(42 * 238 <= METRICS_MAX_POINTS && 42 * 239 > METRICS_MAX_POINTS) };
    let over = window(239 * MINUTE);
    assert_eq!(
        metrics_shape(over, &two(Granularity::Step(Step::Minute))),
        Err(too_broad(
            "max_points",
            METRICS_MAX_POINTS,
            Some(Step::FiveMinutes)
        ))
    );
    assert_eq!(
        metrics_shape(over, &two(Granularity::Auto)).unwrap().step,
        Step::FiveMinutes
    );
    // Fewer keys leave room: 2 measures over 5 keys and `other` are 12 series.
    let few = MetricsQuery {
        top_n: Some(5),
        ..two(Granularity::Step(Step::Minute))
    };
    assert_eq!(metrics_shape(over, &few).unwrap().keys, 5);
    // Percentiles have no `other` series: one percentile over 20 keys is 20 series, and 500 buckets fit.
    let percentile = MetricsQuery {
        measures: vec![(MeasureId::Duration, Aggregation::P99)],
        ..query(1, 1, Granularity::Step(Step::Minute))
    };
    let shape = metrics_shape(window(500 * MINUTE), &percentile).unwrap();
    assert!(!shape.has_other());
    assert_eq!(shape.other_columns, [None]);
}

#[test]
fn no_granularity_fits_a_window_of_decades() {
    let decades = window(20 * 366 * DAY);
    for granularity in [Granularity::Auto, Granularity::Step(Step::Week)] {
        assert_eq!(
            metrics_shape(decades, &query(1, 0, granularity)),
            Err(too_broad("max_buckets", METRICS_MAX_BUCKETS, None))
        );
    }
}

#[test]
fn at_most_20_keys_are_returned_and_the_ranking_reads_one_more() {
    let run = |query: &MetricsQuery| {
        metrics_ranking(scope(), window(HOUR), &keep(), Caller::Session, query)
    };
    for (top_n, keys) in [(None, 20), (Some(20), 20), (Some(1), 1)] {
        let query = MetricsQuery {
            top_n,
            ..query(2, 2, Granularity::Auto)
        };
        assert_eq!(metrics_shape(window(HOUR), &query).unwrap().keys, keys);
        let plan = run(&query).unwrap().unwrap();
        let Node::Aggregate(ranking) = plan.root() else {
            panic!("an aggregate")
        };
        // One more than the keys returned: it tells that the rest went to `other`.
        assert_eq!(ranking.limit(), Some(keys + 1));
        assert_eq!(ranking.order().len(), 3);
        assert_eq!(plan.class(), QueryClass::Analytics);
    }
    for top_n in [0, METRICS_MAX_SERIES + 1] {
        let query = MetricsQuery {
            top_n: Some(top_n),
            ..query(1, 1, Granularity::Auto)
        };
        assert!(matches!(
            run(&query),
            Err(MetricsError::Invalid { field: "top_n", .. })
        ));
    }
    // Without dimensions there is nothing to rank and `top_n` means nothing.
    let flat = MetricsQuery {
        top_n: Some(999),
        ..query(1, 0, Granularity::Auto)
    };
    assert_eq!(metrics_shape(window(HOUR), &flat).unwrap().keys, 0);
    assert!(matches!(
        run(&flat),
        Err(MetricsError::Invalid {
            field: "dimensions",
            ..
        })
    ));
}

#[test]
fn the_series_take_the_ranked_keys_and_the_other_series_takes_the_rest() {
    let by = query(4, 2, Granularity::Auto);
    let shape = metrics_shape(window(HOUR), &by).unwrap();
    // Two sums and two histograms, in the order of the query; `other` holds the sums only.
    assert_eq!(shape.columns, [0, 1, 2, 3]);
    assert_eq!(shape.other_columns, [Some(0), Some(1), None, None]);
    assert!(shape.has_other());

    let rows = [
        Row::new(vec![
            Value::String("prod".to_owned()),
            Value::String("m1".to_owned()),
            Value::UInt(9),
        ]),
        Row::new(vec![
            Value::String("dev".to_owned()),
            Value::String(String::new()),
            Value::UInt(4),
        ]),
    ];
    let keys = metrics_keys(&by, &rows).unwrap();
    assert_eq!(
        keys[0],
        [
            Literal::String("prod".to_owned()),
            Literal::String("m1".to_owned())
        ]
    );
    let series = metrics_series(
        scope(),
        window(HOUR),
        &keep(),
        Caller::Session,
        &by,
        Some(&keys),
    );
    let series = series.unwrap().unwrap();
    let Node::TimeSeries(node) = series.root() else {
        panic!("a time series")
    };
    assert_eq!(node.step(), shape.step);
    assert_eq!(node.fill(), Fill::Zero);
    assert_eq!(node.aggregate().group().len(), 2);
    assert!(matches!(node.aggregate().pred(), Pred::Or(keys) if keys.len() == 2));
    let other = metrics_other(scope(), window(HOUR), &keep(), Caller::Session, &by, &keys);
    let other = other.unwrap().unwrap();
    let Node::TimeSeries(node) = other.root() else {
        panic!("a time series")
    };
    assert!(node.aggregate().group().is_empty());
    assert!(matches!(node.aggregate().pred(), Pred::Not(_)));
    assert!(
        node.aggregate()
            .measures()
            .iter()
            .all(|m| matches!(m, Measure::Sum(_)))
    );

    // The keys must match the query: a key per dimension, no more keys than `top_n`, none without dimensions.
    let run = |query: &MetricsQuery, keys: Option<&[Vec<Literal>]>| {
        metrics_series(scope(), window(HOUR), &keep(), Caller::Session, query, keys)
    };
    let invalid = |result| matches!(result, Err(MetricsError::Invalid { field: "keys", .. }));
    assert!(invalid(run(&by, None)));
    assert!(invalid(run(&by, Some(&[]))));
    assert!(invalid(run(
        &by,
        Some(&[vec![Literal::String("prod".to_owned())]])
    )));
    let one = MetricsQuery {
        top_n: Some(1),
        ..by.clone()
    };
    assert!(invalid(run(&one, Some(&keys))));
    assert!(invalid(run(&query(1, 0, Granularity::Auto), Some(&keys))));
    assert!(
        run(&query(1, 0, Granularity::Auto), None)
            .unwrap()
            .is_some()
    );
    // A value that is no dimension value gives no keys.
    assert_eq!(
        metrics_keys(&by, &[Row::new(vec![Value::Null, Value::UInt(1)])]),
        None
    );
}

#[test]
fn the_compiler_accepts_what_the_schema_lists_and_nothing_else() {
    let schema = metrics_schema();
    assert_eq!(
        schema.iter().map(|s| s.source).collect::<Vec<_>>(),
        [Source::Spans, Source::McpOps, Source::Traces]
    );
    let spans = &schema[0];
    assert_eq!(spans.source, Source::Spans);
    assert_eq!(spans.granularities, Step::ALL);
    assert_eq!(
        (
            spans.limits.max_buckets,
            spans.limits.max_series,
            spans.limits.max_points,
            spans.limits.max_measures,
            spans.limits.max_dimensions
        ),
        (500, 20, 10_000, 4, 2)
    );
    let run = |query: &MetricsQuery| {
        metrics_series(scope(), window(HOUR), &keep(), Caller::Session, query, None)
    };
    let base = query(1, 0, Granularity::Auto);
    for measure in &spans.measures {
        assert_eq!(MeasureId::parse(measure.as_str()), Some(*measure));
        for aggregation in Aggregation::ALL {
            assert_eq!(Aggregation::parse(aggregation.as_str()), Some(aggregation));
            let query = MetricsQuery {
                measures: vec![(*measure, aggregation)],
                ..base.clone()
            };
            let allowed = measure.aggregations().contains(&aggregation);
            assert_eq!(run(&query).is_ok(), allowed, "{measure:?} {aggregation:?}");
            // A percentile is a quantile of the histogram kernel; a sum is additive; a rate is neither.
            assert_eq!(
                aggregation.quantile_ppm().is_some(),
                !matches!(aggregation, Aggregation::Sum | Aggregation::Rate)
            );
            assert_eq!(
                allowed && measure.additive(),
                allowed && aggregation == Aggregation::Sum
            );
        }
    }
    for dimension in &spans.dimensions {
        assert_eq!(DimensionId::parse(dimension.as_str()), Some(*dimension));
        let query = MetricsQuery {
            dimensions: vec![*dimension],
            ..base.clone()
        };
        assert!(metrics_ranking(scope(), window(HOUR), &keep(), Caller::Session, &query).is_ok());
    }
    // The filter fields are those the rollup evaluates; any other is refused by name.
    for field in FilterField::ALL {
        let filter = match field {
            FilterField::Text(text) => Filter::Text(text, vec!["x".to_owned()]),
            FilterField::Level => Filter::Level(vec![crucible_records::Level::Error]),
            FilterField::ObservationType => {
                Filter::ObservationType(vec![crucible_records::ObservationType::Generation])
            }
            FilterField::Tags => Filter::Tags(vec!["x".to_owned()]),
            FilterField::Metadata => Filter::Metadata {
                key: "k".to_owned(),
                value: "v".to_owned(),
            },
            FilterField::Name => Filter::Name("x".to_owned()),
        };
        let query = MetricsQuery {
            filter,
            ..base.clone()
        };
        match run(&query) {
            Ok(_) => assert!(spans.filter_fields.contains(&field), "{field:?}"),
            Err(MetricsError::Plan(PlanError::UnsupportedFilter { field: named, .. })) => {
                assert!(!spans.filter_fields.contains(&field));
                assert_eq!(named, field.as_str());
            }
            Err(other) => panic!("{other:?}"),
        }
    }
    assert!(
        spans
            .filter_fields
            .contains(&FilterField::Text(TextField::Environment))
    );

    // What the registry does not allow.
    let invalid = |query: MetricsQuery, field: &str| {
        assert!(
            matches!(run(&query), Err(MetricsError::Invalid { field: named, .. }) if named == field),
            "{field}"
        );
    };
    invalid(
        MetricsQuery {
            source: Source::McpOps,
            ..base.clone()
        },
        "source",
    );
    // A measure of another source.
    invalid(
        MetricsQuery {
            source: Source::Traces,
            ..base.clone()
        },
        "measures",
    );
    invalid(
        MetricsQuery {
            measures: Vec::new(),
            ..base.clone()
        },
        "measures",
    );
    invalid(
        MetricsQuery {
            measures: vec![(MeasureId::Count, Aggregation::Sum); 5],
            ..base.clone()
        },
        "measures",
    );
    invalid(
        MetricsQuery {
            dimensions: vec![DimensionId::Level, DimensionId::Level],
            ..base.clone()
        },
        "dimensions",
    );
    invalid(
        MetricsQuery {
            dimensions: vec![DimensionId::Level, DimensionId::Model, DimensionId::Release],
            ..base
        },
        "dimensions",
    );
}

proptest! {
    /// Whatever the window, the measures and the dimensions: a compiled query stays inside every cap, `auto`
    /// picks the finest step that does, and a refusal suggests exactly that step.
    #[test]
    fn a_compiled_query_stays_inside_the_caps(
        micros in 1i64..(3 * 366 * DAY),
        offset in 0i64..(8 * DAY),
        measures in 1usize..=4,
        dimensions in 0usize..=2,
        top_n in prop::option::of(1u32..=20),
        step in prop::option::of(0usize..6),
    ) {
        let from = T0.saturating_add_micros(offset);
        let window = TimeWindow::new(from, from.saturating_add_micros(micros)).unwrap();
        let granularity = step.map_or(Granularity::Auto, |step| Granularity::Step(Step::ALL[step]));
        let query = MetricsQuery { top_n, ..query(measures, dimensions, granularity) };
        let inside = |step: Step| {
            let shape = metrics_shape(window, &MetricsQuery {
                granularity: Granularity::Step(step),
                ..query.clone()
            });
            shape.is_ok()
        };
        let finest = Step::ALL.into_iter().find(|step| inside(*step));
        match metrics_shape(window, &query) {
            Ok(shape) => {
                let (first, buckets) = shape.buckets;
                prop_assert_eq!((first, buckets), shape.step.buckets(window));
                prop_assert!(buckets <= METRICS_MAX_BUCKETS);
                let keys = u64::from(shape.keys);
                prop_assert!(keys <= u64::from(METRICS_MAX_SERIES));
                let series = measures as u64 * if dimensions == 0 { 1 } else { keys + u64::from(shape.has_other()) };
                prop_assert!(buckets * series <= METRICS_MAX_POINTS);
                if step.is_none() {
                    prop_assert_eq!(Some(shape.step), finest);
                }
                // The buckets cover the window and nothing beyond its first and last bucket.
                let end = first.saturating_add_micros(buckets as i64 * shape.step.seconds() * 1_000_000);
                prop_assert!(first <= window.from() && end >= window.to());
                prop_assert!(shape.step.bucket(window.from()) == first);
            }
            Err(MetricsError::TooBroad { suggested_granularity, .. }) => {
                prop_assert_eq!(suggested_granularity, finest);
                prop_assert!(step.is_none_or(|step| !inside(Step::ALL[step])));
            }
            Err(other) => prop_assert!(false, "{other:?}"),
        }
    }
}

#[test]
fn the_traces_source_counts_root_heads_in_the_core_tier() {
    let schema = metrics_schema();
    let traces = schema.iter().find(|s| s.source == Source::Traces).unwrap();
    assert_eq!(traces.measures, [MeasureId::Traces]);
    assert!(traces.dimensions.is_empty() && traces.filter_fields.is_empty());
    assert_eq!(traces.limits.max_dimensions, 0);
    let measure = MeasureId::Traces;
    assert_eq!(
        (measure.as_str(), measure.unit(), measure.additive()),
        ("traces", "count", true)
    );
    assert_eq!(measure.tier(), crucible_plan::Tier::Core);
    assert_eq!(MeasureId::parse("traces"), Some(measure));

    let query = MetricsQuery {
        source: Source::Traces,
        measures: vec![(MeasureId::Traces, Aggregation::Sum)],
        dimensions: Vec::new(),
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: None,
    };
    let plan = metrics_series(scope(), window(DAY), &keep(), Caller::Session, &query, None)
        .unwrap()
        .unwrap();
    assert_eq!(plan.class(), QueryClass::AnalyticsRaw);
    let Node::TimeSeries(series) = plan.root() else {
        panic!("a time series")
    };
    assert_eq!(series.time().name(), "start_time");
    assert_eq!(series.step(), Step::FiveMinutes);
    let aggregate = series.aggregate();
    assert_eq!(aggregate.table(), crucible_records::TableId::TraceHeads);
    assert_eq!(aggregate.measures(), [Measure::Count]);
    assert!(aggregate.group().is_empty());
    let root = Literal::Enum(crucible_records::HeadKind::Root.value());
    assert_eq!(
        aggregate.pred(),
        &Pred::eq(crucible_schema::TraceHeadsCol::HeadKind, root)
    );
    // An agent's read runs in the agent class, as every other.
    let agent = metrics_series(scope(), window(DAY), &keep(), Caller::Agent, &query, None);
    assert_eq!(agent.unwrap().unwrap().class(), QueryClass::Agent);

    // The source has no dimensions and no filter fields yet, and no ranking or `other` series.
    let refused = |query: MetricsQuery, field: &str| {
        let result = metrics_series(scope(), window(DAY), &keep(), Caller::Session, &query, None);
        assert!(
            matches!(result, Err(MetricsError::Invalid { field: named, .. }) if named == field),
            "{field}: {result:?}"
        );
    };
    refused(
        MetricsQuery {
            dimensions: vec![DimensionId::Level],
            ..query.clone()
        },
        "dimensions",
    );
    refused(
        MetricsQuery {
            filter: Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
            ..query.clone()
        },
        "filter",
    );
    refused(
        MetricsQuery {
            measures: vec![(MeasureId::Count, Aggregation::Sum)],
            ..query.clone()
        },
        "measures",
    );
    assert!(metrics_ranking(scope(), window(DAY), &keep(), Caller::Session, &query).is_err());
}
