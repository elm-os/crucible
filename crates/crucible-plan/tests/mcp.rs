//! The MCP analytics builders (contract 9.9): every plan carries its scope, window, class and side; the
//! gated endpoints read only the aggregating tables; the capture-source selection of contract 9.5 and the
//! caller-fault rule of contract 9.2 reach every plan; windows are clamped to retention; and the output of
//! each builder is what its documentation lists.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    Aggregation, Caller, CaptureClass, CmpOp, DimensionId, Fill, Filter, Granularity, Literal,
    MCP_LIST_MAX_LIMIT, McpContentField, McpOperationsRequest, McpOpsFilter, McpRead, McpSelection,
    McpUserKey, MeasureId, MetricsError, MetricsQuery, Node, PlanError, Pred, QueryClass,
    QueryPlan, RetentionClamp, Source, SourceChoice, SourceSchema, Step, effective_window,
    mcp_client_versions, mcp_clients, mcp_compare_tools, mcp_deploys, mcp_error_breakdown,
    mcp_error_classes, mcp_inventory, mcp_metrics_other, mcp_metrics_ranking, mcp_metrics_series,
    mcp_metrics_totals, mcp_operation, mcp_operation_chain, mcp_operation_content,
    mcp_operation_peers, mcp_operations, mcp_presence, mcp_servers, mcp_session_errors,
    mcp_sessions, mcp_tool_clients, mcp_tool_series, mcp_tools, mcp_user_errors, mcp_users,
    metrics_schema, metrics_series, subject_hash,
};
use crucible_records::{
    CaptureSource, McpSide, ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp,
    TraceId,
};
use crucible_schema::{ColumnRef, RetentionSettings, TableKind, table};
use proptest::prelude::*;

fn scope_strategy() -> impl Strategy<Value = ProjectScope> {
    any::<[u8; 16]>()
        .prop_filter("the nil project cannot be scoped", |b| *b != [0; 16])
        .prop_map(|b| test_support::scope(ProjectId::from_bytes(b)).unwrap())
}

fn window_strategy() -> impl Strategy<Value = TimeWindow> {
    let max = Timestamp::MAX_EXCLUSIVE.micros();
    (0..max - 1, 1..max).prop_map(move |(from, len)| {
        let to = from.saturating_add(len).min(max);
        TimeWindow::new(Timestamp::from_micros(from), Timestamp::from_micros(to)).unwrap()
    })
}

fn caller_strategy() -> impl Strategy<Value = Caller> {
    prop_oneof![Just(Caller::Session), Just(Caller::Agent)]
}

fn side_strategy() -> impl Strategy<Value = McpSide> {
    prop_oneof![Just(McpSide::Client), Just(McpSide::Server)]
}

fn choice_strategy() -> impl Strategy<Value = SourceChoice> {
    prop_oneof![
        Just(SourceChoice::Auto),
        Just(SourceChoice::Class(CaptureClass::Sdk)),
        Just(SourceChoice::Class(CaptureClass::Proxy)),
        Just(SourceChoice::Class(CaptureClass::Otlp)),
    ]
}

/// A clamp that expires nothing.
fn keep() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), Timestamp::EPOCH)
}

/// A selection in which every class has a server, so every choice reads something.
fn selection(choice: SourceChoice) -> McpSelection {
    McpSelection::select(
        choice,
        [
            ("a".to_owned(), CaptureSource::Sdk),
            ("a".to_owned(), CaptureSource::ProxyStdio),
            ("b".to_owned(), CaptureSource::ProxyHttp),
            ("c".to_owned(), CaptureSource::Otlp),
        ],
    )
}

/// Every condition of a node: its predicate's leaves, and its key columns with their values.
fn conditions(node: &Node) -> Vec<(ColumnRef, CmpOp, Literal)> {
    fn walk(pred: &Pred, out: &mut Vec<(ColumnRef, CmpOp, Literal)>) {
        match pred {
            Pred::And(parts) | Pred::Or(parts) => parts.iter().for_each(|p| walk(p, out)),
            Pred::Not(part) => walk(part, out),
            Pred::Cmp(column, op, value) => out.push((*column, *op, value.clone())),
            Pred::In(column, values) => {
                out.extend(values.iter().map(|v| (*column, CmpOp::Eq, v.clone())));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    match node {
        Node::Scan(scan) => walk(scan.pred(), &mut out),
        Node::Aggregate(aggregate) => walk(aggregate.pred(), &mut out),
        Node::TimeSeries(series) => walk(series.aggregate().pred(), &mut out),
        Node::PointLookup(lookup) => {
            for key in lookup.keys() {
                for (column, value) in lookup.key_columns().iter().zip(key) {
                    out.push((*column, CmpOp::Eq, value.clone()));
                }
            }
        }
        Node::FieldRange(range) => {
            for (column, value) in range.key_columns().iter().zip(range.key()) {
                out.push((*column, CmpOp::Eq, value.clone()));
            }
        }
        _ => {}
    }
    out
}

/// One built plan with what the endpoint promises about it.
struct Built {
    name: &'static str,
    plan: QueryPlan,
    class: QueryClass,
    gated: bool,
    side: bool,
}

/// Every MCP plan of one read.
fn every_plan(
    scope: ProjectScope,
    window: TimeWindow,
    caller: Caller,
    read: &McpRead<'_>,
) -> Vec<Built> {
    let clamp = keep();
    let trace = TraceId::from_bytes([7; 16]);
    let span = SpanId::from_bytes([8; 8]);
    let users = [McpUserKey {
        user_id: "u".to_owned(),
        subject_hash: [1; 16],
    }];
    let request = McpOperationsRequest {
        filter: McpOpsFilter {
            target: Some("t".to_owned()),
            user: Some("01".repeat(16)),
            errors_only: true,
            ..McpOpsFilter::default()
        },
        limit: 10,
        cursor: Some((window.from(), span)),
        as_of: window.to(),
    };
    let deploys = ["d1".to_owned(), "d2".to_owned()];
    let mut out = Vec::new();
    let mut add = |name, plan: Result<Option<QueryPlan>, PlanError>, class, gated, side| {
        out.push(Built {
            name,
            plan: plan.unwrap().unwrap(),
            class,
            gated,
            side,
        });
    };
    let analytics = QueryClass::Analytics;
    let raw = QueryClass::AnalyticsRaw;
    let presence = mcp_presence(
        scope,
        window,
        &clamp,
        caller.class(analytics),
        read.side,
        read.server,
    );
    add("presence", presence, analytics, true, true);
    add(
        "servers",
        mcp_servers(scope, window, &clamp, caller, read),
        analytics,
        true,
        true,
    );
    add(
        "tools",
        mcp_tools(scope, window, &clamp, caller, read),
        analytics,
        true,
        true,
    );
    let series = mcp_tool_series(scope, window, &clamp, caller, read, "t", Step::Hour);
    add("tool series", series, analytics, true, true);
    let clients = mcp_tool_clients(scope, window, &clamp, caller, read, "t");
    add("tool clients", clients, analytics, true, true);
    let classes = mcp_error_classes(scope, window, &clamp, caller, read);
    add("error classes", classes, analytics, true, true);
    add(
        "clients",
        mcp_clients(scope, window, &clamp, caller, read),
        analytics,
        true,
        true,
    );
    let deploys_plan = mcp_deploys(scope, window, &clamp, caller, read.server);
    add("deploys", deploys_plan, analytics, true, false);
    let inventory = mcp_inventory(scope, window, &clamp, caller, read.server, &deploys);
    add("inventory", inventory, analytics, true, false);
    if read.server.is_some() {
        let compare = mcp_compare_tools(scope, window, &clamp, caller, read, &deploys);
        add("compare", compare, analytics, true, true);
    }
    let breakdown = mcp_error_breakdown(scope, window, &clamp, caller, read, 50);
    add("error breakdown", breakdown, raw, false, true);
    let versions = mcp_client_versions(scope, window, &clamp, caller, read, 50);
    add("client versions", versions, raw, false, true);
    add(
        "users",
        mcp_users(scope, window, &clamp, caller, read, 50),
        raw,
        false,
        true,
    );
    let errors = mcp_user_errors(scope, window, &clamp, caller, read, &users);
    add("user errors", errors, raw, false, true);
    add(
        "sessions",
        mcp_sessions(scope, window, &clamp, caller, read, 50),
        raw,
        false,
        true,
    );
    let keys = ["k".to_owned()];
    let errors = mcp_session_errors(scope, window, &clamp, caller, read, &keys);
    add("session errors", errors, raw, false, true);
    let operations = mcp_operations(scope, window, &clamp, caller, read, &request);
    add("operations", operations, QueryClass::List, false, true);
    let point = QueryClass::Point;
    let one = mcp_operation(scope, window, &clamp, caller, read.side, (trace, span));
    add("operation", one, point, false, true);
    let key = (read.side, trace, span);
    let args = mcp_operation_content(
        scope,
        window,
        &clamp,
        caller,
        key,
        McpContentField::Args,
        64,
    );
    add("operation args", args, point, false, true);
    let other = match read.side {
        McpSide::Server => McpSide::Client,
        _ => McpSide::Server,
    };
    let peers = mcp_operation_peers(scope, window, &clamp, caller, other, (trace, span, span));
    add("operation peers", peers, point, false, true);
    let chain = (trace, CaptureSource::Sdk);
    let chain = mcp_operation_chain(scope, window, &clamp, caller, read.side, chain);
    add("operation chain", chain, point, false, true);
    out
}

proptest! {
    #[test]
    fn every_mcp_plan_carries_its_scope_window_class_and_side(
        scope in scope_strategy(),
        window in window_strategy(),
        caller in caller_strategy(),
        side in side_strategy(),
        choice in choice_strategy(),
        include_caller_faults: bool,
        server in prop::option::of("[a-c]"),
    ) {
        let selection = selection(choice);
        let read = McpRead { side, selection: &selection, include_caller_faults, server: server.as_deref() };
        for built in every_plan(scope, window, caller, &read) {
            let plan = &built.plan;
            prop_assert_eq!(plan.scope(), scope, "{}", built.name);
            prop_assert_eq!(plan.window(), window, "{}", built.name);
            let class = match caller {
                Caller::Session => built.class,
                Caller::Agent => QueryClass::Agent,
            };
            prop_assert_eq!(plan.class(), class, "{}", built.name);
            let tables = plan.root().tables();
            // The 300 ms dashboard target binds these: they read the aggregating tables only.
            if built.gated {
                for table_id in &tables {
                    prop_assert_eq!(table(*table_id).kind, TableKind::Aggregating, "{}", built.name);
                }
            }
            let found = conditions(plan.root());
            let on = |name: &str| found.iter().filter(|(c, _, _)| c.name() == name).collect::<Vec<_>>();
            if built.side {
                let sides = on("side");
                prop_assert!(
                    sides.iter().any(|(_, op, v)| *op == CmpOp::Eq && *v == Literal::Enum(
                        if built.name == "operation peers" {
                            match side { McpSide::Server => McpSide::Client.value(), _ => McpSide::Server.value() }
                        } else {
                            side.value()
                        }
                    )),
                    "{} reads no side",
                    built.name
                );
            }
            // A data plan of a side table under a selection names its capture sources; a point read does not.
            let data = built.side && !matches!(built.class, QueryClass::Point) && built.name != "presence";
            if data {
                let sources = on("capture_source");
                prop_assert!(!sources.is_empty(), "{} reads every capture source", built.name);
            }
            // Raw reads leave caller faults out unless asked.
            let raw = tables.contains(&TableId::McpOps) && data;
            if raw {
                let faults = on("caller_fault");
                prop_assert_eq!(faults.is_empty(), include_caller_faults, "{}", built.name);
            }
            if let (Some(name), true) = (server.as_deref(), data || built.name == "presence") {
                prop_assert!(
                    on("server_name").iter().any(|(_, _, v)| *v == Literal::String(name.to_owned())),
                    "{} ignores the server",
                    built.name
                );
            }
        }
    }

    #[test]
    fn every_mcp_builder_clamps_its_window_to_retention(
        scope in scope_strategy(),
        window in window_strategy(),
        now in (0..Timestamp::MAX_EXCLUSIVE.micros()).prop_map(Timestamp::from_micros),
        days in prop::option::of(0u32..800),
        content_days in prop::option::of(0u32..800),
    ) {
        let settings = RetentionSettings {
            retention_days: days,
            mcp_content_retention_days: content_days,
            ..RetentionSettings::default()
        };
        let clamp = RetentionClamp::new(settings, now);
        let selection = selection(SourceChoice::Auto);
        let read = McpRead { side: McpSide::Server, selection: &selection, include_caller_faults: false, server: None };
        let caller = Caller::Session;
        let (trace, span) = (TraceId::from_bytes([1; 16]), SpanId::from_bytes([2; 8]));
        let plans = [
            (mcp_presence(scope, window, &clamp, caller.class(QueryClass::Analytics), McpSide::Server, None).unwrap(), TableId::McpToolRollup),
            (mcp_servers(scope, window, &clamp, caller, &read).unwrap(), TableId::McpToolRollup),
            (mcp_deploys(scope, window, &clamp, caller, None).unwrap(), TableId::McpDeploys),
            (mcp_inventory(scope, window, &clamp, caller, None, &[]).unwrap(), TableId::McpToolInventory),
            (mcp_users(scope, window, &clamp, caller, &read, 10).unwrap(), TableId::McpOps),
            (mcp_operation(scope, window, &clamp, caller, McpSide::Server, (trace, span)).unwrap(), TableId::McpOps),
            (
                mcp_operation_content(scope, window, &clamp, caller, (McpSide::Server, trace, span), McpContentField::Result, 8)
                    .unwrap(),
                TableId::McpContent,
            ),
        ];
        for (plan, table_id) in plans {
            let expected = effective_window(table_id, window, &settings, now).unwrap();
            prop_assert_eq!(plan.as_ref().map(QueryPlan::window), expected);
            if let Some(plan) = plan {
                prop_assert_eq!(plan.root().tables(), vec![table_id]);
            }
        }
    }
}

fn scope() -> ProjectScope {
    test_support::scope(ProjectId::from_bytes([9; 16])).unwrap()
}

fn window() -> TimeWindow {
    let from = Timestamp::from_micros(1_794_900_000_000_000);
    TimeWindow::new(from, Timestamp::from_micros(1_794_907_800_000_000)).unwrap()
}

fn shape(plan: &QueryPlan) -> Vec<String> {
    plan.output()
        .columns()
        .iter()
        .map(|c| format!("{} {}", c.name, c.ty))
        .collect()
}

#[test]
fn the_outputs_are_what_the_builders_document() {
    let selection = selection(SourceChoice::Auto);
    let read = McpRead {
        side: McpSide::Server,
        selection: &selection,
        include_caller_faults: false,
        server: Some("a"),
    };
    let (s, w, k, c) = (
        scope(),
        window(),
        RetentionClamp::new(RetentionSettings::default(), Timestamp::EPOCH),
        Caller::Session,
    );
    let hist = "Tuple(Array(UInt16), Array(UInt64))";
    let dt64 = "DateTime64(6, 'UTC')";
    let presence = mcp_presence(
        s,
        w,
        &k,
        c.class(QueryClass::Analytics),
        McpSide::Client,
        None,
    )
    .unwrap()
    .unwrap();
    let found = shape(&presence);
    assert_eq!(found[0], "server_name String");
    assert!(found[1].starts_with("capture_source Enum8"), "{found:?}");
    assert_eq!(found[2], "sum_count UInt64");
    assert_eq!(found[3], format!("max_last_seen {dt64}"));
    let servers = mcp_servers(s, w, &k, c, &read).unwrap().unwrap();
    let names: Vec<String> = servers
        .output()
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "server_name",
            "client_name",
            "error_class",
            "caller_fault",
            "sum_count",
            "histogram_dur_hist",
            "max_last_seen"
        ]
    );
    assert_eq!(servers.output().columns()[5].ty.to_string(), hist);
    let tools = mcp_tools(s, w, &k, c, &read).unwrap().unwrap();
    let names: Vec<String> = tools
        .output()
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "server_name",
            "target_name",
            "error_class",
            "caller_fault",
            "sum_count",
            "histogram_dur_hist",
            "sum_request_bytes",
            "sum_response_bytes",
            "max_last_seen",
        ]
    );
    let series = mcp_tool_series(s, w, &k, c, &read, "t", Step::FiveMinutes)
        .unwrap()
        .unwrap();
    let names: Vec<String> = series
        .output()
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "bucket",
            "error_class",
            "caller_fault",
            "sum_count",
            "histogram_dur_hist"
        ]
    );
    let deploys = mcp_deploys(s, w, &k, c, None).unwrap().unwrap();
    let names: Vec<String> = deploys
        .output()
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "server_name",
            "deploy_id",
            "server_version",
            "capture_source",
            "min_first_seen",
            "max_last_seen"
        ]
    );
    let breakdown = mcp_error_breakdown(s, w, &k, c, &read, 10)
        .unwrap()
        .unwrap();
    let types: Vec<String> = shape(&breakdown);
    assert!(types[5].ends_with("Nullable(Int32)"), "{types:?}");
    assert_eq!(types.len(), 9);
    let users = mcp_users(s, w, &k, c, &read, 10).unwrap().unwrap();
    let names: Vec<String> = users
        .output()
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "user_id",
            "user_subject_hash",
            "count",
            "min_start_time",
            "max_start_time",
            "uniq_group_key",
            "max_identity_source",
        ]
    );
    let content = mcp_operation_content(
        s,
        w,
        &k,
        c,
        (
            McpSide::Server,
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([1; 8]),
        ),
        McpContentField::Args,
        16,
    )
    .unwrap()
    .unwrap();
    assert_eq!(content.output().columns().len(), 2);
}

#[test]
fn refusals_name_what_to_change() {
    let selection = selection(SourceChoice::Auto);
    let none = McpRead {
        side: McpSide::None,
        selection: &selection,
        include_caller_faults: false,
        server: None,
    };
    let (s, w, k, c) = (
        scope(),
        window(),
        RetentionClamp::new(RetentionSettings::default(), Timestamp::EPOCH),
        Caller::Session,
    );
    assert!(matches!(
        mcp_servers(s, w, &k, c, &none),
        Err(PlanError::UnsupportedFilter { field: "side", .. })
    ));
    let read = McpRead {
        side: McpSide::Server,
        ..none
    };
    assert!(matches!(
        mcp_compare_tools(s, w, &k, c, &read, &[]),
        Err(PlanError::UnsupportedFilter {
            field: "server",
            ..
        })
    ));
    for limit in [0, MCP_LIST_MAX_LIMIT + 1] {
        assert_eq!(mcp_users(s, w, &k, c, &read, limit), Err(PlanError::Limit));
    }
    // A named class no server has: nothing to read, so no plan.
    let empty = McpSelection::select(
        SourceChoice::Class(CaptureClass::Sdk),
        [("x".to_owned(), CaptureSource::Otlp)],
    );
    let read = McpRead {
        selection: &empty,
        ..read
    };
    assert_eq!(mcp_servers(s, w, &k, c, &read), Ok(None));
    assert_eq!(subject_hash(&"ab".repeat(16)), Some([0xab; 16]));
    assert_eq!(subject_hash("not-a-hash"), None);
}

fn mcp_query(
    measure: MeasureId,
    aggregation: Aggregation,
    dimensions: Vec<DimensionId>,
) -> MetricsQuery {
    MetricsQuery {
        source: Source::McpOps,
        measures: vec![(measure, aggregation)],
        dimensions,
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: Some(5),
    }
}

#[test]
fn the_registry_holds_the_mcp_alert_measures() {
    let schema = SourceSchema::of(Source::McpOps).unwrap();
    let ids: Vec<&str> = schema.measures.iter().map(|m| m.as_str()).collect();
    assert_eq!(
        ids,
        [
            "mcp.calls",
            "mcp.error_count",
            "mcp.error_rate",
            "mcp.duration_p50",
            "mcp.duration_p90",
            "mcp.duration_p99"
        ]
    );
    assert_eq!(schema.limits.max_measures, 1);
    assert_eq!(
        schema.dimensions,
        [DimensionId::McpServer, DimensionId::McpTarget]
    );
    assert_eq!(MeasureId::McpErrorRate.unit(), "ppm");
    assert_eq!(MeasureId::McpErrorRate.aggregations(), [Aggregation::Rate]);
    assert_eq!(MeasureId::McpDurationP90.aggregations(), [Aggregation::P90]);
    assert!(MeasureId::McpCalls.additive() && !MeasureId::McpErrorRate.additive());
    assert!(metrics_schema().iter().any(|s| s.source == Source::McpOps));
    // The spans source keeps its own dimensions.
    let spans = SourceSchema::of(Source::Spans).unwrap();
    assert!(spans.dimensions.iter().all(|d| d.source() == Source::Spans));
}

#[test]
fn mcp_metrics_compile_one_measure_for_one_side() {
    let (s, w, k, c) = (
        scope(),
        window(),
        RetentionClamp::new(RetentionSettings::default(), Timestamp::EPOCH),
        Caller::Session,
    );
    let selection = selection(SourceChoice::Auto);
    let read = McpRead {
        side: McpSide::Client,
        selection: &selection,
        include_caller_faults: false,
        server: None,
    };
    for measure in [
        MeasureId::McpCalls,
        MeasureId::McpErrorCount,
        MeasureId::McpErrorRate,
        MeasureId::McpDurationP99,
    ] {
        let aggregation = measure.aggregations()[0];
        let query = mcp_query(measure, aggregation, vec![DimensionId::McpServer]);
        let ranking = mcp_metrics_ranking(s, w, &k, c, &query, &read)
            .unwrap()
            .unwrap();
        assert_eq!(ranking.root().tables(), [TableId::McpToolRollup]);
        let keys = [vec![Literal::String("a".to_owned())]];
        let series = mcp_metrics_series(s, w, &k, c, &query, (&read, Some(&keys)))
            .unwrap()
            .unwrap();
        let Node::TimeSeries(series_node) = series.root() else {
            panic!("a time series")
        };
        let group: Vec<&str> = series_node
            .aggregate()
            .group()
            .iter()
            .map(|c| c.name())
            .collect();
        let expected: &[&str] = if measure.by_error_class() {
            &["server_name", "error_class"]
        } else {
            &["server_name"]
        };
        assert_eq!(group, expected, "{measure:?}");
        let found = conditions(series.root());
        assert!(
            found
                .iter()
                .any(|(c, _, v)| c.name() == "caller_fault" && *v == Literal::UInt(0))
        );
        assert!(found.iter().any(|(c, _, v)| c.name() == "side" && *v == Literal::Enum(McpSide::Client.value())));
        let errors = found
            .iter()
            .any(|(c, op, _)| c.name() == "error_class" && *op == CmpOp::Ne);
        assert_eq!(errors, measure == MeasureId::McpErrorCount, "{measure:?}");
        let other = mcp_metrics_other(s, w, &k, c, &query, (&read, &keys));
        assert_eq!(other.is_ok(), measure.additive(), "{measure:?}");
    }
    // The generic builders refuse the source: it needs a side.
    let query = mcp_query(MeasureId::McpCalls, Aggregation::Sum, Vec::new());
    assert!(matches!(
        metrics_series(s, w, &k, c, &query, None),
        Err(MetricsError::Invalid {
            field: "source",
            ..
        })
    ));
    // Two measures, a dimension of another source, or a filter are refused.
    let mut two = query.clone();
    two.measures
        .push((MeasureId::McpErrorCount, Aggregation::Sum));
    assert!(matches!(
        mcp_metrics_series(s, w, &k, c, &two, (&read, None)),
        Err(MetricsError::Invalid {
            field: "measures",
            ..
        })
    ));
    let other = mcp_query(
        MeasureId::McpCalls,
        Aggregation::Sum,
        vec![DimensionId::Level],
    );
    assert!(matches!(
        mcp_metrics_series(s, w, &k, c, &other, (&read, None)),
        Err(MetricsError::Invalid {
            field: "dimensions",
            ..
        })
    ));
    let mut filtered = query;
    filtered.filter = Filter::Name("x".to_owned());
    assert!(matches!(
        mcp_metrics_series(s, w, &k, c, &filtered, (&read, None)),
        Err(MetricsError::Invalid {
            field: "filter",
            ..
        })
    ));
}

#[test]
fn an_alert_reads_mcp_totals_in_class_alert_with_every_call_counted() {
    let (s, w, k) = (
        scope(),
        window(),
        RetentionClamp::new(RetentionSettings::default(), Timestamp::EPOCH),
    );
    let selection = selection(SourceChoice::Auto);
    let read = McpRead {
        side: McpSide::Server,
        selection: &selection,
        include_caller_faults: false,
        server: None,
    };
    let both = || vec![DimensionId::McpServer, DimensionId::McpTarget];
    for measure in [
        MeasureId::McpCalls,
        MeasureId::McpErrorCount,
        MeasureId::McpErrorRate,
        MeasureId::McpDurationP50,
        MeasureId::McpDurationP90,
        MeasureId::McpDurationP99,
    ] {
        let query = mcp_query(measure, measure.aggregations()[0], both());
        let plan = mcp_metrics_totals(s, w, &k, &query, &read, (true, 51))
            .unwrap()
            .unwrap();
        assert_eq!(plan.class(), QueryClass::Alert, "{measure:?}");
        assert_eq!(plan.root().tables(), [TableId::McpToolRollup]);
        let Node::Aggregate(aggregate) = plan.root() else {
            panic!("an aggregate")
        };
        let group: Vec<&str> = aggregate.group().iter().map(|c| c.name()).collect();
        // The error measures are read whole, per error class, so that a key without errors reads zero.
        let errors = matches!(measure, MeasureId::McpErrorCount | MeasureId::McpErrorRate);
        let expected: &[&str] = if errors {
            &["server_name", "target_name", "error_class"]
        } else {
            &["server_name", "target_name"]
        };
        assert_eq!(group, expected, "{measure:?}");
        assert_eq!(aggregate.limit(), (!errors).then_some(51), "{measure:?}");
        let found = conditions(plan.root());
        assert!(
            !found
                .iter()
                .any(|(c, op, _)| c.name() == "error_class" && *op == CmpOp::Ne),
            "{measure:?} counts every call"
        );
        assert!(
            found
                .iter()
                .any(|(c, _, v)| c.name() == "caller_fault" && *v == Literal::UInt(0))
        );
        assert!(found.iter().any(|(c, _, v)| c.name() == "side" && *v == Literal::Enum(McpSide::Server.value())));
    }
    // A spans dimension, a dimension twice, an aggregation the measure lacks, or a spans measure are refused.
    let refused = |query: MetricsQuery, field: &str| {
        let error = mcp_metrics_totals(s, w, &k, &query, &read, (true, 51)).unwrap_err();
        assert!(
            matches!(&error, MetricsError::Invalid { field: f, .. } if *f == field),
            "{error:?}"
        );
    };
    let rate = MeasureId::McpErrorRate;
    refused(
        mcp_query(rate, Aggregation::Rate, vec![DimensionId::Level]),
        "dimensions",
    );
    refused(
        mcp_query(rate, Aggregation::Rate, vec![DimensionId::McpServer; 2]),
        "dimensions",
    );
    refused(mcp_query(rate, Aggregation::Sum, Vec::new()), "measures");
    let mut spans = mcp_query(MeasureId::Count, Aggregation::Sum, Vec::new());
    spans.source = Source::Spans;
    refused(spans, "measures");
}
