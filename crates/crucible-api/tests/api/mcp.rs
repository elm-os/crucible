//! The MCP analytics routes (contract 9.9 and 12.6): every route answers with `capture_sources` and `meta`,
//! refuses a missing or invalid parameter by name, pages the operation list with a cursor bound to its
//! parameters, keeps projects apart, and the metrics query reads the `mcp_ops` source with a side.

use axum::http::{Method, StatusCode};
use crucible_plan::Value;
use crucible_records::{CaptureSource, ErrorClass, GroupKeyKind, McpSide, TableId, Timestamp, TraceId};
use serde_json::json;

use crate::store::stored_row;
use crate::world::{NOW, World, project};
use crate::{api, get, other_key, path, post, run, send};

const MINUTE: i64 = 60_000_000;

fn ago(minutes: i64) -> Timestamp {
    NOW.saturating_add_micros(-minutes * MINUTE)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn histogram(bucket: u64, count: u64) -> Value {
    Value::Tuple2(
        Box::new(Value::Array(vec![Value::UInt(bucket)])),
        Box::new(Value::Array(vec![Value::UInt(count)])),
    )
}

fn trace() -> TraceId {
    TraceId::from_bytes([0x5a; 16])
}

/// A server with SDK and proxy rows, an OTLP-only server, a deploy, an inventory entry and two operations.
fn world() -> World {
    let world = World::new(10);
    let project = Value::Uuid(project(1).uuid());
    for (server, source, class, fault, count) in [
        ("files", CaptureSource::Sdk, ErrorClass::None, 0, 5u64),
        ("files", CaptureSource::Sdk, ErrorClass::Validation, 1, 2),
        ("files", CaptureSource::ProxyHttp, ErrorClass::None, 0, 9),
        ("db", CaptureSource::Otlp, ErrorClass::Timeout, 0, 1),
    ] {
        let row = stored_row(
            TableId::McpToolRollup,
            &[
                ("project_id", project.clone()),
                ("side", Value::Enum(McpSide::Server.value())),
                ("minute", Value::Time(ago(10))),
                ("server_name", text(server)),
                ("deploy_id", text("v1")),
                ("capture_source", Value::Enum(source.value())),
                ("method", text("tools/call")),
                ("target_name", text("read")),
                ("client_name", text("agent")),
                ("error_class", Value::Enum(class.value())),
                ("caller_fault", Value::UInt(fault)),
                ("count", Value::UInt(count)),
                ("dur_hist", histogram(100, count)),
                ("last_seen", Value::Time(ago(10))),
            ],
        );
        world.store.insert(TableId::McpToolRollup, row);
    }
    let deploy = stored_row(
        TableId::McpDeploys,
        &[
            ("project_id", project.clone()),
            ("server_name", text("files")),
            ("deploy_id", text("v1")),
            ("server_version", text("1.0.0")),
            ("capture_source", Value::Enum(CaptureSource::Sdk.value())),
            ("first_seen", Value::Time(ago(30))),
            ("last_seen", Value::Time(ago(5))),
        ],
    );
    world.store.insert(TableId::McpDeploys, deploy);
    let listed = stored_row(
        TableId::McpToolInventory,
        &[
            ("project_id", project.clone()),
            ("server_name", text("files")),
            ("deploy_id", text("v1")),
            ("tool_name", text("write")),
            ("schema_hash", Value::UInt(0xfeed)),
            ("first_seen", Value::Time(ago(20))),
            ("last_seen", Value::Time(ago(20))),
        ],
    );
    world.store.insert(TableId::McpToolInventory, listed);
    for span in [1u64, 2] {
        let start = ago(10).saturating_add_micros(span as i64);
        let row = stored_row(
            TableId::McpOps,
            &[
                ("project_id", project.clone()),
                ("side", Value::Enum(McpSide::Server.value())),
                ("capture_source", Value::Enum(CaptureSource::Sdk.value())),
                ("group_key_kind", Value::Enum(GroupKeyKind::Trace.value())),
                ("start_minute", Value::Time(start.floor_minute())),
                ("trace_hash", Value::UInt(u64::from(trace().trace_hash()))),
                ("start_time", Value::Time(start)),
                ("span_id", Value::Bytes(span.to_be_bytes().to_vec())),
                ("trace_id", Value::Bytes(trace().to_bytes().to_vec())),
                ("server_name", text("files")),
                ("method", text("tools/call")),
                ("target_name", text("read")),
                ("client_name", text("agent")),
                ("client_version", text("2.0")),
                ("user_id", text("alice")),
                ("group_key", text("g1")),
                ("duration_us", Value::UInt(1_000)),
                ("received_at", Value::Time(start)),
            ],
        );
        world.store.insert(TableId::McpOps, row);
    }
    world
}

fn params(extra: &str) -> String {
    format!("side=server&from={}&to={}{extra}", ago(60), NOW)
}

#[test]
fn servers_answer_with_the_capture_source_of_each() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let reply = get(&api, &path(1, &format!("/mcp/servers?{}", params("")))).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        insta::assert_snapshot!("mcp_servers", reply.pretty());
        let files = &reply.body["data"][0];
        assert_eq!(files["server_name"], "files");
        assert_eq!(files["stats"]["calls"], 5, "the SDK's calls, without caller faults");
        assert_eq!(files["stats"]["caller_faults"], 2);
        assert_eq!(files["version"], "1.0.0");
        assert_eq!(reply.body["capture_sources"][1]["chosen"], "sdk");
        assert_eq!(reply.body["capture_sources"][1]["available"], json!(["sdk", "proxy"]));

        let proxy = get(&api, &path(1, &format!("/mcp/servers?{}", params("&source=proxy")))).await;
        assert_eq!(proxy.body["data"][0]["stats"]["calls"], 9);
        let faults =
            get(&api, &path(1, &format!("/mcp/servers?{}", params("&include_caller_faults=true")))).await;
        assert_eq!(faults.body["data"][0]["stats"]["calls"], 7);
    });
}

#[test]
fn every_route_refuses_bad_parameters_by_name() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let window = format!("from={}&to={}", ago(60), NOW);
        let reply = get(&api, &path(1, &format!("/mcp/tools?{window}"))).await;
        reply.assert_problem(422, "validation_failed");
        assert_eq!(reply.field_errors(), [("/side", "required")]);
        let reply = get(&api, &path(1, &format!("/mcp/tools?{window}&side=both&source=x"))).await;
        assert_eq!(reply.field_errors(), [("/side", "invalid"), ("/source", "invalid")]);
        let reply = get(&api, &path(1, &format!("/mcp/deploys?{}", params("&include_caller_faults=1")))).await;
        assert_eq!(reply.field_errors(), [("/include_caller_faults", "invalid")]);
        let reply = get(&api, &path(1, &format!("/mcp/users?{}", params("&cursor=x")))).await;
        assert_eq!(reply.field_errors(), [("/cursor", "unknown_parameter")]);
        let long = format!("side=server&from={}&to={}", NOW.saturating_add_micros(-31 * 1_440 * MINUTE), NOW);
        let reply = get(&api, &path(1, &format!("/mcp/servers?{long}"))).await;
        reply.assert_problem(422, "query_too_broad");
        let reply = get(&api, &path(1, &format!("/mcp/compare?{}", params("")))).await;
        assert_eq!(reply.field_errors(), [("/server", "required"), ("/baseline_deploy", "required")]);
        let reply = get(&api, &path(1, &format!("/mcp/operations/{}/zz?{}", trace(), params("")))).await;
        assert_eq!(reply.field_errors(), [("/span_id", "invalid_id")]);
        // Another project's key cannot see the project.
        let reply = send(&api, Method::GET, &path(1, &format!("/mcp/servers?{}", params(""))), Some(&other_key())).await;
        reply.assert_problem(404, "not_found");
    });
}

#[test]
fn tools_tool_detail_errors_and_clients() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let tools = get(&api, &path(1, &format!("/mcp/tools?{}", params("")))).await;
        assert_eq!(tools.status, StatusCode::OK, "{:#}", tools.body);
        let names: Vec<(&str, bool)> = tools.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| (t["tool_name"].as_str().unwrap(), t["unused"].as_bool().unwrap()))
            .collect();
        assert_eq!(names, [("read", false), ("read", false), ("write", true)]);
        assert_eq!(tools.body["data"][2]["schema_hash"], "000000000000feed");

        let tool = get(&api, &path(1, &format!("/mcp/tools/read?{}", params("&server=files")))).await;
        assert_eq!(tool.status, StatusCode::OK, "{:#}", tool.body);
        assert_eq!(tool.body["granularity"], "minute");
        assert_eq!(tool.body["series"].as_array().unwrap().len(), 60);
        assert_eq!(tool.body["deploys"][0]["server_version"], "1.0.0");

        let errors = get(&api, &path(1, &format!("/mcp/errors?{}", params("")))).await;
        assert_eq!(errors.body["view"], "summary");
        assert_eq!(errors.body["classes"], json!([{"error_class": "timeout", "calls": 1}]));
        let breakdown = get(&api, &path(1, &format!("/mcp/errors?{}", params("&view=breakdown")))).await;
        assert_eq!(breakdown.body["view"], "breakdown");
        assert_eq!(breakdown.body["stats"], serde_json::Value::Null);

        let clients = get(&api, &path(1, &format!("/mcp/clients?{}", params("")))).await;
        assert_eq!(clients.body["self_reported"], true);
        assert_eq!(clients.body["data"][0]["client_name"], "agent");
        let versions = get(&api, &path(1, &format!("/mcp/clients?{}", params("&view=breakdown")))).await;
        assert_eq!(versions.body["versions"][0]["client_version"], "2.0");
        assert_eq!(versions.body["versions"][0]["calls"], 2);
    });
}

#[test]
fn users_sessions_deploys_and_compare() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let users = get(&api, &path(1, &format!("/mcp/users?{}", params("")))).await;
        assert_eq!(users.body["data"][0]["user_id"], "alice");
        assert_eq!(users.body["data"][0]["calls"], 2);
        let sessions = get(&api, &path(1, &format!("/mcp/sessions?{}", params("&limit=1")))).await;
        assert_eq!(sessions.body["data"][0]["group_key"], "g1");
        assert_eq!(sessions.body["data"][0]["group_key_kind"], "trace");
        let deploys = get(&api, &path(1, &format!("/mcp/deploys?{}", params("")))).await;
        assert_eq!(deploys.body["data"][0]["deploy_id"], "v1");
        let compare = get(
            &api,
            &path(1, &format!("/mcp/compare?{}", params("&server=files&baseline_deploy=v0&candidate_deploy=v1"))),
        )
        .await;
        assert_eq!(compare.status, StatusCode::OK, "{:#}", compare.body);
        assert_eq!(compare.body["server_name"], "files");
        assert_eq!(compare.body["tools"][0]["candidate"]["calls"], 5);
        assert_eq!(compare.body["added"], json!(["write"]));
    });
}

#[test]
fn the_operation_list_pages_with_a_bound_cursor_and_one_operation_reads_whole() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let first = get(&api, &path(1, &format!("/mcp/operations?{}", params("&limit=1")))).await;
        assert_eq!(first.status, StatusCode::OK, "{:#}", first.body);
        assert_eq!(first.body["data"][0]["span_id"], "0000000000000002");
        assert_eq!(first.body["has_more"], true);
        let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
        let second = get(&api, &path(1, &format!("/mcp/operations?{}", params(&format!("&limit=1&cursor={cursor}"))))).await;
        assert_eq!(second.body["data"][0]["span_id"], "0000000000000001");
        // The cursor is bound to its filters.
        let moved = get(
            &api,
            &path(1, &format!("/mcp/operations?{}", params(&format!("&limit=1&target=x&cursor={cursor}")))),
        )
        .await;
        moved.assert_problem(400, "cursor_invalid");

        let one = get(&api, &path(1, &format!("/mcp/operations/{}/0000000000000001?{}", trace(), params("")))).await;
        assert_eq!(one.status, StatusCode::OK, "{:#}", one.body);
        insta::assert_snapshot!("mcp_operation", one.pretty());
        assert_eq!(one.body["operation"]["operation"]["client_version"], "2.0");
        let missing =
            get(&api, &path(1, &format!("/mcp/operations/{}/00000000000000ff?{}", trace(), params("")))).await;
        missing.assert_problem(404, "not_found");
    });
}

#[test]
fn the_metrics_query_reads_the_mcp_ops_source_with_a_side() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let body = |extra: serde_json::Value| {
            let mut body = json!({
                "source": "mcp_ops",
                "measures": [{"measure": "mcp.calls", "aggregation": "sum"}],
                "dimensions": ["server_name"],
                "granularity": "hour",
                "fill": "none",
                "from": ago(60).to_string(),
                "to": NOW.to_string(),
            });
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            body.to_string()
        };
        let missing = post(&api, &path(1, "/metrics/query"), &body(json!({})), &[]).await;
        missing.assert_problem(422, "validation_failed");
        assert_eq!(missing.field_errors(), [("/side", "required")]);
        let reply = post(&api, &path(1, "/metrics/query"), &body(json!({"side": "server"})), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        let points: Vec<(String, f64)> = reply.body["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (s["key"][0]["value"].as_str().unwrap().to_owned(), s["points"][0]["value"].as_f64().unwrap())
            })
            .collect();
        assert_eq!(points, [("files".to_owned(), 5.0), ("db".to_owned(), 1.0)]);
        let rate = post(
            &api,
            &path(1, "/metrics/query"),
            &body(json!({"side": "server", "measures": [{"measure": "mcp.error_rate", "aggregation": "rate"}]})),
            &[],
        )
        .await;
        assert_eq!(rate.status, StatusCode::OK, "{:#}", rate.body);
        assert_eq!(rate.body["series"][0]["unit"], "ppm");
        // The side belongs to the mcp_ops source only.
        let spans = post(
            &api,
            &path(1, "/metrics/query"),
            &json!({
                "source": "spans",
                "measures": [{"measure": "count", "aggregation": "sum"}],
                "side": "server",
                "from": ago(60).to_string(),
                "to": NOW.to_string(),
            })
            .to_string(),
            &[],
        )
        .await;
        assert_eq!(spans.field_errors(), [("/side", "invalid")]);
    });
}
