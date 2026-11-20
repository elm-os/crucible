//! The MCP analytics tools (contract 9.9): `list_mcp_servers`, `list_mcp_tools`, `get_mcp_tool`,
//! `list_mcp_errors` and `compare_mcp_deploys` read through the query services in class `agent`, name the
//! capture source of each server, keep caller faults apart, and refuse bad arguments by name.

use crucible_plan::Value as Cell;
use crucible_records::{CaptureSource, ErrorClass, McpSide, TableId, Timestamp};
use serde_json::{Value, json};

use crate::store::stored_row;
use crate::world::{NOW, World, project};
use crate::{Client, key, mcp, run};

const MINUTE: i64 = 60_000_000;

fn ago(minutes: i64) -> Timestamp {
    NOW.saturating_add_micros(-minutes * MINUTE)
}

fn text(value: &str) -> Cell {
    Cell::String(value.to_owned())
}

/// `files` captured by the SDK and the proxy, `db` by OTLP; two deploys of `files` and its inventory.
fn world() -> World {
    let world = World::new(10);
    let project = Cell::Uuid(project(1).uuid());
    let histogram = |count: u64| {
        Cell::Tuple2(
            Box::new(Cell::Array(vec![Cell::UInt(100)])),
            Box::new(Cell::Array(vec![Cell::UInt(count)])),
        )
    };
    for (server, source, deploy, class, fault, count) in [
        ("files", CaptureSource::Sdk, "v1", ErrorClass::None, 0, 4u64),
        ("files", CaptureSource::Sdk, "v2", ErrorClass::None, 0, 3),
        ("files", CaptureSource::Sdk, "v2", ErrorClass::Timeout, 0, 2),
        (
            "files",
            CaptureSource::Sdk,
            "v2",
            ErrorClass::Validation,
            1,
            5,
        ),
        (
            "files",
            CaptureSource::ProxyStdio,
            "v2",
            ErrorClass::None,
            0,
            30,
        ),
        ("db", CaptureSource::Otlp, "d1", ErrorClass::None, 0, 1),
    ] {
        let row = stored_row(
            TableId::McpToolRollup,
            &[
                ("project_id", project.clone()),
                ("side", Cell::Enum(McpSide::Server.value())),
                ("minute", Cell::Time(ago(5))),
                ("server_name", text(server)),
                ("deploy_id", text(deploy)),
                ("capture_source", Cell::Enum(source.value())),
                ("method", text("tools/call")),
                ("target_name", text("read")),
                ("client_name", text("agent")),
                ("error_class", Cell::Enum(class.value())),
                ("caller_fault", Cell::UInt(fault)),
                ("count", Cell::UInt(count)),
                ("dur_hist", histogram(count)),
                ("last_seen", Cell::Time(ago(5))),
            ],
        );
        world.store.insert(TableId::McpToolRollup, row);
    }
    for (deploy, version, first) in [("v1", "1.0.0", 50), ("v2", "1.1.0", 20)] {
        let row = stored_row(
            TableId::McpDeploys,
            &[
                ("project_id", project.clone()),
                ("server_name", text("files")),
                ("deploy_id", text(deploy)),
                ("server_version", text(version)),
                ("capture_source", Cell::Enum(CaptureSource::Sdk.value())),
                ("first_seen", Cell::Time(ago(first))),
                ("last_seen", Cell::Time(ago(first - 10))),
            ],
        );
        world.store.insert(TableId::McpDeploys, row);
    }
    for (deploy, tool, hash) in [("v1", "read", 1u64), ("v2", "read", 2), ("v2", "write", 3)] {
        let row = stored_row(
            TableId::McpToolInventory,
            &[
                ("project_id", project.clone()),
                ("server_name", text("files")),
                ("deploy_id", text(deploy)),
                ("tool_name", text(tool)),
                ("schema_hash", Cell::UInt(hash)),
                ("first_seen", Cell::Time(ago(30))),
                ("last_seen", Cell::Time(ago(30))),
            ],
        );
        world.store.insert(TableId::McpToolInventory, row);
    }
    world
}

fn hour(extra: Value) -> Value {
    let mut arguments =
        json!({ "side": "server", "from": ago(60).to_string(), "to": NOW.to_string() });
    for (name, value) in extra.as_object().unwrap() {
        arguments[name] = value.clone();
    }
    arguments
}

#[test]
fn the_analytics_tools_read_one_capture_source_per_server() {
    run(|env| async move {
        let world = world();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));

        let servers = client.output("list_mcp_servers", hour(json!({}))).await;
        assert_eq!(servers["data"][0]["server_name"], "files");
        assert_eq!(
            servers["data"][0]["stats"]["calls"], 9,
            "the SDK's calls, without caller faults"
        );
        assert_eq!(servers["data"][0]["stats"]["caller_faults"], 5);
        assert_eq!(servers["data"][0]["version"], "1.1.0");
        assert_eq!(
            servers["capture_sources"][1],
            json!({ "server_name": "files", "available": ["sdk", "proxy"], "chosen": "sdk" })
        );
        let proxy = client
            .output("list_mcp_servers", hour(json!({ "source": "proxy" })))
            .await;
        assert_eq!(proxy["data"][0]["stats"]["calls"], 30);
        let faults = client
            .output(
                "list_mcp_servers",
                hour(json!({ "include_caller_faults": true })),
            )
            .await;
        assert_eq!(faults["data"][0]["stats"]["calls"], 14);

        let tools = client
            .output("list_mcp_tools", hour(json!({ "server": "files" })))
            .await;
        let rows: Vec<(String, bool)> = tools["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["tool_name"].as_str().unwrap().to_owned(),
                    t["unused"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [("read".to_owned(), false), ("write".to_owned(), true)]
        );
        assert_eq!(tools["data"][0]["schema_hash"], "0000000000000002");

        let tool = client
            .output(
                "get_mcp_tool",
                hour(json!({ "name": "read", "server": "files" })),
            )
            .await;
        assert_eq!(tool["stats"]["calls"], 9);
        assert_eq!(tool["granularity"], "minute");
        assert_eq!(
            tool["series"].as_array().unwrap().len(),
            1,
            "only buckets with calls"
        );
        assert_eq!(
            tool["error_classes"],
            json!([{ "error_class": "timeout", "calls": 2 }])
        );
        assert_eq!(tool["deploys"].as_array().unwrap().len(), 2);

        let errors = client.output("list_mcp_errors", hour(json!({}))).await;
        assert_eq!(
            errors["classes"],
            json!([{ "error_class": "timeout", "calls": 2 }])
        );
        assert_eq!(errors["targets"][0]["target_name"], "read");
        let breakdown = client
            .output("list_mcp_errors", hour(json!({ "breakdown": true })))
            .await;
        assert_eq!(breakdown["stats"], Value::Null);
        assert_eq!(breakdown["breakdown"], json!([]));

        let compare = client
            .output(
                "compare_mcp_deploys",
                hour(
                    json!({ "server": "files", "baseline_deploy": "v1", "candidate_deploy": "v2" }),
                ),
            )
            .await;
        assert_eq!(compare["tools"][0]["baseline"]["calls"], 4);
        assert_eq!(compare["tools"][0]["candidate"]["calls"], 5);
        assert_eq!(compare["tools"][0]["candidate"]["errors"], 2);
        assert_eq!(compare["added"], json!(["write"]));
        assert_eq!(compare["changed"], json!(["read"]));
    });
}

#[test]
fn the_analytics_tools_refuse_bad_arguments_by_name() {
    run(|env| async move {
        let world = world();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let pointers = |problem: &Value| -> Vec<(String, String)> {
            problem["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e["pointer"].as_str().unwrap().to_owned(),
                        e["code"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        };
        let problem = client
            .error(
                "list_mcp_servers",
                hour(json!({ "side": "both", "source": "x" })),
            )
            .await;
        assert_eq!(problem["code"], "validation_failed");
        assert_eq!(
            pointers(&problem),
            [
                ("/side".to_owned(), "invalid".to_owned()),
                ("/source".to_owned(), "invalid".to_owned())
            ]
        );
        let problem = client
            .error("get_mcp_tool", hour(json!({ "name": "" })))
            .await;
        assert_eq!(
            pointers(&problem),
            [("/name".to_owned(), "required".to_owned())]
        );
        let problem = client
            .error(
                "compare_mcp_deploys",
                hour(
                    json!({ "server": "files", "baseline_deploy": "v1", "candidate_deploy": "v1" }),
                ),
            )
            .await;
        assert_eq!(
            pointers(&problem),
            [("/candidate_deploy".to_owned(), "invalid".to_owned())]
        );
        let problem = client
            .error("list_mcp_tools", json!({ "side": "server" }))
            .await;
        assert_eq!(
            pointers(&problem),
            [(String::new(), "invalid_arguments".to_owned())]
        );
        let wide =
            hour(json!({ "from": NOW.saturating_add_micros(-31 * 1_440 * MINUTE).to_string() }));
        let problem = client.error("list_mcp_errors", wide).await;
        assert_eq!(problem["code"], "query_too_broad");
    });
}
