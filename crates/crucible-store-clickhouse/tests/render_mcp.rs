//! The SQL of the MCP analytics plans (contract 9.9): one snapshot per plan of each builder, read before it was
//! accepted. Every statement carries the project predicate, the window predicates and the side, and binds
//! every value.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support::scope;
use crucible_plan::{
    Aggregation, Caller, DimensionId, Fill, Filter, Granularity, Literal, McpContentField,
    McpOperationsRequest, McpOpsFilter, McpRead, McpSelection, McpUserKey, MeasureId, MetricsQuery,
    PlanError, QueryClass, QueryPlan, RetentionClamp, Source, SourceChoice, Step,
    mcp_client_versions, mcp_clients, mcp_compare_tools, mcp_deploys, mcp_error_breakdown,
    mcp_error_classes, mcp_inventory, mcp_metrics_other, mcp_metrics_ranking, mcp_metrics_series,
    mcp_metrics_totals, mcp_operation, mcp_operation_chain, mcp_operation_content,
    mcp_operation_peers, mcp_operations, mcp_presence, mcp_servers, mcp_session_errors,
    mcp_sessions, mcp_tool_clients, mcp_tool_series, mcp_tools, mcp_user_errors, mcp_users,
};
use crucible_records::{
    CaptureSource, ErrorClass, McpSide, ProjectId, ProjectScope, SpanId, TimeWindow, Timestamp,
    TraceId,
};
use crucible_schema::RetentionSettings;
use crucible_store_clickhouse::render;

const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

fn project() -> ProjectScope {
    scope(ProjectId::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap()).unwrap()
}

fn window() -> TimeWindow {
    TimeWindow::new(T0, T0.saturating_add_micros(3_600_000_000)).unwrap()
}

fn clamp() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), T0)
}

fn live(plan: Result<Option<QueryPlan>, PlanError>) -> QueryPlan {
    plan.unwrap().unwrap()
}

fn rendered(plan: &QueryPlan) -> String {
    let statement = render::plan(plan).unwrap();
    assert!(
        statement.sql.contains("WHERE project_id = {p"),
        "{}",
        statement.sql
    );
    statement.to_string()
}

/// Two servers under auto: one read from the SDK, one from the proxy.
fn selection() -> McpSelection {
    McpSelection::select(
        SourceChoice::Auto,
        [
            ("files".to_owned(), CaptureSource::Sdk),
            ("files".to_owned(), CaptureSource::ProxyStdio),
            ("db".to_owned(), CaptureSource::ProxyHttp),
        ],
    )
}

fn read(selection: &McpSelection) -> McpRead<'_> {
    McpRead {
        side: McpSide::Server,
        selection,
        include_caller_faults: false,
        server: None,
    }
}

#[test]
fn mcp_rollup_plans() {
    let (s, w, c, caller) = (project(), window(), clamp(), Caller::Session);
    let selection = selection();
    let read = read(&selection);
    let files = McpRead {
        server: Some("files"),
        ..read
    };
    insta::assert_snapshot!(
        "mcp_presence",
        rendered(&live(mcp_presence(
            s,
            w,
            &c,
            caller.class(QueryClass::Analytics),
            McpSide::Client,
            Some("files")
        )))
    );
    insta::assert_snapshot!(
        "mcp_servers",
        rendered(&live(mcp_servers(s, w, &c, caller, &read)))
    );
    insta::assert_snapshot!(
        "mcp_tools",
        rendered(&live(mcp_tools(s, w, &c, caller, &read)))
    );
    insta::assert_snapshot!(
        "mcp_tool_series",
        rendered(&live(mcp_tool_series(
            s,
            w,
            &c,
            caller,
            &files,
            "read_file",
            Step::Minute
        )))
    );
    insta::assert_snapshot!(
        "mcp_tool_clients",
        rendered(&live(mcp_tool_clients(
            s,
            w,
            &c,
            caller,
            &files,
            "read_file"
        )))
    );
    insta::assert_snapshot!(
        "mcp_error_classes",
        rendered(&live(mcp_error_classes(s, w, &c, caller, &read)))
    );
    insta::assert_snapshot!(
        "mcp_clients",
        rendered(&live(mcp_clients(s, w, &c, caller, &read)))
    );
    let deploys = ["v1".to_owned(), "v2".to_owned()];
    insta::assert_snapshot!(
        "mcp_compare_tools",
        rendered(&live(mcp_compare_tools(s, w, &c, caller, &files, &deploys)))
    );
    insta::assert_snapshot!(
        "mcp_deploys",
        rendered(&live(mcp_deploys(s, w, &c, caller, Some("files"))))
    );
    insta::assert_snapshot!(
        "mcp_inventory",
        rendered(&live(mcp_inventory(
            s,
            w,
            &c,
            caller,
            Some("files"),
            &deploys
        )))
    );
}

#[test]
fn mcp_raw_plans() {
    let (s, w, c, caller) = (project(), window(), clamp(), Caller::Session);
    let selection = McpSelection::select(
        SourceChoice::Class(crucible_plan::CaptureClass::Sdk),
        [("files".to_owned(), CaptureSource::Sdk)],
    );
    let read = read(&selection);
    insta::assert_snapshot!(
        "mcp_error_breakdown",
        rendered(&live(mcp_error_breakdown(s, w, &c, caller, &read, 50)))
    );
    insta::assert_snapshot!(
        "mcp_client_versions",
        rendered(&live(mcp_client_versions(s, w, &c, caller, &read, 50)))
    );
    insta::assert_snapshot!(
        "mcp_users",
        rendered(&live(mcp_users(s, w, &c, caller, &read, 50)))
    );
    let users = [McpUserKey {
        user_id: "alice".to_owned(),
        subject_hash: [0xab; 16],
    }];
    insta::assert_snapshot!(
        "mcp_user_errors",
        rendered(&live(mcp_user_errors(s, w, &c, caller, &read, &users)))
    );
    let with_faults = McpRead {
        include_caller_faults: true,
        ..read
    };
    insta::assert_snapshot!(
        "mcp_sessions_with_caller_faults",
        rendered(&live(mcp_sessions(s, w, &c, caller, &with_faults, 50)))
    );
    insta::assert_snapshot!(
        "mcp_session_errors",
        rendered(&live(mcp_session_errors(
            s,
            w,
            &c,
            caller,
            &read,
            &["g1".to_owned()]
        )))
    );
    let page = McpOperationsRequest {
        filter: McpOpsFilter::default(),
        limit: 50,
        cursor: None,
        as_of: T0,
    };
    insta::assert_snapshot!(
        "mcp_operations_first_page",
        rendered(&live(mcp_operations(s, w, &c, caller, &read, &page)))
    );
    let later = McpOperationsRequest {
        filter: McpOpsFilter {
            target: Some("read_file".to_owned()),
            error_class: Some(ErrorClass::Timeout),
            session: Some("g1".to_owned()),
            user: Some("ab".repeat(16)),
            ..McpOpsFilter::default()
        },
        cursor: Some((
            T0.saturating_add_micros(5_000_000),
            SpanId::from_bytes([3; 8]),
        )),
        ..page
    };
    insta::assert_snapshot!(
        "mcp_operations_filtered_after_cursor",
        rendered(&live(mcp_operations(s, w, &c, caller, &read, &later)))
    );
}

#[test]
fn mcp_operation_plans() {
    let (s, w, c, caller) = (project(), window(), clamp(), Caller::Session);
    let (trace, span) = (TraceId::from_bytes([0xa1; 16]), SpanId::from_bytes([2; 8]));
    insta::assert_snapshot!(
        "mcp_operation",
        rendered(&live(mcp_operation(
            s,
            w,
            &c,
            caller,
            McpSide::Server,
            (trace, span)
        )))
    );
    insta::assert_snapshot!(
        "mcp_operation_content",
        rendered(&live(mcp_operation_content(
            s,
            w,
            &c,
            caller,
            (McpSide::Server, trace, span),
            McpContentField::Result,
            16_384
        )))
    );
    insta::assert_snapshot!(
        "mcp_operation_peers",
        rendered(&live(mcp_operation_peers(
            s,
            w,
            &c,
            caller,
            McpSide::Client,
            (trace, span, SpanId::from_bytes([1; 8]))
        )))
    );
    insta::assert_snapshot!(
        "mcp_operation_chain",
        rendered(&live(mcp_operation_chain(
            s,
            w,
            &c,
            caller,
            McpSide::Server,
            (trace, CaptureSource::Sdk)
        )))
    );
}

#[test]
fn mcp_metrics_plans() {
    let (s, w, c, caller) = (project(), window(), clamp(), Caller::Session);
    let selection = selection();
    let read = read(&selection);
    let query = |measure: MeasureId, aggregation: Aggregation| MetricsQuery {
        source: Source::McpOps,
        measures: vec![(measure, aggregation)],
        dimensions: vec![DimensionId::McpTarget],
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: Some(5),
    };
    let keys = [vec![Literal::String("read_file".to_owned())]];
    let errors = query(MeasureId::McpErrorCount, Aggregation::Sum);
    insta::assert_snapshot!(
        "mcp_metrics_ranking",
        rendered(
            &mcp_metrics_ranking(s, w, &c, caller, &errors, &read)
                .unwrap()
                .unwrap()
        )
    );
    insta::assert_snapshot!(
        "mcp_metrics_other",
        rendered(
            &mcp_metrics_other(s, w, &c, caller, &errors, (&read, &keys))
                .unwrap()
                .unwrap()
        )
    );
    let rate = query(MeasureId::McpErrorRate, Aggregation::Rate);
    insta::assert_snapshot!(
        "mcp_metrics_series_rate",
        rendered(
            &mcp_metrics_series(s, w, &c, caller, &rate, (&read, Some(&keys)))
                .unwrap()
                .unwrap()
        )
    );
    // An alert's totals: an error measure grouped by error class and read whole, a percentile ranked by calls.
    insta::assert_snapshot!(
        "mcp_metrics_totals_rate",
        rendered(
            &mcp_metrics_totals(s, w, &c, &rate, &read, (true, 51))
                .unwrap()
                .unwrap()
        )
    );
    let p99 = query(MeasureId::McpDurationP99, Aggregation::P99);
    insta::assert_snapshot!(
        "mcp_metrics_totals_p99",
        rendered(
            &mcp_metrics_totals(s, w, &c, &p99, &read, (true, 51))
                .unwrap()
                .unwrap()
        )
    );
}

/// Seven days from half past an hour: long enough for the hourly tier, with half an hour of minutes at each edge.
fn week() -> TimeWindow {
    TimeWindow::new(T0, T0.saturating_add_micros(7 * 86_400_000_000)).unwrap()
}

/// A tiered statement groups the union of two branches, each with the project and window predicates of its own
/// table and the plan's conditions.
fn tiered(plan: &QueryPlan) -> String {
    let sql = rendered(plan);
    assert_eq!(sql.matches("WHERE project_id = {p").count(), 2, "{sql}");
    assert_eq!(sql.matches(" UNION ALL ").count(), 1, "{sql}");
    assert!(sql.contains("FROM crucible.mcp_tool_rollup_hourly WHERE"), "{sql}");
    sql
}

#[test]
fn mcp_rollup_plans_over_the_hourly_tier() {
    let (s, w, c, caller) = (project(), week(), clamp(), Caller::Session);
    let selection = selection();
    let read = read(&selection);
    let files = McpRead {
        server: Some("files"),
        ..read
    };
    insta::assert_snapshot!(
        "mcp_presence_hourly",
        tiered(&live(mcp_presence(
            s,
            w,
            &c,
            caller.class(QueryClass::Analytics),
            McpSide::Client,
            Some("files")
        )))
    );
    insta::assert_snapshot!(
        "mcp_servers_hourly",
        tiered(&live(mcp_servers(s, w, &c, caller, &read)))
    );
    // A series over the tier selects its hours under the minute column's name.
    insta::assert_snapshot!(
        "mcp_tool_series_hourly",
        tiered(&live(mcp_tool_series(
            s,
            w,
            &c,
            caller,
            &files,
            "read_file",
            Step::Hour
        )))
    );
    // A step finer than an hour cannot be served by the tier: the series reads minute rows only.
    let fine = rendered(&live(mcp_tool_series(
        s,
        w,
        &c,
        caller,
        &files,
        "read_file",
        Step::FifteenMinutes,
    )));
    assert!(!fine.contains("mcp_tool_rollup_hourly"), "{fine}");
    let query = MetricsQuery {
        source: Source::McpOps,
        measures: vec![(MeasureId::McpErrorRate, Aggregation::Rate)],
        dimensions: vec![DimensionId::McpTarget],
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: Some(5),
    };
    let keys = [vec![Literal::String("read_file".to_owned())]];
    insta::assert_snapshot!(
        "mcp_metrics_series_rate_hourly",
        tiered(
            &mcp_metrics_series(s, w, &c, caller, &query, (&read, Some(&keys)))
                .unwrap()
                .unwrap()
        )
    );
}
