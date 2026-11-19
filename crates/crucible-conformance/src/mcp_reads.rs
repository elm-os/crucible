//! The differential check of the store model for the MCP analytics plans (contract 9.9): every builder of
//! `crucible-plan`'s MCP endpoints runs on the server and on `crucible_sim::ch_model::ChModel` over one fixture
//! of the five MCP tables, before and after the model merges its parts, and the answers must agree and equal
//! what the rows say. The fixture has one server captured by the SDK and the proxy (double capture), one by
//! OTLP only, both sides, a caller fault, two deploys, an inventory, operations with a parent link and a
//! multi-round chain, and captured content.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_plan::{
    Caller, CaptureClass, McpContentField, McpOperationsRequest, McpOpsFilter, McpRead,
    McpSelection, McpUserKey, QueryPlan, RetentionClamp, Row, SourceChoice, Step, Value,
    mcp_client_versions, mcp_clients, mcp_compare_tools, mcp_deploys, mcp_error_breakdown,
    mcp_error_classes, mcp_inventory, mcp_operation, mcp_operation_chain, mcp_operation_content,
    mcp_operation_peers, mcp_operations, mcp_presence, mcp_servers, mcp_session_errors,
    mcp_sessions, mcp_tool_clients, mcp_tool_series, mcp_tools, mcp_user_errors, mcp_users,
};
use crucible_records::{
    CaptureSource, ErrorClass, GroupKeyKind, McpSide, ProjectScope, SpanId, TableId, TimeWindow,
    Timestamp, TraceId,
};
use crucible_schema::RetentionSettings;

use crate::blocks::Rows;
use crate::differential::{insert, rows, stamped};
use crate::inserts::plain_rows;
use crate::reads::rollup;
use crate::{Ctx, T0, expect};

const HOUR: i64 = 3_600_000_000;
const SALT: u32 = 91;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn histogram(bucket: u64, count: u64) -> Value {
    let array = |v: u64| Value::Array(vec![Value::UInt(v)]);
    Value::Tuple2(Box::new(array(bucket)), Box::new(array(count)))
}

fn empty_histogram() -> Value {
    Value::Tuple2(
        Box::new(Value::Array(Vec::new())),
        Box::new(Value::Array(Vec::new())),
    )
}

/// One data row of the tool rollup.
#[derive(Clone, Copy)]
struct Tool {
    side: McpSide,
    server: &'static str,
    deploy: &'static str,
    source: CaptureSource,
    target: &'static str,
    client: &'static str,
    class: ErrorClass,
    fault: u64,
    count: u64,
}

const fn tool(server: &'static str, source: CaptureSource, class: ErrorClass, count: u64) -> Tool {
    Tool {
        side: McpSide::Server,
        server,
        deploy: "v1",
        source,
        target: "read",
        client: "agent",
        class,
        fault: 0,
        count,
    }
}

const TOOLS: [Tool; 7] = [
    tool("files", CaptureSource::Sdk, ErrorClass::None, 5),
    Tool {
        fault: 1,
        ..tool("files", CaptureSource::Sdk, ErrorClass::Validation, 2)
    },
    Tool {
        client: "ide",
        ..tool("files", CaptureSource::Sdk, ErrorClass::ToolExecution, 1)
    },
    tool("files", CaptureSource::ProxyStdio, ErrorClass::None, 8),
    Tool {
        target: "query",
        ..tool("db", CaptureSource::Otlp, ErrorClass::None, 3)
    },
    Tool {
        side: McpSide::Client,
        ..tool("files", CaptureSource::Sdk, ErrorClass::None, 40)
    },
    Tool {
        deploy: "v2",
        ..tool("files", CaptureSource::Sdk, ErrorClass::None, 4)
    },
];

/// One operation of `mcp_ops`: the span, its parent, its side, its capture source, its user and session, its
/// error class, and its multi-round state in and out.
type Op = (u64, u64, McpSide, CaptureSource, &'static str, ErrorClass, u8, u8);

const OPS: [Op; 5] = [
    (1, 0, McpSide::Client, CaptureSource::Sdk, "alice", ErrorClass::None, 0, 0),
    (2, 1, McpSide::Server, CaptureSource::Sdk, "alice", ErrorClass::None, 0, 7),
    (3, 1, McpSide::Server, CaptureSource::Sdk, "alice", ErrorClass::ToolExecution, 7, 0),
    (4, 1, McpSide::Server, CaptureSource::ProxyStdio, "bob", ErrorClass::None, 0, 0),
    (5, 0, McpSide::Server, CaptureSource::Sdk, "bob", ErrorClass::Internal, 0, 0),
];

fn trace() -> TraceId {
    TraceId::from_bytes([0x3c; 16])
}

fn span(n: u64) -> SpanId {
    SpanId::from_bytes(n.to_be_bytes())
}

async fn load(ctx: &Ctx) -> Result<ProjectScope> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let mut seq = 500;
    let mut put = async |rows: Rows, time: Timestamp| -> Result<()> {
        seq += 1;
        let table = rows.table();
        let rows = stamped(rows, epoch, seq);
        insert(ctx, rows, ctx.token(epoch, seq, table, time)?)
            .await
            .with_context(|| format!("MCP analytics rows of {}", table.name()))
    };
    let data = |row: usize| TOOLS.get(row).copied();
    let tools = rollup(TableId::McpToolRollup, project, epoch, 0, TOOLS.len())
        .each("side", |row| Value::Enum(data(row).map_or(McpSide::Server, |t| t.side).value()))
        .all("minute", Value::Time(T0.floor_minute()))
        .each("server_name", |row| text(data(row).map_or("", |t| t.server)))
        .each("deploy_id", |row| text(data(row).map_or("", |t| t.deploy)))
        .each("capture_source", |row| {
            Value::Enum(data(row).map_or(CaptureSource::Sdk, |t| t.source).value())
        })
        .each("method", |row| text(if data(row).is_some() { "tools/call" } else { "" }))
        .each("target_name", |row| text(data(row).map_or("", |t| t.target)))
        .each("client_name", |row| text(data(row).map_or("", |t| t.client)))
        .each("error_class", |row| {
            Value::Enum(data(row).map_or(ErrorClass::None, |t| t.class).value())
        })
        .each("caller_fault", |row| Value::UInt(data(row).map_or(0, |t| t.fault)))
        .each("count", |row| Value::UInt(data(row).map_or(0, |t| t.count)))
        .each("request_bytes", |row| Value::UInt(data(row).map_or(0, |t| 10 * t.count)))
        .each("response_bytes", |row| Value::UInt(data(row).map_or(0, |t| 100 * t.count)))
        .each("dur_hist", |row| match data(row) {
            Some(t) => histogram(100 + row as u64, t.count),
            None => empty_histogram(),
        })
        .each("semconv_hist", |_| empty_histogram())
        .each("last_seen", |row| {
            Value::Time(if data(row).is_some() { at(row as i64) } else { Timestamp::EPOCH })
        });
    put(tools, T0).await?;

    let deploys_data = [
        ("v1", "1.0.0", CaptureSource::Sdk, -HOUR / 2),
        ("v2", "1.1.0", CaptureSource::Sdk, -HOUR / 4),
        ("v9", "9.9.9", CaptureSource::ProxyStdio, -HOUR / 8),
    ];
    let d = |row: usize| deploys_data.get(row).copied();
    let deploys = rollup(TableId::McpDeploys, project, epoch, 0, deploys_data.len())
        .each("server_name", |row| text(if d(row).is_some() { "files" } else { "" }))
        .each("deploy_id", |row| text(d(row).map_or("", |x| x.0)))
        .each("server_version", |row| text(d(row).map_or("", |x| x.1)))
        .each("capture_source", |row| {
            Value::Enum(d(row).map_or(CaptureSource::Sdk, |x| x.2).value())
        })
        .each("first_seen", |row| Value::Time(d(row).map_or(Timestamp::EPOCH, |x| at(x.3))))
        .each("last_seen", |row| {
            Value::Time(d(row).map_or(Timestamp::EPOCH, |x| at(x.3 + 60_000_000)))
        });
    put(deploys, T0).await?;

    let listed = [("v1", "read", 1u64), ("v2", "read", 2), ("v2", "write", 3)];
    let l = |row: usize| listed.get(row).copied();
    let inventory = rollup(TableId::McpToolInventory, project, epoch, 0, listed.len())
        .each("server_name", |row| text(if l(row).is_some() { "files" } else { "" }))
        .each("deploy_id", |row| text(l(row).map_or("", |x| x.0)))
        .each("tool_name", |row| text(l(row).map_or("", |x| x.1)))
        .each("schema_hash", |row| Value::UInt(l(row).map_or(0, |x| x.2)))
        .each("first_seen", |row| {
            Value::Time(if l(row).is_some() { at(-HOUR / 2) } else { Timestamp::EPOCH })
        })
        .each("last_seen", |row| Value::Time(if l(row).is_some() { T0 } else { Timestamp::EPOCH }))
        .each("capture_sources", |_| Value::Array(Vec::new()));
    put(inventory, T0).await?;

    let o = |row: usize| OPS.get(row).copied();
    let state = |byte: u8| Value::Bytes(vec![byte; 16]);
    let ops = plain_rows(TableId::McpOps, project, epoch, 900, T0, OPS.len(), SALT)
        .each("span_id", |row| Value::Bytes(o(row).map_or(0, |x| x.0).to_be_bytes().to_vec()))
        .each("parent_span_id", |row| {
            Value::Bytes(o(row).map_or(0, |x| x.1).to_be_bytes().to_vec())
        })
        .each("start_time", |row| Value::Time(at(row as i64 * 1_000)))
        .all("trace_id", Value::Bytes(trace().to_bytes().to_vec()))
        .all("trace_hash", Value::UInt(u64::from(trace().trace_hash())))
        .each("side", |row| Value::Enum(o(row).map_or(McpSide::Server, |x| x.2).value()))
        .each("capture_source", |row| {
            Value::Enum(o(row).map_or(CaptureSource::Sdk, |x| x.3).value())
        })
        .all("server_name", text("files"))
        .all("method", text("tools/call"))
        .all("target_name", text("read"))
        .each("client_name", |row| text(if row % 2 == 0 { "agent" } else { "ide" }))
        .each("client_version", |row| text(if row < 3 { "2.0" } else { "2.1" }))
        .each("user_id", |row| text(o(row).map_or("", |x| x.4)))
        .each("group_key", |row| text(if row < 4 { "g1" } else { "g2" }))
        .all("group_key_kind", Value::Enum(GroupKeyKind::Trace.value()))
        .each("error_class", |row| {
            Value::Enum(o(row).map_or(ErrorClass::None, |x| x.5).value())
        })
        .each("error_message", |row| text(if row == 2 { "disk full" } else { "" }))
        .each("jsonrpc_code", |row| if row == 4 { Value::Int(-32603) } else { Value::Null })
        .each("mrtr_state_in", |row| state(o(row).map_or(0, |x| x.6)))
        .each("mrtr_state_out", |row| state(o(row).map_or(0, |x| x.7)))
        .each("content_captured", |row| Value::UInt(u64::from(row == 1)))
        .each("duration_us", |row| Value::UInt(1_000 + row as u64));
    put(ops, T0).await?;
    let content = plain_rows(TableId::McpContent, project, epoch, 901, at(1_000), 1, SALT)
        .all("side", Value::Enum(McpSide::Server.value()))
        .all("span_id", Value::Bytes(span(2).to_bytes().to_vec()))
        .all("trace_id", Value::Bytes(trace().to_bytes().to_vec()))
        .all("trace_hash", Value::UInt(u64::from(trace().trace_hash())))
        .all("args", text("{\"path\":\"/tmp/a\"}"))
        .all("result", text("ok"))
        .all("args_bytes", Value::UInt(16))
        .all("result_bytes", Value::UInt(2));
    put(content, T0).await?;
    scope(project).map_err(|e| anyhow!("{e}"))
}

fn live(plan: Result<Option<QueryPlan>, crucible_plan::PlanError>) -> Result<QueryPlan> {
    plan?.context("the builder returned no plan")
}

fn sum_of(found: &[Row], at: usize) -> u64 {
    found
        .iter()
        .filter_map(|row| match row.values().get(at) {
            Some(Value::UInt(v)) => Some(*v),
            _ => None,
        })
        .sum()
}

/// Runs every MCP analytics builder on the server and on the model; returns how many plans agreed.
pub(crate) async fn mcp_reads(ctx: &Ctx) -> Result<String> {
    let scope = load(ctx).await?;
    let window = TimeWindow::new(at(-HOUR), at(2 * HOUR)).map_err(|e| anyhow!("{e}"))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let caller = Caller::Session;
    let mut plans = 0;
    for merged in [false, true] {
        if merged {
            ctx.model.merge_all();
        }
        let when = if merged { "after" } else { "before" };
        // The presence of each server's capture sources, and the selection under auto.
        let presence = rows(ctx, live(mcp_presence(scope, window, &clamp, caller, McpSide::Server, None))?).await?;
        let pairs = presence
            .iter()
            .filter_map(|row| match (row.values().first(), row.values().get(1)) {
                (Some(Value::String(server)), Some(Value::Enum(source))) => {
                    Some((server.clone(), CaptureSource::from_value(*source)?))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        expect(pairs.len() == 3, || format!("the presence {when} the merge: {pairs:?}"))?;
        let selection = McpSelection::select(SourceChoice::Auto, pairs.clone());
        expect(
            selection.chosen("files") == Some(CaptureClass::Sdk) && selection.chosen("db") == Some(CaptureClass::Otlp),
            || format!("the selection {when} the merge: {selection:?}"),
        )?;
        let read = McpRead { side: McpSide::Server, selection: &selection, include_caller_faults: false, server: None };
        let files = McpRead { server: Some("files"), ..read };

        // Servers: the SDK's rows of files and the OTLP rows of db, never the proxy's.
        let servers = rows(ctx, live(mcp_servers(scope, window, &clamp, caller, &read))?).await?;
        expect(sum_of(&servers, 4) == 5 + 2 + 1 + 4 + 3, || {
            format!("the servers' calls {when} the merge: {servers:?}")
        })?;
        let tools = rows(ctx, live(mcp_tools(scope, window, &clamp, caller, &read))?).await?;
        expect(sum_of(&tools, 4) == 15, || format!("the tools {when} the merge: {tools:?}"))?;
        let series = rows(ctx, live(mcp_tool_series(scope, window, &clamp, caller, &files, "read", Step::Minute))?).await?;
        expect(sum_of(&series, 3) == 12, || format!("the tool series {when} the merge: {series:?}"))?;
        let clients = rows(ctx, live(mcp_tool_clients(scope, window, &clamp, caller, &files, "read"))?).await?;
        expect(sum_of(&clients, 4) == 12, || format!("the tool clients {when} the merge: {clients:?}"))?;
        let classes = rows(ctx, live(mcp_error_classes(scope, window, &clamp, caller, &read))?).await?;
        expect(sum_of(&classes, 4) == 15, || format!("the error classes {when} the merge: {classes:?}"))?;
        let by_client = rows(ctx, live(mcp_clients(scope, window, &clamp, caller, &read))?).await?;
        expect(sum_of(&by_client, 3) == 15, || format!("the clients {when} the merge: {by_client:?}"))?;
        let compare = rows(
            ctx,
            live(mcp_compare_tools(scope, window, &clamp, caller, &files, &["v1".to_owned(), "v2".to_owned()]))?,
        )
        .await?;
        expect(sum_of(&compare, 4) == 12, || format!("the compare {when} the merge: {compare:?}"))?;
        let deploys = rows(ctx, live(mcp_deploys(scope, window, &clamp, caller, Some("files")))?).await?;
        expect(deploys.len() == 3, || format!("the deploys {when} the merge: {deploys:?}"))?;
        let inventory = rows(ctx, live(mcp_inventory(scope, window, &clamp, caller, Some("files"), &[]))?).await?;
        expect(inventory.len() == 3, || format!("the inventory {when} the merge: {inventory:?}"))?;
        plans += 11;

        // The raw rows: the SDK's operations of files, without caller faults.
        let breakdown = rows(ctx, live(mcp_error_breakdown(scope, window, &clamp, caller, &read, 10))?).await?;
        expect(breakdown.len() == 2, || format!("the error breakdown {when} the merge: {breakdown:?}"))?;
        let versions = rows(ctx, live(mcp_client_versions(scope, window, &clamp, caller, &read, 10))?).await?;
        expect(sum_of(&versions, 2) == 3, || format!("the client versions {when} the merge: {versions:?}"))?;
        let users = rows(ctx, live(mcp_users(scope, window, &clamp, caller, &read, 10))?).await?;
        expect(users.len() == 2, || format!("the users {when} the merge: {users:?}"))?;
        let keys = [McpUserKey { user_id: "alice".to_owned(), subject_hash: [0; 16] }];
        let errors = rows(ctx, live(mcp_user_errors(scope, window, &clamp, caller, &read, &keys))?).await?;
        expect(sum_of(&errors, 2) == 1, || format!("the user errors {when} the merge: {errors:?}"))?;
        let sessions = rows(ctx, live(mcp_sessions(scope, window, &clamp, caller, &read, 10))?).await?;
        expect(sessions.len() == 2, || format!("the sessions {when} the merge: {sessions:?}"))?;
        let session_keys = ["g1".to_owned(), "g2".to_owned()];
        let errors = rows(ctx, live(mcp_session_errors(scope, window, &clamp, caller, &read, &session_keys))?).await?;
        expect(sum_of(&errors, 2) == 2, || format!("the session errors {when} the merge: {errors:?}"))?;
        let page = McpOperationsRequest { filter: McpOpsFilter::default(), limit: 2, cursor: None, as_of: at(HOUR) };
        let first = rows(ctx, live(mcp_operations(scope, window, &clamp, caller, &read, &page))?).await?;
        expect(first.len() == 3, || format!("the first operations {when} the merge: {first:?}"))?;
        let later = McpOperationsRequest { cursor: Some((at(2_000), span(3))), ..page };
        let rest = rows(ctx, live(mcp_operations(scope, window, &clamp, caller, &read, &later))?).await?;
        expect(rest.len() == 1, || format!("the later operations {when} the merge: {rest:?}"))?;
        let point = (trace(), span(2));
        let one = rows(ctx, live(mcp_operation(scope, window, &clamp, caller, McpSide::Server, point))?).await?;
        expect(one.len() == 1, || format!("the operation {when} the merge: {one:?}"))?;
        let key = (McpSide::Server, trace(), span(2));
        let args = rows(ctx, live(mcp_operation_content(scope, window, &clamp, caller, key, McpContentField::Args, 6))?).await?;
        expect(
            args.first().map(|r| r.values().to_vec())
                == Some(vec![Value::String("{\"path".to_owned()), Value::UInt(17)]),
            || format!("the operation's arguments {when} the merge: {args:?}"),
        )?;
        let peers = rows(
            ctx,
            live(mcp_operation_peers(scope, window, &clamp, caller, McpSide::Client, (trace(), span(2), span(1))))?,
        )
        .await?;
        expect(peers.len() == 1, || format!("the other side {when} the merge: {peers:?}"))?;
        let chain = rows(
            ctx,
            live(mcp_operation_chain(scope, window, &clamp, caller, McpSide::Server, (trace(), CaptureSource::Sdk)))?,
        )
        .await?;
        expect(chain.len() == 2, || format!("the chain {when} the merge: {chain:?}"))?;
        plans += 14;
    }
    let violations = ctx.model.violations();
    expect(violations.is_empty(), || format!("the model recorded {violations:?}"))?;
    Ok(format!(
        "{plans} plans of the MCP analytics builders (presence, servers, tools, one tool, error classes and \
         breakdown, clients and versions, users, sessions, operations, one operation with its content, other \
         side and chain, deploys, inventory, compare) returned the same rows on the server and on the store \
         model, before and after the model merged its parts, with the SDK's rows of a doubly captured server \
         read and the proxy's never added in"
    ))
}
