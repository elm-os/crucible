//! `QueryPlan::output`: the one definition of a plan's result shape (contract section 8.1).

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    Aggregate, Caller, FieldRange, Fill, Literal, Measure, Node, OutputSchema, PointLookup, Pred,
    QueryClass, QueryPlan, RetentionClamp, Scan, SemiJoin, Step, TextSearch, TimeSeries, TokenMode,
    TraceBounds, TraceListRequest, child_counts, data_bounds, span_children, spans_of_parents,
    trace_list, trace_page_stats, trace_roots, trace_spans, trace_time_bounds,
};
use crucible_records::{ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{
    Column, FactScoresCol, RetentionSettings, SpanCoreCol, SpanMetricsCol, SpanPayloadCol,
    TraceHeadsCol, TraceStatsCol, Ty,
};

fn scope() -> ProjectScope {
    test_support::scope(ProjectId::from_bytes([9; 16])).unwrap()
}

fn window() -> TimeWindow {
    let from = Timestamp::from_micros(1_794_900_000_000_000);
    TimeWindow::new(from, Timestamp::from_micros(1_794_907_800_000_000)).unwrap()
}

fn plan(root: Node) -> QueryPlan {
    crucible_plan::test_support::query_plan(scope(), window(), QueryClass::Point, root)
}

/// A project that keeps its data forever: the clamp leaves every window as it is.
fn keep() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), window().to())
}

/// The output as (label, type string) pairs.
fn shape(output: &OutputSchema) -> Vec<(String, String)> {
    output
        .columns()
        .iter()
        .map(|c| (c.name.clone(), c.ty.to_string()))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(name, ty)| ((*name).to_owned(), (*ty).to_owned()))
        .collect()
}

/// A scan's output is its columns in order, with `LowCardinality(String)` described as `String`.
fn table_shape<C: Column>() -> Vec<(String, String)> {
    C::ALL
        .iter()
        .map(|c| {
            let ty = c.column_ref().ty().to_string();
            let ty = ty.replace("LowCardinality(String)", "String");
            (c.name().to_owned(), ty)
        })
        .collect()
}

const DT64: &str = "DateTime64(6, 'UTC')";

#[test]
fn trace_bounds_output_is_the_key_then_the_merged_partials() {
    let trace_id = TraceId::from_bytes([3; 16]);
    let plan = trace_time_bounds(scope(), window(), &keep(), Caller::Session, trace_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        shape(&plan.output()),
        pairs(&[
            ("trace_id", "FixedString(16)"),
            ("min_min_start", DT64),
            ("max_max_end", DT64),
            ("sum_span_count", "UInt64"),
            ("max_has_root", "UInt8"),
            ("min_min_head_start", DT64),
        ])
    );
}

#[test]
fn scans_of_the_m1_builders_output_their_columns_in_table_order() {
    let (from, to) = (window().from(), window().to());
    let trace_id = TraceId::from_bytes([3; 16]);
    let spans = trace_spans(scope(), from, to, &keep(), Caller::Session, trace_id)
        .unwrap()
        .unwrap();
    assert_eq!(shape(&spans.output()), table_shape::<SpanCoreCol>());
    assert_eq!(spans.output().columns().len(), SpanCoreCol::ALL.len());

    let trace = TraceBounds {
        trace_id,
        min_start: from,
        max_end: to,
    };
    let children = span_children(
        scope(),
        trace,
        &keep(),
        Caller::Agent,
        SpanId::ZERO,
        None,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(children.output(), spans.output());

    let request = TraceListRequest {
        filter: Pred::all(),
        limit: 50,
        cursor: None,
        as_of: to,
        since: None,
    };
    let list = trace_list(scope(), window(), &keep(), Caller::Session, request)
        .unwrap()
        .unwrap();
    assert_eq!(shape(&list.output()), table_shape::<TraceHeadsCol>());
}

#[test]
fn page_stats_and_paged_detail_outputs() {
    let trace_id = TraceId::from_bytes([3; 16]);
    let stats = trace_page_stats(scope(), window(), &keep(), Caller::Session, &[trace_id])
        .unwrap()
        .unwrap();
    assert_eq!(
        shape(&stats.output()),
        pairs(&[
            ("trace_id", "FixedString(16)"),
            ("min_min_start", DT64),
            ("max_max_end", DT64),
            ("max_has_root", "UInt8"),
            ("min_min_head_start", DT64),
            ("sum_span_count", "UInt64"),
            ("sum_error_count", "UInt64"),
            ("sum_total_cost_nusd", "Int64"),
            ("sum_input_tokens", "UInt64"),
            ("sum_output_tokens", "UInt64"),
            ("sum_total_tokens", "UInt64"),
            ("pick_trace_name_pick", "String"),
            ("pick_user_id_pick", "String"),
            ("pick_session_id_pick", "String"),
        ])
    );
    assert_eq!(
        trace_page_stats(scope(), window(), &keep(), Caller::Session, &[]).map(|_| ()),
        Err(crucible_plan::PlanError::ListLength("trace_id"))
    );

    let trace = TraceBounds {
        trace_id,
        min_start: window().from(),
        max_end: window().to(),
    };
    let parents = [SpanId::from_bytes([1; 8]), SpanId::from_bytes([2; 8])];
    let roots = trace_roots(scope(), trace, &keep(), Caller::Session)
        .unwrap()
        .unwrap();
    assert_eq!(shape(&roots.output()), table_shape::<SpanCoreCol>());
    let children = spans_of_parents(scope(), trace, &keep(), Caller::Session, &parents, 10)
        .unwrap()
        .unwrap();
    assert_eq!(children.output(), roots.output());
    let counts = child_counts(scope(), trace, &keep(), Caller::Session, &parents)
        .unwrap()
        .unwrap();
    assert_eq!(
        shape(&counts.output()),
        pairs(&[("parent_span_id", "FixedString(8)"), ("count", "UInt64")])
    );
}

#[test]
fn data_bounds_output_is_the_first_and_last_minute() {
    let plans = data_bounds(scope(), window(), &keep(), Caller::Session).unwrap();
    let minutes = [
        ("min_minute", "DateTime('UTC')"),
        ("max_minute", "DateTime('UTC')"),
    ];
    assert_eq!(shape(&plans.spans.unwrap().output()), pairs(&minutes));
    let mcp = shape(&plans.mcp.unwrap().output());
    assert_eq!(mcp.len(), 3);
    assert_eq!(mcp[0].0, "side");
    assert!(mcp[0].1.starts_with("Enum8('"), "{}", mcp[0].1);
    assert_eq!(mcp[1..], pairs(&minutes));
}

#[test]
fn no_output_type_carries_a_storage_wrapper() {
    let every_measure = Aggregate::new(
        TableId::SpanMetrics,
        Pred::all(),
        [SpanMetricsCol::Minute, SpanMetricsCol::Environment],
        vec![
            Measure::Sum(SpanMetricsCol::Count.column_ref()),
            Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
            Measure::Max(SpanMetricsCol::Minute.column_ref()),
        ],
    )
    .unwrap();
    let output = plan(Node::Aggregate(every_measure.clone())).output();
    assert_eq!(
        shape(&output),
        pairs(&[
            ("minute", "DateTime('UTC')"),
            ("environment", "String"),
            ("sum_count", "UInt64"),
            ("histogram_dur_hist", "Tuple(Array(UInt16), Array(UInt64))"),
            ("max_minute", "DateTime('UTC')"),
        ])
    );

    let series = TimeSeries::new(
        every_measure,
        SpanMetricsCol::Minute,
        Step::Hour,
        Fill::Zero,
    );
    let series = shape(&plan(Node::TimeSeries(series.unwrap())).output());
    assert_eq!(
        series[0],
        ("bucket".to_owned(), "DateTime('UTC')".to_owned())
    );
    assert_eq!(series[1..], shape(&output));

    let picks = Aggregate::new(
        TableId::TraceStats,
        Pred::all(),
        [TraceStatsCol::TraceId],
        vec![
            Measure::Sum(TraceStatsCol::TotalCostNusd.column_ref()),
            Measure::PickMerge(TraceStatsCol::UserIdPick.column_ref()),
        ],
    )
    .unwrap();
    let picks = shape(&plan(Node::Aggregate(picks)).output());
    assert_eq!(
        picks[1..],
        pairs(&[
            ("sum_total_cost_nusd", "Int64"),
            ("pick_user_id_pick", "String")
        ])
    );

    let raw = Aggregate::new(
        TableId::FactScores,
        Pred::all(),
        [FactScoresCol::DataType],
        vec![
            Measure::Count,
            Measure::Uniq(FactScoresCol::Comment.column_ref()),
            Measure::Sum(FactScoresCol::ValueNum.column_ref()),
            Measure::LatestByVersion(FactScoresCol::ValueStr.column_ref()),
        ],
    )
    .unwrap();
    let raw = shape(&plan(Node::Aggregate(raw)).output());
    assert_eq!(
        raw[1..],
        pairs(&[
            ("count", "UInt64"),
            ("uniq_comment", "UInt64"),
            ("sum_value_num", "Float64"),
            ("latest_value_str", "String"),
        ])
    );
}

#[test]
fn output_is_defined_for_every_node_kind() {
    let trace_id = TraceId::from_bytes([3; 16]);
    let span_id = SpanId::from_bytes([4; 8]);
    let key = || {
        vec![
            Literal::Time(Timestamp::from_micros(1_794_900_000_000_000)),
            Literal::from(trace_id.trace_hash()),
            Literal::from(span_id),
        ]
    };
    let key_columns = [
        SpanPayloadCol::StartMinute,
        SpanPayloadCol::TraceHash,
        SpanPayloadCol::SpanId,
    ];
    let keys = pairs(&[
        ("start_minute", "DateTime('UTC')"),
        ("trace_hash", "UInt32"),
        ("span_id", "FixedString(8)"),
    ]);

    let lookup = PointLookup::new(
        TableId::SpanPayload,
        key_columns,
        vec![key()],
        [SpanPayloadCol::Input, SpanPayloadCol::TraceId],
    )
    .unwrap();
    assert_eq!(
        shape(&plan(Node::PointLookup(lookup)).output()),
        pairs(&[("input", "String"), ("trace_id", "FixedString(16)")])
    );

    let search = TextSearch::new(
        TableId::SpanPayload,
        [SpanPayloadCol::Input],
        TokenMode::All,
        vec!["refund".to_owned()],
        Pred::all(),
        20,
    )
    .unwrap();
    assert_eq!(shape(&plan(Node::TextSearch(search)).output()), keys);

    let range = FieldRange::new(
        TableId::SpanPayload,
        key_columns,
        key(),
        SpanPayloadCol::Input,
        0,
        4096,
    )
    .unwrap();
    assert_eq!(
        shape(&plan(Node::FieldRange(range)).output()),
        pairs(&[("input", "String")])
    );

    let heads = || {
        Scan::new(
            TableId::TraceHeads,
            [TraceHeadsCol::TraceId, TraceHeadsCol::Name],
            Pred::all(),
        )
        .unwrap()
    };
    let ids = Scan::new(TableId::SpanCore, [SpanCoreCol::TraceId], Pred::all()).unwrap();
    let join = SemiJoin::new(heads(), TraceHeadsCol::TraceId, Node::Scan(ids), 100).unwrap();
    assert_eq!(
        plan(Node::SemiJoin(join)).output(),
        plan(Node::Scan(heads())).output()
    );
    let nullable = Scan::new(
        TableId::SpanCore,
        [SpanCoreCol::CompletionStartTime, SpanCoreCol::Tags],
        Pred::all(),
    )
    .unwrap();
    let nullable = plan(Node::Scan(nullable)).output();
    assert_eq!(nullable.columns()[0].ty, Ty::Nullable(&Ty::DateTime64));
    assert_eq!(nullable.columns()[1].ty, Ty::Array(&Ty::String));
}
