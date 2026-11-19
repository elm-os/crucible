//! The renderer (contract 8.3): snapshots of the SQL and the bound values of every plan of the first endpoints
//! and of every node kind, and the property that every rendered statement carries the project predicate and the
//! window predicate and contains only bound values.
//!
//! The same statements run against real servers in `crucible-conformance`.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support::scope;
use crucible_plan::test_support::{batch_probe, query_plan};
use crucible_plan::{
    AggOutput, Aggregate, Aggregation, ArrivalBounds, Caller, CmpOp, DeletePlan, DimensionId,
    Direction, FieldRange, Fill, Filter, Granularity, Having, Literal, Measure, MeasureId,
    MetricsQuery, Node, Order, PayloadField, PayloadRangeRequest, PlanError, PointLookup, Pred,
    QueryClass, QueryPlan, RetentionClamp, Scan, SearchField, SearchRequest, SemiJoin,
    SessionListRequest, Snippet, Source, SpanListRequest, Step, TextField, TextSearch, TimeSeries,
    TokenMode, TraceBounds, TraceListRequest, UserListRequest, UserSessionsRequest, child_counts,
    data_bounds, filtered_trace_list, metrics_other, metrics_ranking, metrics_series,
    payload_range, search, search_spans, session_detail, session_list, span_children, span_list,
    spans_of_parents, trace_list, trace_page_stats, trace_roots, trace_spans, trace_time_bounds,
    user_detail, user_list, user_session_counts, user_sessions,
};
use crucible_records::{
    CivilDate, Lsn, PartitionId, ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp,
    TraceId, Uuid, WalEpoch,
};
use crucible_records::{Level, ObservationType};
use crucible_schema::{
    Column, ColumnRef, FactTagsCol, McpDeploysCol, McpOpsCol, McpToolInventoryCol,
    McpToolRollupCol, RetentionSettings, SessionRollupCol, SpanCoreCol, SpanMetricsCol,
    SpanPayloadCol, TraceHeadsCol, TraceStatsCol, Ty, UserRollupCol, table,
};
use crucible_store_clickhouse::render::{self, Statement};
use proptest::prelude::*;

const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

fn project() -> ProjectScope {
    scope(ProjectId::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap()).unwrap()
}

fn window() -> TimeWindow {
    TimeWindow::new(
        T0,
        T0.saturating_add_micros(3_600_000_000)
            .saturating_add_micros(1),
    )
    .unwrap()
}

fn trace() -> TraceId {
    TraceId::from_bytes([0xa1; 16])
}

fn span(n: u8) -> SpanId {
    SpanId::from_bytes([n; 8])
}

fn plan(class: QueryClass, root: Node) -> QueryPlan {
    query_plan(project(), window(), class, root)
}

/// A clamp that expires nothing: the window a builder is given is the window its plan carries.
fn clamp() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), T0)
}

/// The plan of a builder whose window has not expired.
fn live(plan: Result<Option<QueryPlan>, PlanError>) -> QueryPlan {
    plan.unwrap().unwrap()
}

fn rendered(plan: &QueryPlan) -> String {
    render::plan(plan).unwrap().to_string()
}

// ---------------------------------------------------------------------------------------------------------
// Snapshots: the plans of the first endpoints.

#[test]
fn m1_trace_bounds() {
    let plan = live(trace_time_bounds(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        trace(),
    ));
    insta::assert_snapshot!(rendered(&plan));
}

#[test]
fn m1_trace_spans() {
    let plan = live(trace_spans(
        project(),
        T0,
        T0.saturating_add_micros(9_000),
        &clamp(),
        Caller::Agent,
        trace(),
    ));
    insta::assert_snapshot!(rendered(&plan));
}

#[test]
fn m1_trace_list() {
    let request = |cursor, since| TraceListRequest {
        filter: Pred::eq(TraceHeadsCol::Environment, "prod"),
        limit: 50,
        cursor,
        as_of: T0.saturating_add_micros(60_000_000),
        since,
    };
    let list = |request| {
        live(trace_list(
            project(),
            window(),
            &clamp(),
            Caller::Session,
            request,
        ))
    };
    let first = list(request(None, None));
    insta::assert_snapshot!("m1_trace_list_first_page", rendered(&first));
    let next = list(request(Some((T0, trace().trace_hash(), trace())), None));
    insta::assert_snapshot!("m1_trace_list_after_cursor", rendered(&next));
    let tail = list(request(None, Some(T0)));
    insta::assert_snapshot!("m1_trace_list_since", rendered(&tail));
}

#[test]
fn m1_span_children() {
    let bounds = TraceBounds {
        trace_id: trace(),
        min_start: T0,
        max_end: T0.saturating_add_micros(9_000),
    };
    let first = live(span_children(
        project(),
        bounds,
        &clamp(),
        Caller::Session,
        span(1),
        None,
        None,
    ));
    insta::assert_snapshot!("m1_span_children_first_page", rendered(&first));
    let next = live(span_children(
        project(),
        bounds,
        &clamp(),
        Caller::Session,
        span(1),
        Some((T0, span(2))),
        Some(100),
    ));
    insta::assert_snapshot!("m1_span_children_after_cursor", rendered(&next));
}

#[test]
fn m1_data_bounds() {
    let plans = data_bounds(project(), window(), &clamp(), Caller::Session).unwrap();
    insta::assert_snapshot!("m1_data_bounds_spans", rendered(&plans.spans.unwrap()));
    insta::assert_snapshot!("m1_data_bounds_mcp", rendered(&plans.mcp.unwrap()));
}

#[test]
fn m1_trace_page_stats() {
    let traces = [
        trace(),
        TraceId::from_bytes([0xa2; 16]),
        TraceId::from_bytes([0xa3; 16]),
    ];
    let plan = live(trace_page_stats(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        &traces,
    ));
    insta::assert_snapshot!(rendered(&plan));
}

#[test]
fn m1_paged_trace() {
    let bounds = TraceBounds {
        trace_id: trace(),
        min_start: T0,
        max_end: T0.saturating_add_micros(9_000),
    };
    let roots = live(trace_roots(project(), bounds, &clamp(), Caller::Session));
    insta::assert_snapshot!("m1_trace_roots", rendered(&roots));
    let parents = [span(1), span(2), span(3)];
    let children = live(spans_of_parents(
        project(),
        bounds,
        &clamp(),
        Caller::Session,
        &parents,
        500,
    ));
    insta::assert_snapshot!("m1_spans_of_parents", rendered(&children));
    let counts = live(child_counts(
        project(),
        bounds,
        &clamp(),
        Caller::Agent,
        &parents,
    ));
    insta::assert_snapshot!("m1_child_counts", rendered(&counts));
}

#[test]
fn an_expired_window_has_no_plan_to_render() {
    let expired = RetentionClamp::new(
        RetentionSettings {
            retention_days: Some(1),
            ..RetentionSettings::default()
        },
        T0.saturating_add_micros(10 * 86_400_000_000),
    );
    let plan = trace_time_bounds(project(), window(), &expired, Caller::Session, trace()).unwrap();
    assert_eq!(plan, None);
}

// ---------------------------------------------------------------------------------------------------------
// Snapshots: one per node kind, and the predicate forms.

#[test]
fn node_scan_with_every_predicate_form() {
    let pred = Pred::And(vec![
        Pred::Or(vec![
            Pred::eq(SpanCoreCol::Name, "it's a \\ name\twith\nescapes"),
            Pred::Not(Box::new(Pred::cmp(
                SpanCoreCol::TotalTokens,
                CmpOp::Le,
                7u64,
            ))),
        ]),
        Pred::is_in(
            SpanCoreCol::SpanId,
            vec![span(1).into(), span(2).into(), span(3).into()],
        ),
        Pred::StartsWith(SpanCoreCol::Environment.column_ref(), "pro".to_owned()),
        Pred::HasAny(
            SpanCoreCol::Tags.column_ref(),
            vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
        ),
        Pred::TextMatch("Chat".to_owned()),
        Pred::eq(SpanCoreCol::Level, Literal::Enum(3)),
        Pred::cmp(SpanCoreCol::TotalCostNusd, CmpOp::Ne, Literal::Int(-5)),
        Pred::cmp(
            SpanCoreCol::PriceModelId,
            CmpOp::Eq,
            Uuid::from_bytes([9; 16]),
        ),
        Pred::cmp(SpanCoreCol::CompletionStartTime, CmpOp::Gt, T0),
        Pred::Or(Vec::new()),
    ]);
    let scan = Scan::new(
        TableId::SpanCore,
        [SpanCoreCol::Name, SpanCoreCol::StartTime, SpanCoreCol::Tags],
        pred,
    )
    .unwrap()
    .ordered(Order::by(
        [SpanCoreCol::StartTime, SpanCoreCol::SpanId],
        Direction::Desc,
    ))
    .unwrap()
    .after(vec![T0.into(), span(4).into()])
    .unwrap()
    .arrived(ArrivalBounds {
        as_of: Some(T0),
        since: Some(T0.saturating_add_micros(-1)),
    })
    .unwrap()
    .limited(25)
    .unwrap();
    insta::assert_snapshot!(rendered(&plan(QueryClass::List, Node::Scan(scan))));
}

#[test]
fn node_scan_with_metadata() {
    let scan = Scan::new(
        TableId::SpanPayload,
        [SpanPayloadCol::Input],
        Pred::MetadataEq {
            key: "tenant".to_owned(),
            value: "acme".to_owned(),
        },
    )
    .unwrap();
    insta::assert_snapshot!(rendered(&plan(QueryClass::List, Node::Scan(scan))));
}

#[test]
fn node_point_lookup() {
    let key = |n: u8| -> Vec<Literal> {
        vec![
            T0.floor_minute().into(),
            trace().trace_hash().into(),
            span(n).into(),
        ]
    };
    let lookup = PointLookup::new(
        TableId::SpanPayload,
        [
            SpanPayloadCol::StartMinute,
            SpanPayloadCol::TraceHash,
            SpanPayloadCol::SpanId,
        ],
        vec![key(1), key(2), key(3)],
        [SpanPayloadCol::Input, SpanPayloadCol::Output],
    )
    .unwrap();
    insta::assert_snapshot!(rendered(&plan(
        QueryClass::Point,
        Node::PointLookup(lookup)
    )));
}

#[test]
fn node_aggregate() {
    let plain = Aggregate::new(
        TableId::SpanCore,
        Pred::eq(SpanCoreCol::Environment, "prod"),
        [SpanCoreCol::UserId, SpanCoreCol::ModelResponse],
        vec![
            Measure::Count,
            Measure::Sum(SpanCoreCol::TotalCostNusd.column_ref()),
            Measure::Uniq(SpanCoreCol::TraceId.column_ref()),
            Measure::Min(SpanCoreCol::StartTime.column_ref()),
            Measure::Max(SpanCoreCol::DurationUs.column_ref()),
        ],
    )
    .unwrap()
    .having(vec![
        Having {
            measure: 0,
            op: CmpOp::Gt,
            value: Literal::UInt(10),
        },
        Having {
            measure: 1,
            op: CmpOp::Lt,
            value: Literal::Int(-1),
        },
    ])
    .unwrap()
    .ordered(vec![
        (AggOutput::Measure(0), Direction::Desc),
        (AggOutput::Group(0), Direction::Asc),
    ])
    .unwrap()
    .limited(100)
    .unwrap();
    insta::assert_snapshot!(
        "node_aggregate_plain",
        rendered(&plan(QueryClass::AnalyticsRaw, Node::Aggregate(plain)))
    );

    let rollup = Aggregate::new(
        TableId::TraceStats,
        Pred::all(),
        [TraceStatsCol::TraceId],
        vec![
            Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
            Measure::PickMerge(TraceStatsCol::TraceNamePick.column_ref()),
        ],
    );
    assert!(rollup.is_err(), "a measure of another table is refused");
    let rollup = Aggregate::new(
        TableId::SpanMetrics,
        Pred::all(),
        [SpanMetricsCol::Environment],
        vec![
            Measure::Sum(SpanMetricsCol::Count.column_ref()),
            Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
        ],
    )
    .unwrap();
    insta::assert_snapshot!(
        "node_aggregate_rollup",
        rendered(&plan(QueryClass::Analytics, Node::Aggregate(rollup)))
    );

    let picks = Aggregate::new(
        TableId::TraceStats,
        Pred::all(),
        [] as [TraceStatsCol; 0],
        vec![Measure::PickMerge(
            TraceStatsCol::TraceNamePick.column_ref(),
        )],
    )
    .unwrap();
    insta::assert_snapshot!(
        "node_aggregate_ungrouped_pick",
        rendered(&plan(QueryClass::Point, Node::Aggregate(picks)))
    );

    // A table whose time column is an aggregated measure: the window applies after grouping.
    let inventory = Aggregate::new(
        TableId::McpToolInventory,
        Pred::all(),
        [
            McpToolInventoryCol::ServerName,
            McpToolInventoryCol::ToolName,
        ],
        vec![Measure::Max(McpToolInventoryCol::LastSeen.column_ref())],
    )
    .unwrap();
    insta::assert_snapshot!(
        "node_aggregate_inventory",
        rendered(&plan(QueryClass::Analytics, Node::Aggregate(inventory)))
    );

    let facts = Aggregate::new(
        TableId::FactTags,
        Pred::eq(FactTagsCol::Key, "quality"),
        [FactTagsCol::TraceId],
        vec![Measure::LatestByVersion(FactTagsCol::Op.column_ref())],
    )
    .unwrap();
    insta::assert_snapshot!(
        "node_aggregate_latest_by_version",
        rendered(&plan(QueryClass::List, Node::Aggregate(facts)))
    );
}

#[test]
fn node_semi_join() {
    let facts = Aggregate::new(
        TableId::FactTags,
        Pred::eq(FactTagsCol::Key, "quality"),
        [FactTagsCol::TraceId],
        vec![Measure::LatestByVersion(FactTagsCol::Op.column_ref())],
    )
    .unwrap()
    .having(vec![Having {
        measure: 0,
        op: CmpOp::Eq,
        value: Literal::Enum(1),
    }])
    .unwrap();
    let heads = Scan::new(
        TableId::TraceHeads,
        [TraceHeadsCol::TraceId, TraceHeadsCol::Name],
        Pred::all(),
    )
    .unwrap()
    .ordered(Order::key(TableId::TraceHeads, Direction::Desc))
    .unwrap()
    .limited(50)
    .unwrap();
    let join = SemiJoin::new(
        heads,
        TraceHeadsCol::TraceId,
        Node::Aggregate(facts),
        10_000,
    )
    .unwrap();
    insta::assert_snapshot!(rendered(&plan(QueryClass::List, Node::SemiJoin(join))));
}

#[test]
fn node_text_search() {
    for (name, mode) in [
        ("node_text_search_all", TokenMode::All),
        ("node_text_search_any", TokenMode::Any),
        ("node_text_search_phrase", TokenMode::Phrase),
    ] {
        let search = TextSearch::new(
            TableId::SpanPayload,
            [SpanPayloadCol::Input, SpanPayloadCol::Output],
            mode,
            vec!["rate".to_owned(), "limit".to_owned(), "exceeded".to_owned()],
            Pred::eq(SpanPayloadCol::UserId, "u1"),
            20,
        )
        .unwrap();
        insta::assert_snapshot!(
            name,
            rendered(&plan(QueryClass::Search, Node::TextSearch(search)))
        );
    }
}

#[test]
fn node_time_series() {
    let series = |step, fill| {
        let counts = Aggregate::new(
            TableId::SpanMetrics,
            Pred::all(),
            [SpanMetricsCol::Environment],
            vec![Measure::Sum(SpanMetricsCol::Count.column_ref())],
        )
        .unwrap();
        let series = TimeSeries::new(counts, SpanMetricsCol::Minute, step, fill).unwrap();
        plan(QueryClass::Analytics, Node::TimeSeries(series))
    };
    insta::assert_snapshot!(
        "node_time_series_five_minutes",
        rendered(&series(Step::FiveMinutes, Fill::None))
    );
    insta::assert_snapshot!(
        "node_time_series_week",
        rendered(&series(Step::Week, Fill::None))
    );
    // Zero fill: each dimension key is filled on its own, and a result without a matching row is dropped.
    insta::assert_snapshot!(
        "node_time_series_zero_fill_grouped",
        rendered(&series(Step::Hour, Fill::Zero))
    );
    // Every measure: 0 for the sums, null for the others, the empty histogram and the empty set.
    let ops = Aggregate::new(
        TableId::McpOps,
        Pred::all(),
        [] as [McpOpsCol; 0],
        vec![
            Measure::Count,
            Measure::Sum(McpOpsCol::ProgressCount.column_ref()),
            Measure::Sum(McpOpsCol::DurationUs.column_ref()),
            Measure::Uniq(McpOpsCol::UserId.column_ref()),
            Measure::Min(McpOpsCol::ServerName.column_ref()),
            Measure::Max(McpOpsCol::JsonrpcCode.column_ref()),
        ],
    )
    .unwrap();
    let ops = TimeSeries::new(ops, McpOpsCol::StartTime, Step::Day, Fill::Zero).unwrap();
    insta::assert_snapshot!(
        "node_time_series_zero_fill_ungrouped",
        rendered(&plan(QueryClass::AnalyticsRaw, Node::TimeSeries(ops)))
    );
}

// ---------------------------------------------------------------------------------------------------------
// Snapshots: the plans of the milestone 2 builders.

#[test]
fn m2_sessions() {
    let request = |cursor| SessionListRequest { limit: 50, cursor };
    let list = |request| {
        live(session_list(
            project(),
            window(),
            &clamp(),
            Caller::Session,
            request,
        ))
    };
    insta::assert_snapshot!("m2_session_list_first_page", rendered(&list(request(None))));
    let cursor = Some((T0, "session-9".to_owned()));
    insta::assert_snapshot!(
        "m2_session_list_after_cursor",
        rendered(&list(request(cursor)))
    );
    let detail = live(session_detail(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        "session-9",
    ));
    insta::assert_snapshot!("m2_session_detail", rendered(&detail));
}

#[test]
fn m2_lists() {
    let request = TraceListRequest {
        filter: Pred::all(),
        limit: 50,
        cursor: None,
        as_of: T0.saturating_add_micros(60_000_000),
        since: None,
    };
    let filtered = |filter: &Filter| {
        live(filtered_trace_list(
            project(),
            window(),
            &clamp(),
            Caller::Session,
            request.clone(),
            filter,
        ))
    };
    let typed = Filter::And(vec![
        Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
        Filter::ObservationType(vec![ObservationType::Tool, ObservationType::Generation]),
    ]);
    insta::assert_snapshot!("m2_trace_list_span_types", rendered(&filtered(&typed)));
    let metadata = Filter::And(vec![
        Filter::Level(vec![Level::Error]),
        Filter::Metadata {
            key: "tenant".to_owned(),
            value: "acme".to_owned(),
        },
    ]);
    insta::assert_snapshot!("m2_trace_list_metadata", rendered(&filtered(&metadata)));
    let sessions = live(user_sessions(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        UserSessionsRequest {
            user_id: "user-9".to_owned(),
            limit: 50,
            cursor: Some((T0, "session-9".to_owned())),
        },
    ));
    insta::assert_snapshot!("m2_user_sessions", rendered(&sessions));
}

#[test]
fn m2_users() {
    let request = UserListRequest {
        limit: 50,
        cursor: Some((T0, "user-9".to_owned())),
    };
    let list = live(user_list(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        request,
    ));
    insta::assert_snapshot!("m2_user_list_after_cursor", rendered(&list));
    let detail = live(user_detail(
        project(),
        window(),
        &clamp(),
        Caller::Agent,
        "user-9",
    ));
    insta::assert_snapshot!("m2_user_detail", rendered(&detail));
    let users = [
        "user-1".to_owned(),
        "user-2".to_owned(),
        "user-3".to_owned(),
    ];
    let counts = live(user_session_counts(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        &users,
    ));
    insta::assert_snapshot!("m2_user_session_counts", rendered(&counts));
}

/// A filter with every field a span list evaluates on `span_core`.
fn core_filter() -> Filter {
    Filter::And(vec![
        Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
        Filter::Text(TextField::Release, vec!["r2".to_owned(), "r1".to_owned()]),
        Filter::Text(TextField::Version, vec!["v1".to_owned()]),
        Filter::Level(vec![Level::Error, Level::Warning]),
        Filter::ObservationType(vec![ObservationType::Generation]),
        Filter::Not(Box::new(Filter::Text(
            TextField::UserId,
            vec!["u1".to_owned()],
        ))),
        Filter::Text(TextField::SessionId, vec!["s1".to_owned()]),
        Filter::Or(vec![
            Filter::Tags(vec!["beta".to_owned(), "alpha".to_owned()]),
            Filter::Name("Chat".to_owned()),
        ]),
    ])
}

fn metadata_filter() -> Filter {
    Filter::Metadata {
        key: "tenant".to_owned(),
        value: "acme".to_owned(),
    }
}

#[test]
fn m2_span_list() {
    let request = |filter, cursor| SpanListRequest {
        filter,
        limit: 50,
        cursor,
        as_of: T0.saturating_add_micros(60_000_000),
    };
    let list = |request| {
        live(span_list(
            project(),
            window(),
            &clamp(),
            Caller::Session,
            request,
        ))
    };
    insta::assert_snapshot!(
        "m2_span_list_first_page",
        rendered(&list(request(Filter::all(), None)))
    );
    let cursor = Some((T0.saturating_add_micros(5_000_001), span(7)));
    insta::assert_snapshot!(
        "m2_span_list_filtered_after_cursor",
        rendered(&list(request(core_filter(), cursor)))
    );
    let with_metadata = Filter::And(vec![
        metadata_filter(),
        Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
    ]);
    insta::assert_snapshot!(
        "m2_span_list_with_metadata",
        rendered(&list(request(with_metadata, None)))
    );
}

#[test]
fn m2_search() {
    let request = SearchRequest {
        tokens: vec!["Rate".to_owned(), "limit".to_owned(), "exceeded".to_owned()],
        mode: TokenMode::All,
        fields: vec![SearchField::Input, SearchField::Output],
        filter: Filter::And(vec![
            metadata_filter(),
            Filter::Text(TextField::UserId, vec!["u1".to_owned()]),
            Filter::Level(vec![Level::Error]),
        ]),
        limit: 20,
        cursor: None,
        as_of: T0.saturating_add_micros(3_600_000_000),
    };
    let found = search(project(), window(), &clamp(), Caller::Session, &request)
        .unwrap()
        .unwrap();
    insta::assert_snapshot!("m2_search_hits", rendered(&found.hits));
    // The next page, resuming after the key of the last hit; a phrase anchors its snippets on itself.
    let next = SearchRequest {
        mode: TokenMode::Phrase,
        cursor: Some((T0, trace().trace_hash(), span(1))),
        ..request.clone()
    };
    let next = search(project(), window(), &clamp(), Caller::Session, &next)
        .unwrap()
        .unwrap();
    insta::assert_snapshot!("m2_search_phrase_after_cursor", rendered(&next.hits));
    // Metadata entries, searched as they are written.
    let entries = SearchRequest {
        tokens: vec!["tenant=Acme".to_owned()],
        mode: TokenMode::Any,
        fields: vec![SearchField::Metadata],
        filter: Filter::all(),
        ..request.clone()
    };
    let entries = search(project(), window(), &clamp(), Caller::Session, &entries)
        .unwrap()
        .unwrap();
    insta::assert_snapshot!("m2_search_metadata", rendered(&entries.hits));
    let hits = [
        (T0, trace().trace_hash(), span(1)),
        (T0.saturating_add_micros(61_000_000), 7, span(2)),
        (T0.saturating_add_micros(1), trace().trace_hash(), span(3)),
    ];
    let spans = live(search_spans(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        &hits,
        &found.rest,
    ));
    insta::assert_snapshot!("m2_search_spans", rendered(&spans));
}

#[test]
fn m2_payload_range() {
    let request = PayloadRangeRequest {
        trace_id: trace(),
        span_id: span(1),
        start_time: T0.saturating_add_micros(1_500),
        field: PayloadField::Output,
        offset: 4_096,
        len: 65_536,
    };
    let range = live(payload_range(project(), &clamp(), Caller::Session, request));
    insta::assert_snapshot!(rendered(&range));
}

#[test]
fn m2_metrics() {
    let query = |dimensions: Vec<DimensionId>| MetricsQuery {
        source: Source::Spans,
        measures: vec![
            (MeasureId::Count, Aggregation::Sum),
            (MeasureId::Duration, Aggregation::P95),
            (MeasureId::TotalCost, Aggregation::Sum),
            (MeasureId::Duration, Aggregation::P50),
        ],
        dimensions,
        filter: Filter::Level(vec![Level::Error]),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: Some(5),
    };
    let flat = query(Vec::new());
    let series = metrics_series(project(), window(), &clamp(), Caller::Session, &flat, None);
    insta::assert_snapshot!(
        "m2_metrics_series_without_dimensions",
        rendered(&series.unwrap().unwrap())
    );
    let by = query(vec![DimensionId::Model, DimensionId::ObservationType]);
    let ranking = metrics_ranking(project(), window(), &clamp(), Caller::Session, &by);
    insta::assert_snapshot!("m2_metrics_ranking", rendered(&ranking.unwrap().unwrap()));
    let keys = vec![
        vec![Literal::String("model-a".to_owned()), Literal::Enum(1)],
        vec![Literal::String(String::new()), Literal::Enum(5)],
    ];
    let series = metrics_series(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        &by,
        Some(&keys),
    );
    insta::assert_snapshot!("m2_metrics_series", rendered(&series.unwrap().unwrap()));
    let other = metrics_other(project(), window(), &clamp(), Caller::Session, &by, &keys);
    insta::assert_snapshot!("m2_metrics_other", rendered(&other.unwrap().unwrap()));
}

#[test]
fn m2_facts() {
    use crucible_plan::{FactTable, FactsOf, fact_page, fact_rows};
    let traces = [trace(), TraceId::from_bytes([7; 16])];
    for table in FactTable::ALL {
        let name = table.table().name();
        let rows = fact_rows(
            project(),
            window(),
            &clamp(),
            Caller::Session,
            table,
            FactsOf::Traces(&traces),
        );
        insta::assert_snapshot!(format!("m2_fact_rows_{name}"), rendered(&live(rows)));
        let page = fact_page(
            project(),
            window(),
            &clamp(),
            Caller::Session,
            table,
            &traces,
        );
        insta::assert_snapshot!(format!("m2_fact_page_{name}"), rendered(&live(page)));
    }
    let of_span = FactsOf::Span(trace(), span(3));
    let rows = fact_rows(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        FactTable::Tags,
        of_span,
    );
    insta::assert_snapshot!("m2_fact_rows_of_a_span", rendered(&live(rows)));
    let of_session = FactsOf::Session("session-9");
    let rows = fact_rows(
        project(),
        window(),
        &clamp(),
        Caller::Session,
        FactTable::Flags,
        of_session,
    );
    insta::assert_snapshot!("m2_fact_rows_of_a_session", rendered(&live(rows)));
}

#[test]
fn m2_filter_values() {
    use crucible_plan::{FilterField, FilterValuesRequest, filter_values};
    let request = |field, prefix: Option<&str>| FilterValuesRequest {
        source: Source::Spans,
        field,
        prefix: prefix.map(str::to_owned),
        limit: 20,
    };
    let release = request(FilterField::Text(TextField::Release), Some("v1."));
    let plan = filter_values(project(), window(), &clamp(), Caller::Session, &release);
    insta::assert_snapshot!("m2_filter_values_with_a_prefix", rendered(&live(plan)));
    let level = request(FilterField::Level, Some("err"));
    let plan = filter_values(project(), window(), &clamp(), Caller::Session, &level);
    insta::assert_snapshot!("m2_filter_values_of_an_enum", rendered(&live(plan)));
}

#[test]
fn m2_metrics_traces() {
    let query = MetricsQuery {
        source: Source::Traces,
        measures: vec![(MeasureId::Traces, Aggregation::Sum)],
        dimensions: Vec::new(),
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: None,
    };
    let series = metrics_series(project(), window(), &clamp(), Caller::Session, &query, None);
    insta::assert_snapshot!("m2_metrics_traces", rendered(&series.unwrap().unwrap()));
}

#[test]
fn m2_export() {
    use crucible_plan::{
        ExportSelection, FactTable, export_facts, export_mcp_content, export_mcp_ops,
        export_payloads, export_spans, export_ties,
    };
    let traces = [trace(), TraceId::from_bytes([7; 16])];
    let narrowed = ExportSelection {
        trace_ids: &traces,
        user_id: Some("user-1"),
        session_id: Some("session-1"),
    };
    let after = (T0, 42, T0.saturating_add_micros(5), span(3));
    let first = export_spans(
        project(),
        window(),
        &clamp(),
        ExportSelection::default(),
        T0,
        None,
        1_000,
    );
    insta::assert_snapshot!("m2_export_spans_first_page", rendered(&live(first)));
    let next = export_spans(
        project(),
        window(),
        &clamp(),
        narrowed,
        T0,
        Some(after),
        1_000,
    );
    insta::assert_snapshot!("m2_export_spans_after_position", rendered(&live(next)));
    let ties = export_ties(project(), window(), &clamp(), narrowed, T0, after);
    insta::assert_snapshot!("m2_export_ties", rendered(&live(ties)));
    let spans = [
        (T0, trace(), span(1)),
        (T0.saturating_add_micros(90_000_000), traces[1], span(2)),
    ];
    let payloads = export_payloads(project(), &clamp(), &spans);
    insta::assert_snapshot!("m2_export_payloads", rendered(&live(payloads)));
    let ops = export_mcp_ops(project(), &clamp(), &spans);
    insta::assert_snapshot!("m2_export_mcp_ops", rendered(&live(ops)));
    let content = export_mcp_content(project(), &clamp(), &spans);
    insta::assert_snapshot!("m2_export_mcp_content", rendered(&live(content)));
    for table in FactTable::ALL {
        let name = table.table().name();
        let facts = export_facts(project(), window(), &clamp(), table, &traces);
        insta::assert_snapshot!(format!("m2_export_facts_{name}"), rendered(&live(facts)));
    }
}

#[test]
fn node_text_search_with_snippets() {
    let search = TextSearch::new(
        TableId::SpanPayload,
        [
            SpanPayloadCol::Input,
            SpanPayloadCol::MetadataKv,
            SpanPayloadCol::Output,
        ],
        TokenMode::Any,
        vec!["rate".to_owned(), "limit".to_owned(), "exceeded".to_owned()],
        Pred::all(),
        20,
    )
    .unwrap()
    .with_snippets(Snippet {
        before: 40,
        len: 160,
    })
    .unwrap();
    insta::assert_snapshot!(rendered(&plan(
        QueryClass::Search,
        Node::TextSearch(search)
    )));
}

#[test]
fn node_field_range() {
    let range = FieldRange::new(
        TableId::SpanPayload,
        [
            SpanPayloadCol::StartMinute,
            SpanPayloadCol::TraceHash,
            SpanPayloadCol::SpanId,
        ],
        vec![
            T0.floor_minute().into(),
            trace().trace_hash().into(),
            span(1).into(),
        ],
        SpanPayloadCol::Output,
        4_096,
        65_536,
    )
    .unwrap();
    insta::assert_snapshot!(rendered(&plan(QueryClass::Point, Node::FieldRange(range))));
}

// ---------------------------------------------------------------------------------------------------------
// Snapshots: the probe, the deletes and the insert statement.

fn epoch() -> WalEpoch {
    WalEpoch::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1c").unwrap()
}

#[test]
fn probe_forms() {
    let day = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
    let plain = batch_probe(TableId::SpanCore, day, epoch(), Lsn::new(1_048_577)).unwrap();
    insta::assert_snapshot!("probe_batch_seq", render::probe(&plain).to_string());
    let month = PartitionId::month(2026, 11).unwrap();
    let marker = batch_probe(TableId::SpanMetrics, month, epoch(), Lsn::new(1_048_577)).unwrap();
    insta::assert_snapshot!("probe_marker", render::probe(&marker).to_string());
    let all = batch_probe(
        TableId::McpDeploys,
        PartitionId::UNPARTITIONED,
        epoch(),
        Lsn::new(7),
    )
    .unwrap();
    insta::assert_snapshot!(
        "probe_marker_unpartitioned",
        render::probe(&all).to_string()
    );
}

#[test]
fn delete_forms() {
    let day = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
    let cutoff = CivilDate::new(2026, 11, 17).unwrap().start();
    let project = DeletePlan::project(
        project(),
        TableId::SpanCore,
        day,
        Pred::cmp(SpanCoreCol::StartTime, CmpOp::Lt, cutoff),
    )
    .unwrap();
    insta::assert_snapshot!(
        "delete_project",
        render::delete(&project).unwrap().to_string()
    );
    let purge = DeletePlan::marker_purge(
        TableId::SpanMetrics,
        PartitionId::month(2026, 11).unwrap(),
        T0,
        Pred::Not(Box::new(Pred::is_in(
            SpanMetricsCol::MarkerBatchSeq,
            vec![Literal::UInt(5), Literal::UInt(9)],
        ))),
    )
    .unwrap();
    insta::assert_snapshot!(
        "delete_marker_purge",
        render::delete(&purge).unwrap().to_string()
    );
}

#[test]
fn insert_statement() {
    insta::assert_snapshot!(render::insert(TableId::TraceStats));
}

// ---------------------------------------------------------------------------------------------------------
// The property: the project predicate, the window predicate and only bound values.

/// A deterministic value source: one seed gives one assignment of values to a plan shape.
struct Values(u64);

impl Values {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    fn time(&mut self) -> Timestamp {
        Timestamp::from_micros(self.below(4_000_000_000_000_000) as i64)
    }

    /// A string drawn from characters that would break out of a literal if one were ever formatted into SQL.
    fn string(&mut self) -> String {
        const ALPHABET: [&str; 16] = [
            "a",
            "Z",
            "0",
            " ",
            "'",
            "\"",
            "\\",
            "`",
            ";",
            "--",
            "{p0:String}",
            "}",
            "\n",
            "\t",
            "\u{e9}",
            ") OR 1=1",
        ];
        let len = 1 + self.below(6);
        (0..len)
            .map(|_| ALPHABET[self.below(16) as usize])
            .collect()
    }

    fn uuid(&mut self) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&self.bytes(16));
        bytes[0] |= 1;
        Uuid::from_bytes(bytes)
    }

    fn scope(&mut self) -> ProjectScope {
        scope(ProjectId::from_uuid(self.uuid())).unwrap()
    }

    fn window(&mut self) -> TimeWindow {
        let from = self.time();
        let to = from.saturating_add_micros(1 + self.below(10_000_000_000_000) as i64);
        TimeWindow::new(from, to).unwrap()
    }

    fn limit(&mut self) -> u32 {
        1 + self.below(1_000) as u32
    }

    /// A literal that fits a column of type `ty`.
    fn literal(&mut self, ty: &Ty) -> Literal {
        match ty.scalar() {
            Ty::Uuid => Literal::Uuid(self.uuid()),
            Ty::FixedString(n) => Literal::Bytes(self.bytes(usize::from(*n))),
            Ty::String | Ty::LowCardinalityString => Literal::String(self.string()),
            Ty::UInt8 => Literal::UInt(self.below(1 << 8)),
            Ty::UInt16 => Literal::UInt(self.below(1 << 16)),
            Ty::UInt32 => Literal::UInt(self.below(1 << 32)),
            Ty::UInt64 => Literal::UInt(self.next()),
            Ty::Int32 => Literal::Int(i64::from(self.next() as i32)),
            Ty::Int64 => Literal::Int(self.next() as i64),
            Ty::Float32 | Ty::Float64 => Literal::Float(self.next() as f64 / 7.0 - 1e9),
            Ty::DateTime64 => Literal::Time(self.time()),
            Ty::DateTime => Literal::Time(self.time().floor_minute()),
            Ty::Date => Literal::Date(CivilDate::from_day_number(self.below(40_000) as i64)),
            Ty::Enum8(kind) => {
                let variants = kind.variants();
                Literal::Enum(variants[self.below(variants.len() as u64) as usize].1)
            }
            other => panic!("no literal for {other}"),
        }
    }
}

/// The columns of `span_core` the generated predicates compare: one per literal type.
const COMPARED: [SpanCoreCol; 12] = [
    SpanCoreCol::TraceHash,
    SpanCoreCol::StartTime,
    SpanCoreCol::StartMinute,
    SpanCoreCol::SpanId,
    SpanCoreCol::TraceId,
    SpanCoreCol::Name,
    SpanCoreCol::Environment,
    SpanCoreCol::TotalTokens,
    SpanCoreCol::TotalCostNusd,
    SpanCoreCol::Level,
    SpanCoreCol::PriceModelId,
    SpanCoreCol::CompletionStartTime,
];

const OPS: [CmpOp; 6] = [
    CmpOp::Eq,
    CmpOp::Ne,
    CmpOp::Lt,
    CmpOp::Le,
    CmpOp::Gt,
    CmpOp::Ge,
];

/// The shape of a predicate over `span_core`: everything but the values.
#[derive(Clone, Debug)]
enum PredShape {
    And(Vec<PredShape>),
    Or(Vec<PredShape>),
    Not(Box<PredShape>),
    Cmp(usize, usize),
    In(usize, usize),
    StartsWith(bool),
    HasAny(usize),
    TextMatch,
}

fn pred_shape() -> impl Strategy<Value = PredShape> {
    let leaf = prop_oneof![
        (0..COMPARED.len(), 0..OPS.len()).prop_map(|(c, o)| PredShape::Cmp(c, o)),
        (0..COMPARED.len(), 1usize..10).prop_map(|(c, n)| PredShape::In(c, n)),
        any::<bool>().prop_map(PredShape::StartsWith),
        (1usize..6).prop_map(PredShape::HasAny),
        Just(PredShape::TextMatch),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(PredShape::And),
            prop::collection::vec(inner.clone(), 0..3).prop_map(PredShape::Or),
            inner.prop_map(|p| PredShape::Not(Box::new(p))),
        ]
    })
}

fn pred(shape: &PredShape, v: &mut Values) -> Pred {
    match shape {
        PredShape::And(operands) => Pred::And(operands.iter().map(|p| pred(p, v)).collect()),
        PredShape::Or(operands) => Pred::Or(operands.iter().map(|p| pred(p, v)).collect()),
        PredShape::Not(operand) => Pred::Not(Box::new(pred(operand, v))),
        PredShape::Cmp(column, op) => {
            let column = COMPARED[*column].column_ref();
            Pred::Cmp(column, OPS[*op], v.literal(column.ty()))
        }
        PredShape::In(column, len) => {
            let column = COMPARED[*column].column_ref();
            Pred::In(column, (0..*len).map(|_| v.literal(column.ty())).collect())
        }
        PredShape::StartsWith(name) => {
            let column = if *name {
                SpanCoreCol::Name
            } else {
                SpanCoreCol::Environment
            };
            Pred::StartsWith(column.column_ref(), v.string())
        }
        PredShape::HasAny(len) => Pred::HasAny(
            SpanCoreCol::Tags.column_ref(),
            (0..*len).map(|_| Literal::String(v.string())).collect(),
        ),
        PredShape::TextMatch => Pred::TextMatch(v.string()),
    }
}

/// The shape of a plan: its node kinds, tables, clauses and list arities.
#[derive(Clone, Debug)]
enum Shape {
    Scan {
        pred: PredShape,
        order: Option<bool>,
        cursor: bool,
        limit: bool,
        as_of: bool,
        since: bool,
    },
    Metadata,
    PointLookup {
        keys: usize,
    },
    AggregateSpans {
        pred: PredShape,
        grouped: bool,
        having: bool,
        order: bool,
        cursor: bool,
        limit: bool,
    },
    AggregateRollup {
        table: usize,
        grouped: bool,
    },
    SemiJoin {
        right_scan: bool,
        pred: PredShape,
    },
    TextSearch {
        tokens: usize,
        mode: usize,
        both_fields: bool,
        snippets: bool,
        paged: bool,
    },
    TimeSeries {
        step: usize,
        grouped: bool,
        zero: bool,
    },
    FieldRange {
        total: bool,
    },
    TraceList {
        cursor: bool,
        since: bool,
    },
    /// The session list (`users` unset) or the user list, or with `detail` one session or user.
    Rollup {
        users: bool,
        detail: bool,
        cursor: bool,
    },
    SessionCounts {
        users: usize,
    },
    SpanList {
        filtered: bool,
        metadata: bool,
        cursor: bool,
    },
    Search {
        tokens: usize,
        metadata: bool,
        filtered: bool,
        cursor: bool,
    },
    SearchSpans {
        hits: usize,
        filtered: bool,
    },
    Payload,
    /// A metrics plan: the ranking (0), the series (1) or the `other` series (2).
    Metrics {
        dimensions: usize,
        plan: usize,
        keys: usize,
        zero: bool,
        filtered: bool,
    },
}

fn shape() -> impl Strategy<Value = Shape> {
    let flag = any::<bool>;
    prop_oneof![
        (
            pred_shape(),
            prop::option::of(flag()),
            flag(),
            flag(),
            flag(),
            flag()
        )
            .prop_map(|(pred, order, cursor, limit, as_of, since)| Shape::Scan {
                pred,
                order,
                cursor,
                limit,
                as_of,
                since
            }),
        Just(Shape::Metadata),
        (1usize..10).prop_map(|keys| Shape::PointLookup { keys }),
        (pred_shape(), flag(), flag(), flag(), flag(), flag()).prop_map(
            |(pred, grouped, having, order, cursor, limit)| Shape::AggregateSpans {
                pred,
                grouped,
                having,
                order,
                cursor,
                limit
            }
        ),
        (0usize..7, flag()).prop_map(|(table, grouped)| Shape::AggregateRollup { table, grouped }),
        (flag(), pred_shape()).prop_map(|(right_scan, pred)| Shape::SemiJoin { right_scan, pred }),
        (1usize..7, 0usize..3, flag(), flag(), flag()).prop_map(
            |(tokens, mode, both_fields, snippets, paged)| Shape::TextSearch {
                tokens,
                mode,
                both_fields,
                snippets,
                paged,
            }
        ),
        (0usize..6, flag(), flag()).prop_map(|(step, grouped, zero)| Shape::TimeSeries {
            step,
            grouped,
            zero
        }),
        flag().prop_map(|total| Shape::FieldRange { total }),
        (flag(), flag()).prop_map(|(cursor, since)| Shape::TraceList { cursor, since }),
        (flag(), flag(), flag()).prop_map(|(users, detail, cursor)| Shape::Rollup {
            users,
            detail,
            cursor
        }),
        (1usize..6).prop_map(|users| Shape::SessionCounts { users }),
        (flag(), flag(), flag()).prop_map(|(filtered, metadata, cursor)| Shape::SpanList {
            filtered,
            metadata,
            cursor
        }),
        (1usize..5, flag(), flag(), flag()).prop_map(|(tokens, metadata, filtered, cursor)| {
            Shape::Search {
                tokens,
                metadata,
                filtered,
                cursor,
            }
        }),
        (1usize..6, flag()).prop_map(|(hits, filtered)| Shape::SearchSpans { hits, filtered }),
        Just(Shape::Payload),
        (0usize..3, 0usize..3, 1usize..4, flag(), flag()).prop_map(
            |(dimensions, plan, keys, zero, filtered)| Shape::Metrics {
                dimensions,
                plan,
                keys,
                zero,
                filtered
            }
        ),
    ]
}

/// A rollup aggregate: its table, a key column to group by and its measures.
fn rollup(index: usize) -> (TableId, ColumnRef, Vec<Measure>) {
    match index {
        0 => (
            TableId::SpanMetrics,
            SpanMetricsCol::Environment.column_ref(),
            vec![
                Measure::Sum(SpanMetricsCol::Count.column_ref()),
                Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
                Measure::Min(SpanMetricsCol::Minute.column_ref()),
            ],
        ),
        1 => (
            TableId::TraceStats,
            TraceStatsCol::TraceId.column_ref(),
            vec![
                Measure::Min(TraceStatsCol::MinStart.column_ref()),
                Measure::Sum(TraceStatsCol::SpanCount.column_ref()),
                Measure::PickMerge(TraceStatsCol::TraceNamePick.column_ref()),
            ],
        ),
        2 => (
            TableId::SessionRollup,
            SessionRollupCol::SessionId.column_ref(),
            vec![
                Measure::Max(SessionRollupCol::LastSeen.column_ref()),
                Measure::SetMerge(SessionRollupCol::UserIds.column_ref()),
            ],
        ),
        3 => (
            TableId::UserRollup,
            UserRollupCol::UserId.column_ref(),
            vec![Measure::Sum(UserRollupCol::SpanCount.column_ref())],
        ),
        4 => (
            TableId::McpToolRollup,
            McpToolRollupCol::Side.column_ref(),
            vec![Measure::Sum(McpToolRollupCol::Count.column_ref())],
        ),
        5 => (
            TableId::McpToolInventory,
            McpToolInventoryCol::ServerName.column_ref(),
            vec![Measure::Max(McpToolInventoryCol::LastSeen.column_ref())],
        ),
        _ => (
            TableId::McpDeploys,
            McpDeploysCol::ServerName.column_ref(),
            vec![Measure::Min(McpDeploysCol::FirstSeen.column_ref())],
        ),
    }
}

/// A filter over fields of `span_core` and, when asked, a metadata condition: its shape is fixed, its values
/// are drawn.
fn filter(v: &mut Values, filtered: bool, metadata: bool) -> Filter {
    let mut conditions = Vec::new();
    if filtered {
        // Distinct by construction (the second extends the first): the plan keeps a filter's values as a set, so
        // two equal draws would shorten the list and change the shape, not just the values.
        let first = v.string();
        let second = format!("{first}{}", v.string());
        conditions.push(Filter::Text(TextField::Environment, vec![first, second]));
        conditions.push(Filter::Not(Box::new(Filter::Level(vec![Level::Error]))));
        conditions.push(Filter::Or(vec![
            Filter::Tags(vec![v.string()]),
            Filter::Name(v.string()),
        ]));
    }
    if metadata {
        conditions.push(Filter::Metadata {
            key: v.string(),
            value: v.string(),
        });
    }
    Filter::And(conditions)
}

/// The plan of a shape under one assignment of values.
fn build(shape: &Shape, seed: u64) -> QueryPlan {
    let v = &mut Values(seed);
    let (scope, window) = (v.scope(), v.window());
    let plan = |class, root| query_plan(scope, window, class, root);
    match shape {
        Shape::Scan {
            pred: p,
            order,
            cursor,
            limit,
            as_of,
            since,
        } => {
            let mut scan = Scan::new(
                TableId::SpanCore,
                [SpanCoreCol::Name, SpanCoreCol::SpanId],
                pred(p, v),
            )
            .unwrap();
            if let Some(descending) = order {
                let direction = if *descending {
                    Direction::Desc
                } else {
                    Direction::Asc
                };
                let by = [SpanCoreCol::StartTime, SpanCoreCol::SpanId];
                scan = scan.ordered(Order::by(by, direction)).unwrap();
                if *cursor {
                    let after = by.map(|c| v.literal(c.column_ref().ty())).to_vec();
                    scan = scan.after(after).unwrap();
                }
            }
            if *limit {
                scan = scan.limited(v.limit()).unwrap();
            }
            scan = scan
                .arrived(ArrivalBounds {
                    as_of: as_of.then(|| v.time()),
                    since: since.then(|| v.time()),
                })
                .unwrap();
            plan(QueryClass::List, Node::Scan(scan))
        }
        Shape::Metadata => {
            let scan = Scan::new(
                TableId::SpanPayload,
                [SpanPayloadCol::Input],
                Pred::MetadataEq {
                    key: v.string(),
                    value: v.string(),
                },
            )
            .unwrap();
            plan(QueryClass::List, Node::Scan(scan))
        }
        Shape::PointLookup { keys } => {
            let columns = [SpanCoreCol::StartMinute, SpanCoreCol::SpanId];
            let keys = (0..*keys)
                .map(|_| columns.map(|c| v.literal(c.column_ref().ty())).to_vec())
                .collect();
            let lookup =
                PointLookup::new(TableId::SpanCore, columns, keys, [SpanCoreCol::Name]).unwrap();
            plan(QueryClass::Point, Node::PointLookup(lookup))
        }
        Shape::AggregateSpans {
            pred: p,
            grouped,
            having,
            order,
            cursor,
            limit,
        } => {
            let group: Vec<ColumnRef> = grouped
                .then(|| SpanCoreCol::UserId.column_ref())
                .into_iter()
                .collect();
            let mut aggregate = Aggregate::new(
                TableId::SpanCore,
                pred(p, v),
                group,
                vec![
                    Measure::Count,
                    Measure::Max(SpanCoreCol::StartTime.column_ref()),
                ],
            )
            .unwrap();
            if *having {
                aggregate = aggregate
                    .having(vec![
                        Having {
                            measure: 0,
                            op: CmpOp::Ge,
                            value: Literal::UInt(v.next()),
                        },
                        Having {
                            measure: 1,
                            op: CmpOp::Lt,
                            value: Literal::Time(v.time()),
                        },
                    ])
                    .unwrap();
            }
            if *order {
                aggregate = aggregate
                    .ordered(vec![
                        (AggOutput::Measure(0), Direction::Desc),
                        (AggOutput::Measure(1), Direction::Desc),
                    ])
                    .unwrap();
                if *cursor {
                    aggregate = aggregate
                        .after(vec![Literal::UInt(v.next()), Literal::Time(v.time())])
                        .unwrap();
                }
            }
            if *limit {
                aggregate = aggregate.limited(v.limit()).unwrap();
            }
            plan(QueryClass::AnalyticsRaw, Node::Aggregate(aggregate))
        }
        Shape::AggregateRollup { table, grouped } => {
            let (table, key, measures) = rollup(*table);
            let group: Vec<ColumnRef> = grouped.then_some(key).into_iter().collect();
            let aggregate = Aggregate::new(table, Pred::all(), group, measures).unwrap();
            plan(QueryClass::Analytics, Node::Aggregate(aggregate))
        }
        Shape::SemiJoin {
            right_scan,
            pred: p,
        } => {
            let (left, on, right) = if *right_scan {
                let ids = Scan::new(TableId::SpanCore, [SpanCoreCol::TraceId], pred(p, v)).unwrap();
                let heads =
                    Scan::new(TableId::TraceHeads, [TraceHeadsCol::Name], Pred::all()).unwrap();
                (heads, TraceHeadsCol::TraceId.column_ref(), Node::Scan(ids))
            } else {
                let facts = Aggregate::new(
                    TableId::FactTags,
                    Pred::eq(FactTagsCol::Key, Literal::String(v.string())),
                    [FactTagsCol::TraceId],
                    vec![Measure::LatestByVersion(FactTagsCol::Version.column_ref())],
                )
                .unwrap();
                let spans = Scan::new(TableId::SpanCore, [SpanCoreCol::Name], pred(p, v)).unwrap();
                (
                    spans,
                    SpanCoreCol::TraceId.column_ref(),
                    Node::Aggregate(facts),
                )
            };
            let join = SemiJoin::new(left, on, right, v.limit()).unwrap();
            plan(QueryClass::List, Node::SemiJoin(join))
        }
        Shape::TextSearch {
            tokens,
            mode,
            both_fields,
            snippets,
            paged,
        } => {
            let mut fields = vec![SpanPayloadCol::Input];
            if *both_fields {
                fields.push(SpanPayloadCol::Output);
            }
            let mode = [TokenMode::All, TokenMode::Any, TokenMode::Phrase][*mode];
            let tokens = (0..*tokens).map(|_| v.string()).collect();
            let mut search = TextSearch::new(
                TableId::SpanPayload,
                fields,
                mode,
                tokens,
                Pred::all(),
                v.limit(),
            )
            .unwrap();
            if *snippets {
                let snippet = Snippet {
                    before: v.below(1_000) as u32,
                    len: 1 + v.below(1_000) as u32,
                };
                search = search.with_snippets(snippet).unwrap();
            }
            if *paged {
                let mut id = [0u8; 8];
                id.copy_from_slice(&v.bytes(8));
                let after = vec![
                    Literal::Time(v.time().floor_minute()),
                    Literal::UInt(u64::from(v.next() as u32)),
                    Literal::Bytes(id.to_vec()),
                ];
                search = search
                    .ordered(Direction::Desc)
                    .after(after)
                    .unwrap()
                    .arrived(ArrivalBounds {
                        as_of: Some(v.time()),
                        since: None,
                    })
                    .unwrap();
            }
            plan(QueryClass::Search, Node::TextSearch(search))
        }
        Shape::TimeSeries {
            step,
            grouped,
            zero,
        } => {
            let group: Vec<ColumnRef> = grouped
                .then(|| SpanMetricsCol::Release.column_ref())
                .into_iter()
                .collect();
            let aggregate = Aggregate::new(
                TableId::SpanMetrics,
                Pred::eq(SpanMetricsCol::Environment, Literal::String(v.string())),
                group,
                vec![
                    Measure::Sum(SpanMetricsCol::Count.column_ref()),
                    Measure::Max(SpanMetricsCol::Minute.column_ref()),
                    Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
                ],
            )
            .unwrap();
            let step = [
                Step::Minute,
                Step::FiveMinutes,
                Step::FifteenMinutes,
                Step::Hour,
                Step::Day,
                Step::Week,
            ][*step];
            let fill = if *zero { Fill::Zero } else { Fill::None };
            let series = TimeSeries::new(aggregate, SpanMetricsCol::Minute, step, fill).unwrap();
            plan(QueryClass::Analytics, Node::TimeSeries(series))
        }
        Shape::FieldRange { total } => {
            let columns = [SpanPayloadCol::StartMinute, SpanPayloadCol::SpanId];
            let key = columns.map(|c| v.literal(c.column_ref().ty())).to_vec();
            let mut range = FieldRange::new(
                TableId::SpanPayload,
                columns,
                key,
                SpanPayloadCol::Input,
                v.below(1 << 40),
                1 + v.below(1 << 20),
            )
            .unwrap();
            if *total {
                range = range.with_total();
            }
            plan(QueryClass::Point, Node::FieldRange(range))
        }
        Shape::Rollup {
            users,
            detail,
            cursor,
        } => {
            let id = v.string();
            let cursor = cursor.then(|| (v.time(), v.string()));
            let limit = 1 + v.below(200) as u32;
            live(match (users, detail) {
                (false, false) => {
                    let request = SessionListRequest { limit, cursor };
                    session_list(scope, window, &clamp(), Caller::Session, request)
                }
                (false, true) => session_detail(scope, window, &clamp(), Caller::Session, &id),
                (true, false) => {
                    let request = UserListRequest { limit, cursor };
                    user_list(scope, window, &clamp(), Caller::Session, request)
                }
                (true, true) => user_detail(scope, window, &clamp(), Caller::Session, &id),
            })
        }
        Shape::SessionCounts { users } => {
            let users: Vec<String> = (0..*users).map(|_| v.string()).collect();
            live(user_session_counts(
                scope,
                window,
                &clamp(),
                Caller::Agent,
                &users,
            ))
        }
        Shape::SpanList {
            filtered,
            metadata,
            cursor,
        } => {
            let mut id = [0u8; 8];
            id.copy_from_slice(&v.bytes(8));
            let request = SpanListRequest {
                filter: filter(v, *filtered, *metadata),
                limit: 1 + v.below(200) as u32,
                cursor: cursor.then(|| (v.time(), SpanId::from_bytes(id))),
                as_of: v.time(),
            };
            live(span_list(scope, window, &clamp(), Caller::Session, request))
        }
        Shape::Search {
            tokens,
            metadata,
            filtered,
            cursor,
        } => {
            // Metadata entries and text tokens are searched by separate plans.
            let fields = if *metadata {
                vec![SearchField::Metadata]
            } else {
                vec![SearchField::Output, SearchField::Input]
            };
            let mut id = [0u8; 8];
            id.copy_from_slice(&v.bytes(8));
            let request = SearchRequest {
                tokens: (0..*tokens).map(|_| v.string()).collect(),
                mode: TokenMode::All,
                fields,
                filter: filter(v, *filtered, *metadata),
                limit: 1 + v.below(200) as u32,
                cursor: cursor.then(|| {
                    (
                        v.time().floor_minute(),
                        v.next() as u32,
                        SpanId::from_bytes(id),
                    )
                }),
                as_of: v.time(),
            };
            search(scope, window, &clamp(), Caller::Session, &request)
                .unwrap()
                .unwrap()
                .hits
        }
        Shape::SearchSpans { hits, filtered } => {
            let hits: Vec<(Timestamp, u32, SpanId)> = (0..*hits)
                .map(|_| {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&v.bytes(8));
                    // Distinct values, so the lists keep their arity under every assignment.
                    (v.time(), v.next() as u32, SpanId::from_bytes(id))
                })
                .collect();
            let rest = filter(v, *filtered, false);
            live(search_spans(
                scope,
                window,
                &clamp(),
                Caller::Session,
                &hits,
                &rest,
            ))
        }
        Shape::Payload => {
            let mut trace = [0u8; 16];
            trace.copy_from_slice(&v.bytes(16));
            let mut id = [0u8; 8];
            id.copy_from_slice(&v.bytes(8));
            let request = PayloadRangeRequest {
                trace_id: TraceId::from_bytes(trace),
                span_id: SpanId::from_bytes(id),
                start_time: window.from(),
                field: PayloadField::Input,
                offset: v.below(1 << 40),
                len: 1 + v.below(1 << 20),
            };
            live(payload_range(scope, &clamp(), Caller::Session, request))
        }
        Shape::Metrics {
            dimensions,
            plan,
            keys,
            zero,
            filtered,
        } => {
            let all = [DimensionId::Environment, DimensionId::Level];
            let filter = if *filtered {
                Filter::Text(TextField::Release, vec![v.string()])
            } else {
                Filter::all()
            };
            let query = MetricsQuery {
                source: Source::Spans,
                measures: vec![
                    (MeasureId::TotalCost, Aggregation::Sum),
                    (MeasureId::Ttft, Aggregation::P99),
                ],
                dimensions: all[..*dimensions].to_vec(),
                filter,
                // The step is part of the shape, so the SQL text does not move with the window's length.
                granularity: Granularity::Step(Step::Week),
                fill: if *zero { Fill::Zero } else { Fill::None },
                top_n: Some(3),
            };
            let keys: Vec<Vec<Literal>> = (0..*keys)
                .map(|_| {
                    let key = [Literal::String(v.string()), Literal::Enum(v.below(4) as u8)];
                    key[..*dimensions].to_vec()
                })
                .collect();
            let built = match (*dimensions, *plan) {
                (0, _) => metrics_series(scope, window, &clamp(), Caller::Session, &query, None),
                (_, 0) => metrics_ranking(scope, window, &clamp(), Caller::Session, &query),
                (_, 1) => metrics_series(
                    scope,
                    window,
                    &clamp(),
                    Caller::Session,
                    &query,
                    Some(&keys),
                ),
                _ => metrics_other(scope, window, &clamp(), Caller::Session, &query, &keys),
            };
            built.unwrap().unwrap()
        }
        Shape::TraceList { cursor, since } => {
            let mut trace = [0u8; 16];
            trace.copy_from_slice(&v.bytes(16));
            let trace = TraceId::from_bytes(trace);
            let request = TraceListRequest {
                filter: Pred::eq(TraceHeadsCol::UserId, Literal::String(v.string())),
                limit: 1 + v.below(200) as u32,
                cursor: cursor.then(|| (v.time(), trace.trace_hash(), trace)),
                as_of: v.time(),
                since: since.then(|| v.time()),
            };
            live(trace_list(
                scope,
                window,
                &clamp(),
                Caller::Session,
                request,
            ))
        }
    }
}

/// The statement with every placeholder removed, and the parameter index of each placeholder in order.
fn strip_placeholders(statement: &Statement) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut indexes = Vec::new();
    let mut rest = statement.sql.as_str();
    while let Some(at) = rest.find("{p") {
        text.push_str(&rest[..at]);
        let (number, after) = rest[at + 2..].split_once(':').unwrap();
        let index: usize = number.parse().unwrap();
        // The placeholder ends where its type does.
        let ty = &statement.params[index].ty;
        let after = after.strip_prefix(ty.as_str()).unwrap();
        rest = after.strip_prefix('}').unwrap();
        text.push('?');
        indexes.push(index);
    }
    text.push_str(rest);
    (text, indexes)
}

fn micros(t: Timestamp) -> String {
    format!("{}.{:06}", t.micros() / 1_000_000, t.micros() % 1_000_000)
}

/// The window predicates of one table reference, with `?` for each placeholder: the text that follows the
/// project predicate, the text that follows the grouping, and the values the placeholders are bound to, in order.
fn window_clause(id: TableId, window: TimeWindow) -> (String, String, Vec<String>) {
    let def = table(id);
    let time = def.retention.time_column;
    let last = window.to().saturating_add_micros(-1);
    // Ten digits: the server takes a shorter `DateTime` parameter for a broken date.
    let minute = |t: Timestamp| format!("{:010}", t.floor_minute().micros() / 1_000_000);
    let date = |t: Timestamp| t.date().to_string();
    let (mut filter, mut having, mut values) = (String::new(), String::new(), Vec::new());
    match def.column(time).unwrap().ty {
        Ty::DateTime64 => {
            filter = format!(" AND {time} >= ? AND {time} < ?");
            values.extend([micros(window.from()), micros(window.to())]);
        }
        Ty::DateTime => {
            filter = format!(" AND {time} >= ? AND {time} <= ?");
            values.extend([minute(window.from()), minute(last)]);
        }
        Ty::Date => {
            filter = format!(" AND {time} >= ? AND {time} <= ?");
            values.extend([date(window.from()), date(last)]);
        }
        Ty::Saf(..) => {
            having = format!("max({time}) >= ? AND min(first_seen) < ?");
            values.extend([micros(window.from()), micros(window.to())]);
        }
        other => panic!("no window for {other}"),
    }
    // The sort-key bounds.
    if def.column("start_minute").is_some() {
        filter.push_str(" AND start_minute >= ? AND start_minute <= ?");
        values.extend([minute(window.from()), minute(last)]);
    }
    if def.column("target_day").is_some() {
        filter.push_str(" AND target_day >= ? AND target_day <= ?");
        values.extend([date(window.from()), date(last)]);
    }
    (filter, having, values)
}

/// Checks one statement against its plan. Returns the placeholder-free text.
fn check(plan: &QueryPlan, statement: &Statement) -> Result<String, TestCaseError> {
    let (text, indexes) = strip_placeholders(statement);
    let value = |n: usize| statement.params[indexes[n]].value.as_str();

    // Only bound values: nothing quoted is left, every parameter is referenced, and nothing reads FINAL.
    prop_assert!(
        !text.contains('\''),
        "a quote outside a placeholder: {text}"
    );
    prop_assert!(!text.contains('"') && !text.contains('`'), "{text}");
    prop_assert!(!text.to_uppercase().contains("FINAL"), "{text}");
    for index in 0..statement.params.len() {
        prop_assert!(indexes.contains(&index), "p{index} is bound and unused");
    }

    // Every table reference is followed at once by the project predicate, bound to the scope, and then by the
    // window predicates of that table, bound to the plan's window.
    let tables = plan.root().tables();
    let project = plan.scope().project_id().to_string();
    let mut references = 0;
    for (at, _) in text.match_indices("FROM crucible.") {
        let rest = &text[at + "FROM crucible.".len()..];
        let name = rest.split(' ').next().unwrap();
        let id = TableId::from_name(name).unwrap();
        prop_assert!(tables.contains(&id), "{name} is not a table of the plan");
        let scoped = " WHERE project_id = ?";
        prop_assert!(
            rest[name.len()..].starts_with(scoped),
            "{name} is read without the project predicate: {text}"
        );
        let after_project = at + "FROM crucible.".len() + name.len() + scoped.len();
        let placeholder = text[..after_project].matches('?').count() - 1;
        prop_assert_eq!(value(placeholder), project.as_str());
        prop_assert_eq!(&statement.params[indexes[placeholder]].ty, "UUID");

        let (filter, having, bounds) = window_clause(id, plan.window());
        prop_assert!(
            text[after_project..].starts_with(&filter),
            "{name} is read without its window predicate {filter}: {text}"
        );
        let first = if having.is_empty() {
            placeholder + 1
        } else {
            // The window of a table whose time column is aggregated applies after its grouping.
            let found: Vec<usize> = text.match_indices(&having).map(|(at, _)| at).collect();
            prop_assert_eq!(found.len(), 1, "{} in {}", having, text);
            prop_assert!(found[0] > after_project);
            text[..found[0]].matches('?').count()
        };
        for (n, bound) in bounds.iter().enumerate() {
            prop_assert_eq!(value(first + n), bound.as_str(), "bound {} of {}", n, name);
        }
        references += 1;
    }
    prop_assert_eq!(references, tables.len(), "{}", text);
    prop_assert_eq!(text.matches("project_id").count(), tables.len());
    Ok(text)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Contract 8.3: every rendered statement other than the probe carries the project predicate and the
    /// window predicate and contains only bound values. Two assignments of values to one plan shape render
    /// byte-identical SQL, so no value reaches the text.
    #[test]
    fn every_statement_carries_the_project_and_window_predicates_and_only_bound_values(
        shape in shape(),
        seeds in (any::<u64>(), any::<u64>()),
    ) {
        let (first, second) = (build(&shape, seeds.0), build(&shape, seeds.1));
        let (a, b) = (render::plan(&first).unwrap(), render::plan(&second).unwrap());
        check(&first, &a)?;
        check(&second, &b)?;
        prop_assert_eq!(&a.sql, &b.sql, "the SQL text depends on a value");
        let types = |s: &Statement| s.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>();
        prop_assert_eq!(types(&a), types(&b));
    }

    /// A delete always carries the project predicate or the marker predicate, and only bound values.
    #[test]
    fn every_delete_is_scoped_and_bound(shape in pred_shape(), seeds in (any::<u64>(), any::<u64>())) {
        let delete = |seed: u64| {
            let v = &mut Values(seed);
            let day = PartitionId::date(CivilDate::from_day_number(v.below(40_000) as i64));
            let plan = DeletePlan::project(v.scope(), TableId::SpanCore, day, pred(&shape, v)).unwrap();
            let project = match plan.target() {
                crucible_plan::DeleteTarget::Project(scope) => scope.project_id().to_string(),
                crucible_plan::DeleteTarget::MarkerPurge { .. } => unreachable!(),
            };
            (render::delete(&plan).unwrap(), project, day.to_string())
        };
        let ((a, project, day), (b, ..)) = (delete(seeds.0), delete(seeds.1));
        prop_assert_eq!(&a.sql, &b.sql);
        let (text, indexes) = strip_placeholders(&a);
        prop_assert!(
            text.starts_with("DELETE FROM crucible.span_core IN PARTITION ID ? WHERE project_id = ?"),
            "{}", text
        );
        prop_assert!(!text.contains('\''));
        prop_assert_eq!(&a.params[indexes[0]].value, &day);
        prop_assert_eq!(&a.params[indexes[1]].value, &project);
    }
}

#[test]
fn the_probe_is_the_one_statement_without_a_scope() {
    let day = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
    let plain = render::probe(&batch_probe(TableId::SpanCore, day, epoch(), Lsn::new(9)).unwrap());
    let (text, _) = strip_placeholders(&plain);
    assert!(
        !text.contains("project_id") && !text.contains('\''),
        "{text}"
    );
    let month = PartitionId::month(2026, 11).unwrap();
    let marker =
        render::probe(&batch_probe(TableId::SpanMetrics, month, epoch(), Lsn::new(9)).unwrap());
    let (text, indexes) = strip_placeholders(&marker);
    // The marker probe names the nil project, which no scope can hold.
    assert!(
        text.contains("WHERE project_id = ? AND marker_epoch = ?"),
        "{text}"
    );
    let nil = text[..text.find("project_id = ?").unwrap() + "project_id = ?".len()]
        .matches('?')
        .count()
        - 1;
    assert_eq!(marker.params[indexes[nil]].value, Uuid::NIL.to_string());
}
