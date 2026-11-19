//! The MCP analytics services (contract 9.9) over the in-memory store: the capture-source selection of
//! contract 9.5 (one class per server, never summed), the caller-fault rule of contract 9.2, the unused tools,
//! the operation list with its cursor, one operation with its content, other side and multi-round chain, the
//! compare of two deploys, the `mcp_ops` metrics source and the totals an alert reads, and a store outage.

use crucible_plan::{
    Aggregation, CaptureClass, DimensionId, Fill, Filter, Granularity, McpOpsFilter, MeasureId,
    MetricsQuery, Source, SourceChoice, Value,
};
use crucible_query::{
    CompareSide, McpCompareQuery, McpMetricsRequest, McpOperationCursor, McpOperationsQuery,
    McpQuery, MetricValue, MetricsTotals, QueryServiceError,
};
use crucible_records::{
    CaptureSource, ErrorClass, GroupKeyKind, Histogram, McpSide, McpStatus, ResultType, SpanId,
    TableId, TimeWindow, TraceId,
};

use crate::store::stored_row;
use crate::world::{NOW, World, ctx, project, run};

const MINUTE: i64 = 60_000_000;

fn hour() -> TimeWindow {
    TimeWindow::new(NOW.saturating_add_micros(-60 * MINUTE), NOW).unwrap()
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
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

/// One partial of `mcp_tool_rollup`.
struct Partial<'a> {
    side: McpSide,
    minutes_ago: i64,
    server: &'a str,
    source: CaptureSource,
    tool: &'a str,
    client: &'a str,
    class: ErrorClass,
    caller_fault: bool,
    count: u64,
    durations: &'a [u64],
    deploy: &'a str,
}

impl Default for Partial<'_> {
    fn default() -> Self {
        Self {
            side: McpSide::Server,
            minutes_ago: 10,
            server: "files",
            source: CaptureSource::Sdk,
            tool: "read",
            client: "agent",
            class: ErrorClass::None,
            caller_fault: false,
            count: 1,
            durations: &[1_000],
            deploy: "v1",
        }
    }
}

fn rollup(world: &World, p: Partial<'_>) {
    let minute = NOW.saturating_add_micros(-p.minutes_ago * MINUTE);
    let row = stored_row(
        TableId::McpToolRollup,
        &[
            ("project_id", Value::Uuid(project(1).uuid())),
            ("side", Value::Enum(p.side.value())),
            ("minute", Value::Time(minute)),
            ("server_name", text(p.server)),
            ("deploy_id", text(p.deploy)),
            ("capture_source", Value::Enum(p.source.value())),
            ("method", text("tools/call")),
            ("target_name", text(p.tool)),
            ("client_name", text(p.client)),
            ("error_class", Value::Enum(p.class.value())),
            ("caller_fault", Value::UInt(u64::from(p.caller_fault))),
            ("count", Value::UInt(p.count)),
            ("request_bytes", Value::UInt(10 * p.count)),
            ("response_bytes", Value::UInt(100 * p.count)),
            ("dur_hist", histogram(p.durations)),
            ("last_seen", Value::Time(minute.saturating_add_micros(1))),
        ],
    );
    world.store.insert(TableId::McpToolRollup, row);
}

fn deploy(world: &World, server: &str, id: &str, version: &str, source: CaptureSource, ago: i64) {
    let seen = NOW.saturating_add_micros(-ago * MINUTE);
    let row = stored_row(
        TableId::McpDeploys,
        &[
            ("project_id", Value::Uuid(project(1).uuid())),
            ("server_name", text(server)),
            ("deploy_id", text(id)),
            ("server_version", text(version)),
            ("capture_source", Value::Enum(source.value())),
            ("first_seen", Value::Time(seen)),
            ("last_seen", Value::Time(seen.saturating_add_micros(MINUTE))),
        ],
    );
    world.store.insert(TableId::McpDeploys, row);
}

fn listed(world: &World, server: &str, deploy: &str, tool: &str, hash: u64, ago: i64) {
    let seen = NOW.saturating_add_micros(-ago * MINUTE);
    let row = stored_row(
        TableId::McpToolInventory,
        &[
            ("project_id", Value::Uuid(project(1).uuid())),
            ("server_name", text(server)),
            ("deploy_id", text(deploy)),
            ("tool_name", text(tool)),
            ("schema_hash", Value::UInt(hash)),
            ("first_seen", Value::Time(seen)),
            ("last_seen", Value::Time(seen)),
        ],
    );
    world.store.insert(TableId::McpToolInventory, row);
}

fn query(side: McpSide, source: SourceChoice) -> McpQuery {
    McpQuery {
        window: hour(),
        side,
        source,
        include_caller_faults: false,
        server: None,
    }
}

/// Two servers: `files` captured by the SDK and the proxy (double capture), `db` by OTLP only.
fn double_capture(world: &World) {
    rollup(
        world,
        Partial {
            count: 5,
            durations: &[1_000; 5],
            ..Partial::default()
        },
    );
    rollup(
        world,
        Partial {
            class: ErrorClass::Validation,
            caller_fault: true,
            count: 2,
            durations: &[50, 50],
            ..Partial::default()
        },
    );
    rollup(
        world,
        Partial {
            class: ErrorClass::ToolExecution,
            count: 1,
            durations: &[9_000],
            client: "ide",
            ..Partial::default()
        },
    );
    // The proxy saw the same calls, and one more client.
    rollup(
        world,
        Partial {
            source: CaptureSource::ProxyStdio,
            count: 8,
            durations: &[2_000; 8],
            client: "other",
            ..Partial::default()
        },
    );
    rollup(
        world,
        Partial {
            server: "db",
            source: CaptureSource::Otlp,
            tool: "query",
            count: 3,
            durations: &[100, 200, 300],
            ..Partial::default()
        },
    );
    // A client-side row, which a server-side read never counts.
    rollup(
        world,
        Partial {
            side: McpSide::Client,
            count: 40,
            ..Partial::default()
        },
    );
}

#[test]
fn auto_reads_each_server_from_its_best_capture_source_and_never_sums_them() {
    let world = World::new(16);
    double_capture(&world);
    deploy(&world, "files", "v1", "1.0.0", CaptureSource::Sdk, 50);
    deploy(&world, "files", "v2", "1.1.0", CaptureSource::Sdk, 20);
    deploy(&world, "files", "v9", "9.9.9", CaptureSource::ProxyStdio, 5);
    let servers = run(world
        .service
        .mcp_servers(&ctx(1), &query(McpSide::Server, SourceChoice::Auto)))
    .unwrap();
    let files = servers
        .servers
        .iter()
        .find(|s| &*s.server_name == "files")
        .unwrap();
    // The SDK's six calls without the two caller faults; never the proxy's eight added in.
    assert_eq!(files.totals.calls, 6);
    assert_eq!(files.totals.errors, 1);
    assert_eq!(files.totals.caller_faults, 2);
    assert_eq!(
        files.clients, 2,
        "agent and ide; the proxy's client is not read"
    );
    // The latest deploy of the chosen class, not the proxy's.
    assert_eq!(files.version.as_deref(), Some("1.1.0"));
    assert_eq!(files.deploys, 2);
    let db = servers
        .servers
        .iter()
        .find(|s| &*s.server_name == "db")
        .unwrap();
    assert_eq!(db.totals.calls, 3);
    let chosen: Vec<(String, Option<CaptureClass>)> = servers
        .capture_sources
        .iter()
        .map(|s| (s.server_name.clone(), s.chosen))
        .collect();
    assert_eq!(
        chosen,
        [
            ("db".to_owned(), Some(CaptureClass::Otlp)),
            ("files".to_owned(), Some(CaptureClass::Sdk)),
        ]
    );
    assert_eq!(
        servers.servers[0].server_name.as_ref(),
        "files",
        "most calls first"
    );

    // A named class reads that class only, and the servers without it are not read.
    let proxy = run(world.service.mcp_servers(
        &ctx(1),
        &query(McpSide::Server, SourceChoice::Class(CaptureClass::Proxy)),
    ))
    .unwrap();
    assert_eq!(proxy.servers.len(), 1);
    assert_eq!(proxy.servers[0].totals.calls, 8);

    // Caller faults count when the request includes them.
    let mut with_faults = query(McpSide::Server, SourceChoice::Auto);
    with_faults.include_caller_faults = true;
    let servers = run(world.service.mcp_servers(&ctx(1), &with_faults)).unwrap();
    let files = servers
        .servers
        .iter()
        .find(|s| &*s.server_name == "files")
        .unwrap();
    assert_eq!(
        (
            files.totals.calls,
            files.totals.errors,
            files.totals.caller_faults
        ),
        (8, 3, 2)
    );

    // The client side is its own read.
    let client = run(world
        .service
        .mcp_servers(&ctx(1), &query(McpSide::Client, SourceChoice::Auto)))
    .unwrap();
    assert_eq!(client.servers[0].totals.calls, 40);
}

#[test]
fn tools_carry_their_schema_hash_and_the_unused_ones() {
    let world = World::new(16);
    double_capture(&world);
    listed(&world, "files", "v1", "read", 0xaa, 30);
    listed(&world, "files", "v2", "read", 0xbb, 20);
    listed(&world, "files", "v2", "write", 0xcc, 20);
    // Listed long before the window: not seen in it.
    listed(&world, "files", "v0", "delete", 0xdd, 600);
    let tools = run(world
        .service
        .mcp_tools(&ctx(1), &query(McpSide::Server, SourceChoice::Auto)))
    .unwrap();
    let summary: Vec<(&str, &str, u64, Option<u64>, bool)> = tools
        .tools
        .iter()
        .map(|t| {
            (
                &*t.server_name,
                &*t.tool_name,
                t.totals.calls,
                t.schema_hash,
                t.unused,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("files", "read", 6, Some(0xbb), false),
            ("db", "query", 3, None, false),
            ("files", "write", 0, Some(0xcc), true),
        ]
    );
    let read = &tools.tools[0];
    assert_eq!(read.totals.request_bytes, 60);
    let mut one = Histogram::new();
    one.record(1_000);
    assert_eq!(read.totals.latency.p50, one.quantile(500_000));
}

#[test]
fn one_tool_over_time_with_its_classes_clients_and_deploy_markers() {
    let world = World::new(16);
    double_capture(&world);
    rollup(
        &world,
        Partial {
            minutes_ago: 30,
            count: 4,
            durations: &[3_000; 4],
            ..Partial::default()
        },
    );
    deploy(&world, "files", "v2", "1.1.0", CaptureSource::Sdk, 25);
    deploy(&world, "db", "d1", "0.1.0", CaptureSource::Otlp, 25);
    let detail =
        run(world
            .service
            .mcp_tool(&ctx(1), &query(McpSide::Server, SourceChoice::Auto), "read"))
        .unwrap();
    assert_eq!(detail.totals.calls, 10);
    assert_eq!(detail.series.len(), 60, "every minute of the hour");
    let busy: Vec<(i64, u64)> = detail
        .series
        .iter()
        .filter(|p| p.totals.calls > 0)
        .map(|p| ((NOW.micros() - p.bucket.micros()) / MINUTE, p.totals.calls))
        .collect();
    assert_eq!(busy, [(30, 4), (10, 6)]);
    assert_eq!(detail.error_classes, [(ErrorClass::ToolExecution, 1)]);
    let clients: Vec<(&str, u64)> = detail
        .clients
        .iter()
        .map(|c| (&*c.client_name, c.totals.calls))
        .collect();
    assert_eq!(clients, [("agent", 9), ("ide", 1)]);
    // Only the tool's own server marks the series.
    let markers: Vec<&str> = detail.deploys.iter().map(|d| &*d.deploy_id).collect();
    assert_eq!(markers, ["v2"]);
}

#[test]
fn error_classes_and_the_breakdown_by_message() {
    let world = World::new(16);
    double_capture(&world);
    let classes = run(world
        .service
        .mcp_error_classes(&ctx(1), &query(McpSide::Server, SourceChoice::Auto)))
    .unwrap();
    assert_eq!(classes.totals.calls, 9);
    assert_eq!(classes.totals.caller_faults, 2);
    assert_eq!(classes.classes, [(ErrorClass::ToolExecution, 1)]);
    let mut with_faults = query(McpSide::Server, SourceChoice::Auto);
    with_faults.include_caller_faults = true;
    let classes = run(world.service.mcp_error_classes(&ctx(1), &with_faults)).unwrap();
    assert_eq!(
        classes.classes,
        [(ErrorClass::Validation, 2), (ErrorClass::ToolExecution, 1)]
    );

    // The breakdown reads the operations of the chosen sources.
    for (span, source, class, code, message) in [
        (
            1,
            CaptureSource::Sdk,
            ErrorClass::ToolExecution,
            None,
            "disk full",
        ),
        (
            2,
            CaptureSource::Sdk,
            ErrorClass::ToolExecution,
            None,
            "disk full",
        ),
        (
            3,
            CaptureSource::ProxyStdio,
            ErrorClass::Internal,
            Some(-32603),
            "boom",
        ),
        (
            4,
            CaptureSource::Sdk,
            ErrorClass::Validation,
            Some(-32602),
            "bad args",
        ),
    ] {
        operation(
            &world,
            Op {
                span,
                source,
                class,
                code,
                message,
                caller_fault: class == ErrorClass::Validation,
                ..Op::default()
            },
        );
    }
    let breakdown = run(world.service.mcp_error_breakdown(
        &ctx(1),
        &query(McpSide::Server, SourceChoice::Auto),
        50,
    ))
    .unwrap();
    let groups: Vec<(&str, u64)> = breakdown
        .groups
        .iter()
        .map(|g| (&*g.error_message, g.calls))
        .collect();
    assert_eq!(
        groups,
        [("disk full", 2)],
        "the proxy's rows and the caller fault are not read"
    );
    assert!(!breakdown.truncated);
}

/// One operation of `mcp_ops`.
struct Op<'a> {
    span: u64,
    parent: u64,
    side: McpSide,
    source: CaptureSource,
    class: ErrorClass,
    code: Option<i64>,
    message: &'a str,
    caller_fault: bool,
    seconds_ago: i64,
    user: &'a str,
    group: &'a str,
    client: &'a str,
    content: bool,
    state_in: [u8; 16],
    state_out: [u8; 16],
}

impl Default for Op<'_> {
    fn default() -> Self {
        Self {
            span: 1,
            parent: 0,
            side: McpSide::Server,
            source: CaptureSource::Sdk,
            class: ErrorClass::None,
            code: None,
            message: "",
            caller_fault: false,
            seconds_ago: 60,
            user: "",
            group: "",
            client: "agent",
            content: false,
            state_in: [0; 16],
            state_out: [0; 16],
        }
    }
}

fn trace_id() -> TraceId {
    TraceId::from_bytes([0x5a; 16])
}

fn operation(world: &World, op: Op<'_>) {
    let start = NOW.saturating_add_micros(-op.seconds_ago * 1_000_000);
    let trace = trace_id();
    let status = if op.class == ErrorClass::None {
        McpStatus::Ok
    } else {
        McpStatus::ToolError
    };
    let row = stored_row(
        TableId::McpOps,
        &[
            ("project_id", Value::Uuid(project(1).uuid())),
            ("side", Value::Enum(op.side.value())),
            ("capture_source", Value::Enum(op.source.value())),
            ("status", Value::Enum(status.value())),
            ("error_class", Value::Enum(op.class.value())),
            ("group_key_kind", Value::Enum(GroupKeyKind::Trace.value())),
            ("result_type", Value::Enum(ResultType::Complete.value())),
            ("start_minute", Value::Time(start.floor_minute())),
            ("trace_hash", Value::UInt(u64::from(trace.trace_hash()))),
            ("start_time", Value::Time(start)),
            ("span_id", Value::Bytes(op.span.to_be_bytes().to_vec())),
            (
                "parent_span_id",
                Value::Bytes(op.parent.to_be_bytes().to_vec()),
            ),
            ("trace_id", Value::Bytes(trace.to_bytes().to_vec())),
            ("mrtr_state_in", Value::Bytes(op.state_in.to_vec())),
            ("mrtr_state_out", Value::Bytes(op.state_out.to_vec())),
            ("server_name", text("files")),
            ("method", text("tools/call")),
            ("target_name", text("read")),
            ("client_name", text(op.client)),
            ("error_message", text(op.message)),
            ("user_id", text(op.user)),
            ("group_key", text(op.group)),
            ("caller_fault", Value::UInt(u64::from(op.caller_fault))),
            ("content_captured", Value::UInt(u64::from(op.content))),
            ("duration_us", Value::UInt(1_000 + op.span)),
            ("jsonrpc_code", op.code.map_or(Value::Null, Value::Int)),
            ("received_at", Value::Time(start)),
        ],
    );
    world.store.insert(TableId::McpOps, row);
    if op.content {
        let content = stored_row(
            TableId::McpContent,
            &[
                ("project_id", Value::Uuid(project(1).uuid())),
                ("side", Value::Enum(op.side.value())),
                ("start_minute", Value::Time(start.floor_minute())),
                ("trace_hash", Value::UInt(u64::from(trace.trace_hash()))),
                ("span_id", Value::Bytes(op.span.to_be_bytes().to_vec())),
                ("trace_id", Value::Bytes(trace.to_bytes().to_vec())),
                ("start_time", Value::Time(start)),
                ("args", text("{\"path\":\"/tmp\"}")),
                ("result", text("ok")),
                ("received_at", Value::Time(start)),
            ],
        );
        world.store.insert(TableId::McpContent, content);
    }
}

#[test]
fn users_and_sessions_count_their_calls_and_errors() {
    let world = World::new(16);
    rollup(&world, Partial::default());
    for (span, user, group, class) in [
        (1, "alice", "g1", ErrorClass::None),
        (2, "alice", "g2", ErrorClass::ToolExecution),
        (3, "bob", "g1", ErrorClass::None),
        (4, "", "", ErrorClass::None),
    ] {
        operation(
            &world,
            Op {
                span,
                user,
                group,
                class,
                seconds_ago: 100 - span as i64,
                ..Op::default()
            },
        );
    }
    let users =
        run(world
            .service
            .mcp_users(&ctx(1), &query(McpSide::Server, SourceChoice::Auto), 50))
        .unwrap();
    let summary: Vec<(&str, u64, u64, u64)> = users
        .users
        .iter()
        .map(|u| (u.key.user_id.as_str(), u.calls, u.errors, u.sessions))
        .collect();
    assert_eq!(
        summary,
        [("bob", 1, 0, 1), ("alice", 2, 1, 2)],
        "last seen first"
    );
    let sessions =
        run(world
            .service
            .mcp_sessions(&ctx(1), &query(McpSide::Server, SourceChoice::Auto), 1))
        .unwrap();
    assert_eq!(sessions.sessions.len(), 1);
    assert!(sessions.truncated);
    assert_eq!(&*sessions.sessions[0].group_key, "g1");
    assert_eq!(sessions.sessions[0].calls, 2);
}

#[test]
fn the_operation_list_pages_newest_first_under_its_filters() {
    let world = World::new(16);
    rollup(&world, Partial::default());
    for span in 1..=5u64 {
        let class = if span.is_multiple_of(2) {
            ErrorClass::ToolExecution
        } else {
            ErrorClass::None
        };
        operation(
            &world,
            Op {
                span,
                class,
                seconds_ago: 100 - span as i64,
                ..Op::default()
            },
        );
    }
    let page = |cursor: Option<McpOperationCursor>, filter: McpOpsFilter| McpOperationsQuery {
        query: query(McpSide::Server, SourceChoice::Auto),
        filter,
        limit: 2,
        cursor,
    };
    let first = run(world
        .service
        .mcp_operations(&ctx(1), &page(None, McpOpsFilter::default())))
    .unwrap();
    let spans = |ops: &[crucible_query::McpOperationRow]| -> Vec<SpanId> {
        ops.iter().map(|o| o.span_id).collect()
    };
    let id = |n: u64| SpanId::from_bytes(n.to_be_bytes());
    assert_eq!(spans(&first.operations), [id(5), id(4)]);
    let next = first.next.unwrap();
    assert_eq!(next.as_of, NOW);
    // A row that arrives after the first page is not on the later ones.
    world.clock.advance(std::time::Duration::from_secs(5));
    operation(
        &world,
        Op {
            span: 9,
            seconds_ago: -1,
            ..Op::default()
        },
    );
    let second = run(world
        .service
        .mcp_operations(&ctx(1), &page(Some(next), McpOpsFilter::default())))
    .unwrap();
    assert_eq!(spans(&second.operations), [id(3), id(2)]);
    assert_eq!(second.meta.as_of, NOW);
    let failed = McpOpsFilter {
        errors_only: true,
        ..McpOpsFilter::default()
    };
    let errors = run(world.service.mcp_operations(&ctx(1), &page(None, failed))).unwrap();
    assert!(
        errors
            .operations
            .iter()
            .all(|o| o.error_class != ErrorClass::None)
    );
}

#[test]
fn one_operation_with_its_content_other_side_and_chain() {
    let world = World::new(16);
    rollup(&world, Partial::default());
    let state = |n: u8| [n; 16];
    // The agent's call (client side), the server's two rounds under it, and the proxy's copy of round one.
    operation(
        &world,
        Op {
            span: 10,
            side: McpSide::Client,
            ..Op::default()
        },
    );
    operation(
        &world,
        Op {
            span: 11,
            parent: 10,
            content: true,
            state_out: state(1),
            seconds_ago: 59,
            ..Op::default()
        },
    );
    operation(
        &world,
        Op {
            span: 12,
            parent: 10,
            state_in: state(1),
            seconds_ago: 58,
            ..Op::default()
        },
    );
    operation(
        &world,
        Op {
            span: 13,
            parent: 10,
            source: CaptureSource::ProxyStdio,
            seconds_ago: 59,
            ..Op::default()
        },
    );
    let id = |n: u64| SpanId::from_bytes(n.to_be_bytes());
    let detail = run(world.service.mcp_operation(
        &ctx(1),
        &query(McpSide::Server, SourceChoice::Auto),
        (trace_id(), id(11)),
    ))
    .unwrap();
    let op = detail.operation.unwrap();
    assert_eq!(op.row.span_id, id(11));
    let args = op.args.unwrap();
    assert_eq!((&*args.preview, args.bytes), ("{\"path\":\"/tmp\"}", 15));
    assert_eq!(op.other_side.unwrap().span_id, id(10));
    let chain: Vec<(SpanId, u16)> = op.chain.iter().map(|l| (l.span_id, l.round)).collect();
    assert_eq!(chain, [(id(11), 1), (id(12), 2)]);
    // From the client side, the server's row is the other side, the SDK's before the proxy's.
    let client = run(world.service.mcp_operation(
        &ctx(1),
        &query(McpSide::Client, SourceChoice::Auto),
        (trace_id(), id(10)),
    ))
    .unwrap();
    let other = client.operation.unwrap().other_side.unwrap();
    assert_eq!(other.capture_source, CaptureSource::Sdk);
    // An operation that does not exist, in a whole answer, is not found.
    let missing = run(world.service.mcp_operation(
        &ctx(1),
        &query(McpSide::Server, SourceChoice::Auto),
        (trace_id(), id(99)),
    ));
    assert_eq!(missing, Err(QueryServiceError::NotFound));
}

#[test]
fn compare_two_deploys_of_a_server() {
    let world = World::new(16);
    rollup(
        &world,
        Partial {
            deploy: "v1",
            count: 4,
            durations: &[1_000; 4],
            ..Partial::default()
        },
    );
    rollup(
        &world,
        Partial {
            deploy: "v2",
            count: 2,
            class: ErrorClass::Timeout,
            durations: &[8_000; 2],
            ..Partial::default()
        },
    );
    listed(&world, "files", "v1", "read", 1, 30);
    listed(&world, "files", "v1", "old", 2, 30);
    listed(&world, "files", "v2", "read", 9, 20);
    listed(&world, "files", "v2", "new", 3, 20);
    let mut q = query(McpSide::Server, SourceChoice::Auto);
    q.server = Some("files".to_owned());
    let compare = McpCompareQuery {
        query: q,
        baseline: CompareSide::Deploy("v1".to_owned()),
        candidate: CompareSide::Deploy("v2".to_owned()),
    };
    let found = run(world.service.mcp_compare(&ctx(1), &compare)).unwrap();
    assert_eq!(found.tools.len(), 1);
    assert_eq!(
        (
            found.tools[0].baseline.calls,
            found.tools[0].candidate.calls
        ),
        (4, 2)
    );
    assert_eq!(found.tools[0].candidate.errors, 2);
    let names =
        |list: &[std::sync::Arc<str>]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(names(&found.added), ["new"]);
    assert_eq!(names(&found.removed), ["old"]);
    assert_eq!(names(&found.changed), ["read"]);
}

#[test]
fn the_mcp_ops_metrics_source_reads_one_capture_source_per_server() {
    let world = World::new(16);
    double_capture(&world);
    let request = |measure: MeasureId, aggregation: Aggregation| McpMetricsRequest {
        window: hour(),
        query: MetricsQuery {
            source: Source::McpOps,
            measures: vec![(measure, aggregation)],
            dimensions: vec![DimensionId::McpServer],
            filter: Filter::all(),
            granularity: Granularity::Auto,
            fill: Fill::None,
            top_n: None,
        },
        side: McpSide::Server,
        source: SourceChoice::Auto,
        include_caller_faults: false,
    };
    let total = |result: &crucible_query::MetricsResult, server: &str| -> Option<u64> {
        let series = result
            .series
            .iter()
            .find(|s| s.key.first().map(|k| &**k) == Some(server))?;
        match series.points.first()?.value? {
            MetricValue::UInt(v) => Some(v),
            MetricValue::Int(_) => None,
        }
    };
    let calls = run(world
        .service
        .mcp_metrics(&ctx(1), request(MeasureId::McpCalls, Aggregation::Sum)))
    .unwrap();
    assert_eq!(
        total(&calls, "files"),
        Some(6),
        "the SDK's calls, caller faults out"
    );
    assert_eq!(total(&calls, "db"), Some(3));
    let rate = run(world
        .service
        .mcp_metrics(&ctx(1), request(MeasureId::McpErrorRate, Aggregation::Rate)))
    .unwrap();
    // One failed call of six.
    assert_eq!(total(&rate, "files"), Some(166_666));
    assert_eq!(total(&rate, "db"), Some(0));
    let errors = run(world
        .service
        .mcp_metrics(&ctx(1), request(MeasureId::McpErrorCount, Aggregation::Sum)))
    .unwrap();
    assert_eq!(total(&errors, "files"), Some(1));
    let p99 = run(world.service.mcp_metrics(
        &ctx(1),
        request(MeasureId::McpDurationP99, Aggregation::P99),
    ))
    .unwrap();
    assert!(total(&p99, "files").is_some_and(|v| v >= 8_700));
}

#[test]
fn an_alert_reads_one_total_per_server_from_one_capture_source() {
    let world = World::new(16);
    double_capture(&world);
    let request =
        |measure: MeasureId, aggregation: Aggregation, source: SourceChoice| McpMetricsRequest {
            window: hour(),
            query: MetricsQuery {
                source: Source::McpOps,
                measures: vec![(measure, aggregation)],
                dimensions: vec![DimensionId::McpServer],
                filter: Filter::all(),
                granularity: Granularity::Auto,
                fill: Fill::None,
                top_n: None,
            },
            side: McpSide::Server,
            source,
            include_caller_faults: false,
        };
    let totals =
        |(measure, aggregation): (MeasureId, Aggregation), source, largest_first, limit| {
            run(world.service.mcp_metrics_totals(
                &ctx(1),
                request(measure, aggregation, source),
                largest_first,
                limit,
            ))
            .unwrap()
        };
    let values = |totals: &MetricsTotals| -> Vec<(String, Option<u64>)> {
        totals
            .series
            .iter()
            .map(|s| {
                let value = match s.value {
                    Some(MetricValue::UInt(v)) => Some(v),
                    _ => None,
                };
                (s.key.join(","), value)
            })
            .collect()
    };
    let owned = |rows: &[(&str, Option<u64>)]| -> Vec<(String, Option<u64>)> {
        rows.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    };
    let auto = SourceChoice::Auto;
    // The calls of `files` come from its SDK alone, six without the caller faults; the most first.
    let calls = totals((MeasureId::McpCalls, Aggregation::Sum), auto, true, 50);
    assert_eq!(
        values(&calls),
        owned(&[("files", Some(6)), ("db", Some(3))])
    );
    assert!(!calls.truncated);
    // A server with calls and no error reads zero rather than nothing, and the keys nearest to a breach are
    // kept: the largest rate first, or with a downward comparison the smallest count.
    let rate = totals((MeasureId::McpErrorRate, Aggregation::Rate), auto, true, 50);
    assert_eq!(
        values(&rate),
        owned(&[("files", Some(166_666)), ("db", Some(0))])
    );
    let errors = totals((MeasureId::McpErrorCount, Aggregation::Sum), auto, false, 1);
    assert_eq!(values(&errors), owned(&[("db", Some(0))]));
    assert!(errors.truncated);
    // Under the proxy, `files` is read from the proxy's rows only, and `db`, which has none, is not read.
    let proxy = SourceChoice::Class(CaptureClass::Proxy);
    let calls = totals((MeasureId::McpCalls, Aggregation::Sum), proxy, true, 50);
    assert_eq!(values(&calls), owned(&[("files", Some(8))]));
    // A percentile keeps the busiest keys.
    let p99 = totals((MeasureId::McpDurationP99, Aggregation::P99), auto, true, 1);
    assert_eq!(p99.series.len(), 1);
    assert!(p99.series[0].key.join(",") == "files" && p99.truncated);
}

#[test]
fn an_unavailable_store_fails_the_read() {
    let world = World::new(16);
    double_capture(&world);
    world.store.set_unavailable(true);
    let found = run(world
        .service
        .mcp_servers(&ctx(1), &query(McpSide::Server, SourceChoice::Auto)));
    assert!(
        matches!(found, Err(QueryServiceError::Store(_))),
        "{found:?}"
    );
    world.store.set_unavailable(false);
    // Another project sees nothing of project 1.
    let other = run(world
        .service
        .mcp_servers(&ctx(2), &query(McpSide::Server, SourceChoice::Auto)))
    .unwrap();
    assert!(other.servers.is_empty() && other.capture_sources.is_empty());
}
