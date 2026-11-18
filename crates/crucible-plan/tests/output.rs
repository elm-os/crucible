//! `QueryPlan::output`: the one definition of a plan's result shape (contract section 8.1).

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    Aggregate, Aggregation, Caller, DimensionId, FieldRange, Fill, Filter, Granularity, Literal,
    Measure, MeasureId, MetricsQuery, Node, OutputSchema, PayloadField, PayloadRangeRequest,
    PointLookup, Pred, QueryClass, QueryPlan, RetentionClamp, Scan, SearchField, SearchRequest,
    SemiJoin, SessionListRequest, Snippet, Source, SpanListRequest, Step, TextSearch, TimeSeries,
    TokenMode, TraceBounds, TraceListRequest, UserListRequest, child_counts, data_bounds,
    metrics_other, metrics_ranking, metrics_series, payload_range, search, search_spans,
    session_detail, session_list, span_children, span_list, spans_of_parents, trace_list,
    trace_page_stats, trace_roots, trace_spans, trace_time_bounds, user_detail, user_list,
    user_session_counts,
};
use crucible_records::{ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{
    Column, FactScoresCol, McpOpsCol, RetentionSettings, SessionRollupCol, SpanCoreCol,
    SpanMetricsCol, SpanPayloadCol, TraceHeadsCol, TraceStatsCol, Ty,
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
    // A zero-filled bucket holds 0 for the sum, the empty histogram, and null for the maximum.
    assert_eq!(series[1..5], shape(&output)[..4]);
    assert_eq!(
        series[5],
        (
            "max_minute".to_owned(),
            "Nullable(DateTime('UTC'))".to_owned()
        )
    );

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

#[test]
fn outputs_are_nullable_where_a_value_can_be_absent() {
    let measures = || {
        vec![
            Measure::Count,
            Measure::Uniq(McpOpsCol::ServerName.column_ref()),
            Measure::Uniq(McpOpsCol::ProgressCount.column_ref()),
            Measure::Sum(McpOpsCol::ProgressCount.column_ref()),
            Measure::Sum(McpOpsCol::DurationUs.column_ref()),
            Measure::Min(McpOpsCol::ServerName.column_ref()),
            Measure::Max(McpOpsCol::Status.column_ref()),
            Measure::Max(McpOpsCol::JsonrpcCode.column_ref()),
        ]
    };
    let aggregate = || {
        Aggregate::new(
            TableId::McpOps,
            Pred::all(),
            [McpOpsCol::Method],
            measures(),
        )
        .unwrap()
    };
    let plain = shape(&plan(Node::Aggregate(aggregate())).output());
    assert_eq!(
        plain[1..]
            .iter()
            .map(|(_, ty)| ty.as_str())
            .collect::<Vec<_>>(),
        [
            "UInt64",
            "UInt64",
            // The distinct count of a nullable column is null for a group of nulls.
            "Nullable(UInt64)",
            "Nullable(UInt64)",
            "UInt64",
            "String",
            plain[7].1.as_str(),
            "Nullable(Int32)",
        ]
    );
    assert!(plain[7].1.starts_with("Enum8("));

    let filled = TimeSeries::new(aggregate(), McpOpsCol::StartTime, Step::Hour, Fill::Zero);
    let filled = shape(&plan(Node::TimeSeries(filled.unwrap())).output());
    assert_eq!(
        filled[2..]
            .iter()
            .map(|(_, ty)| ty.as_str())
            .collect::<Vec<_>>(),
        [
            "UInt64",
            "Nullable(UInt64)",
            "Nullable(UInt64)",
            // A sum is 0 in an empty bucket, never null.
            "UInt64",
            "UInt64",
            "Nullable(String)",
            format!("Nullable({})", plain[7].1).as_str(),
            "Nullable(Int32)",
        ]
    );
    // The same series without fill has the types of the plain aggregate.
    let sparse = TimeSeries::new(aggregate(), McpOpsCol::StartTime, Step::Hour, Fill::None);
    assert_eq!(
        shape(&plan(Node::TimeSeries(sparse.unwrap())).output())[1..],
        plain
    );
}

#[test]
fn a_zero_filled_series_takes_a_bare_aggregate() {
    let aggregate = || {
        Aggregate::new(
            TableId::SpanMetrics,
            Pred::all(),
            [SpanMetricsCol::Environment],
            vec![Measure::Sum(SpanMetricsCol::Count.column_ref())],
        )
        .unwrap()
    };
    let order = vec![(
        crucible_plan::AggOutput::Measure(0),
        crucible_plan::Direction::Desc,
    )];
    let shaped = [
        aggregate().limited(3).unwrap(),
        aggregate().ordered(order.clone()).unwrap(),
        aggregate()
            .ordered(order)
            .unwrap()
            .after(vec![Literal::UInt(1)])
            .unwrap(),
        aggregate()
            .having(vec![crucible_plan::Having {
                measure: 0,
                op: crucible_plan::CmpOp::Gt,
                value: Literal::UInt(0),
            }])
            .unwrap(),
    ];
    for aggregate in shaped {
        let filled = TimeSeries::new(
            aggregate.clone(),
            SpanMetricsCol::Minute,
            Step::Hour,
            Fill::Zero,
        );
        assert_eq!(filled, Err(crucible_plan::PlanError::ZeroFill));
        assert!(TimeSeries::new(aggregate, SpanMetricsCol::Minute, Step::Hour, Fill::None).is_ok());
    }
}

#[test]
fn session_and_user_outputs() {
    let sessions = pairs(&[
        ("session_id", "String"),
        ("max_last_seen", DT64),
        ("min_first_seen", DT64),
        ("sum_trace_count", "UInt64"),
        ("sum_span_count", "UInt64"),
        ("sum_error_count", "UInt64"),
        ("sum_total_cost_nusd", "Int64"),
        ("sum_input_tokens", "UInt64"),
        ("sum_output_tokens", "UInt64"),
        ("sum_total_tokens", "UInt64"),
        ("set_user_ids", "Array(String)"),
    ]);
    let request = SessionListRequest {
        limit: 50,
        cursor: Some((window().from(), "s1".to_owned())),
    };
    let list = session_list(scope(), window(), &keep(), Caller::Session, request);
    let list = list.unwrap().unwrap();
    assert_eq!(shape(&list.output()), sessions);
    let Node::Aggregate(aggregate) = list.root() else {
        panic!("an aggregate")
    };
    assert_eq!(aggregate.limit(), Some(51));
    assert_eq!(aggregate.cursor().map(<[Literal]>::len), Some(2));
    let detail = session_detail(scope(), window(), &keep(), Caller::Session, "s1");
    assert_eq!(shape(&detail.unwrap().unwrap().output()), sessions);

    let mut users = sessions[..10].to_vec();
    users[0].0 = "user_id".to_owned();
    let request = UserListRequest {
        limit: 200,
        cursor: None,
    };
    let list = user_list(scope(), window(), &keep(), Caller::Session, request);
    assert_eq!(shape(&list.unwrap().unwrap().output()), users);
    let detail = user_detail(scope(), window(), &keep(), Caller::Session, "u1");
    assert_eq!(shape(&detail.unwrap().unwrap().output()), users);
    let counts = user_session_counts(
        scope(),
        window(),
        &keep(),
        Caller::Session,
        &["u1".to_owned(), "u2".to_owned()],
    );
    assert_eq!(
        shape(&counts.unwrap().unwrap().output()),
        pairs(&[("user_id", "String"), ("uniq_session_id", "UInt64")])
    );
    // A set merge is only for the bounded set partials.
    assert!(
        Aggregate::new(
            TableId::SessionRollup,
            Pred::all(),
            [SessionRollupCol::SessionId],
            vec![Measure::SetMerge(SessionRollupCol::TraceCount.column_ref())],
        )
        .is_err()
    );
}

#[test]
fn span_list_search_and_payload_outputs() {
    let request = |filter| SpanListRequest {
        filter,
        limit: 50,
        cursor: Some((window().from(), SpanId::from_bytes([4; 8]))),
        as_of: window().to(),
    };
    let list = span_list(
        scope(),
        window(),
        &keep(),
        Caller::Session,
        request(Filter::all()),
    );
    let list = list.unwrap().unwrap();
    assert_eq!(shape(&list.output()), table_shape::<SpanCoreCol>());
    assert!(matches!(list.root(), Node::Scan(_)));
    // A metadata condition is a semi-join into the payload table, with the same output.
    let metadata = Filter::Metadata {
        key: "k".to_owned(),
        value: "v".to_owned(),
    };
    let joined = span_list(
        scope(),
        window(),
        &keep(),
        Caller::Session,
        request(metadata),
    );
    let joined = joined.unwrap().unwrap();
    assert_eq!(joined.output(), list.output());
    assert_eq!(
        joined.root().tables(),
        [TableId::SpanCore, TableId::SpanPayload]
    );

    let request = SearchRequest {
        tokens: vec!["Rate".to_owned(), "LIMIT".to_owned()],
        mode: TokenMode::All,
        fields: vec![
            SearchField::Output,
            SearchField::Metadata,
            SearchField::Input,
        ],
        filter: Filter::all(),
        limit: 20,
    };
    let found = search(scope(), window(), &keep(), Caller::Session, &request);
    let found = found.unwrap().unwrap();
    assert_eq!(
        shape(&found.hits.output()),
        pairs(&[
            ("start_minute", "DateTime('UTC')"),
            ("trace_hash", "UInt32"),
            ("span_id", "FixedString(8)"),
            ("output_match", "UInt64"),
            ("output_snippet", "String"),
            ("input_match", "UInt64"),
            ("input_snippet", "String"),
        ])
    );
    let Node::TextSearch(text) = found.hits.root() else {
        panic!("a text search")
    };
    // The builder lowers the tokens, as the indexes lower the columns.
    assert_eq!(text.tokens(), ["rate", "limit"]);
    assert!(found.rest.is_all());
    let hit = (window().from(), 7, SpanId::from_bytes([4; 8]));
    let spans = search_spans(
        scope(),
        window(),
        &keep(),
        Caller::Session,
        &[hit, hit],
        &found.rest,
    );
    assert_eq!(
        shape(&spans.unwrap().unwrap().output()),
        table_shape::<SpanCoreCol>()
    );
    // More tokens than the position function takes.
    let many = TextSearch::new(
        TableId::SpanPayload,
        [SpanPayloadCol::Input],
        TokenMode::Any,
        vec!["t".to_owned(); 65],
        Pred::all(),
        1,
    )
    .unwrap();
    let snippet = Snippet { before: 1, len: 1 };
    assert!(many.with_snippets(snippet).is_err());

    for field in PayloadField::ALL {
        assert_eq!(PayloadField::parse(field.as_str()), Some(field));
        let request = PayloadRangeRequest {
            trace_id: TraceId::from_bytes([3; 16]),
            span_id: SpanId::from_bytes([4; 8]),
            start_time: window().from(),
            field,
            offset: 0,
            len: 65_536,
        };
        let range = payload_range(scope(), &keep(), Caller::Session, request);
        let range = range.unwrap().unwrap();
        assert_eq!(
            shape(&range.output()),
            vec![
                (field.as_str().to_owned(), "String".to_owned()),
                (format!("{}_bytes", field.as_str()), "UInt64".to_owned()),
            ]
        );
        assert_eq!(range.class(), QueryClass::Point);
    }
}

#[test]
fn metrics_outputs() {
    let query = |dimensions: Vec<DimensionId>, fill| MetricsQuery {
        source: Source::Spans,
        measures: vec![
            (MeasureId::Duration, Aggregation::P95),
            (MeasureId::TotalCost, Aggregation::Sum),
            (MeasureId::Duration, Aggregation::P50),
            (MeasureId::Count, Aggregation::Sum),
        ],
        dimensions,
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill,
        top_n: Some(5),
    };
    let histogram = "Tuple(Array(UInt16), Array(UInt64))";
    let flat = query(Vec::new(), Fill::Zero);
    let series = metrics_series(scope(), window(), &keep(), Caller::Session, &flat, None);
    assert_eq!(
        shape(&series.unwrap().unwrap().output()),
        pairs(&[
            ("bucket", "DateTime('UTC')"),
            ("histogram_dur_hist", histogram),
            ("sum_total_cost_nusd", "Int64"),
            ("sum_count", "UInt64"),
        ])
    );

    let by = query(vec![DimensionId::Model, DimensionId::Level], Fill::None);
    let ranking = metrics_ranking(scope(), window(), &keep(), Caller::Session, &by);
    let ranking = shape(&ranking.unwrap().unwrap().output());
    assert_eq!(
        ranking[0],
        ("model_response".to_owned(), "String".to_owned())
    );
    assert_eq!(ranking[1].0, "level");
    // The ranking measure is the first additive measure of the query.
    assert_eq!(
        ranking[2],
        ("sum_total_cost_nusd".to_owned(), "Int64".to_owned())
    );
    let keys = vec![vec![Literal::String("m".to_owned()), Literal::Enum(3)]];
    let series = metrics_series(
        scope(),
        window(),
        &keep(),
        Caller::Session,
        &by,
        Some(&keys),
    );
    let series = shape(&series.unwrap().unwrap().output());
    assert_eq!(series.len(), 6);
    assert_eq!(series[3].1, histogram);
    let other = metrics_other(scope(), window(), &keep(), Caller::Session, &by, &keys);
    assert_eq!(
        shape(&other.unwrap().unwrap().output()),
        pairs(&[
            ("bucket", "DateTime('UTC')"),
            ("sum_total_cost_nusd", "Int64"),
            ("sum_count", "UInt64"),
        ])
    );
}
