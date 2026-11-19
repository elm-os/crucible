//! Both protocol eras at the wire level: a session-era client opens a session with `initialize`, a stateless-era
//! client sends self-contained requests and gets plain JSON, and both see the same tools and the same results.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::world::{NOW, World, span};
use crate::{Client, LEGACY, MODERN, key, mcp, run, send, window};

const SECOND: i64 = 1_000_000;
const DAY: i64 = 86_400 * SECOND;

fn ago(seconds: i64) -> crucible_records::Timestamp {
    NOW.saturating_add_micros(-seconds * SECOND)
}

/// Trace 1 with a root and a child, served by the store.
fn one_trace() -> World {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, ago(60)), span(1, 11, 10, ago(59))]);
    world.settle();
    world
}

fn names(tools: &Value) -> Vec<&str> {
    tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

#[test]
fn both_eras_list_the_same_tools_and_get_the_same_results() {
    run(|env| async move {
        let world = one_trace();
        let mcp = mcp(&env, &world);
        let mut legacy = Client::legacy(&mcp, &key(1)).await;
        let mut modern = Client::modern(&mcp, &key(1));

        let discovered = modern.request("server/discover", json!({})).await;
        assert_eq!(discovered.status, StatusCode::OK, "{:#}", discovered.body);
        let result = &discovered.body["result"];
        assert!(
            result["supportedVersions"]
                .as_array()
                .unwrap()
                .contains(&json!(MODERN)),
            "{result:#}"
        );
        assert!(
            result["supportedVersions"]
                .as_array()
                .unwrap()
                .contains(&json!(LEGACY))
        );
        assert!(
            result["instructions"]
                .as_str()
                .unwrap()
                .contains("get_data_bounds")
        );
        assert_eq!(discovered.header("mcp-session-id"), None);

        // The registry decides the list; each era sees all of it, in name order.
        let mut lists = Vec::new();
        for client in [&mut legacy, &mut modern] {
            let listed = client.request("tools/list", json!({})).await;
            assert_eq!(listed.status, StatusCode::OK, "{:#}", listed.body);
            let tools = listed.body["result"]["tools"].clone();
            let mut sorted = names(&tools);
            sorted.sort_unstable();
            assert_eq!(names(&tools), sorted);
            for name in [
                "get_data_bounds",
                "get_span_children",
                "get_trace",
                "list_traces",
            ] {
                assert!(sorted.contains(&name), "{name} is not listed");
            }
            lists.push(sorted.join(","));
            for tool in tools.as_array().unwrap() {
                // Every tool reads; create_export alone starts work (an export job).
                let read_only = tool["name"] != "create_export";
                assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{tool:#}");
                assert_eq!(tool["inputSchema"]["type"], "object", "{tool:#}");
                assert_eq!(tool["outputSchema"]["type"], "object", "{tool:#}");
            }
        }
        assert_eq!(lists[0], lists[1]);

        let day = window(NOW.saturating_add_micros(-DAY), NOW);
        let from_session = legacy.output("list_traces", day.clone()).await;
        let stateless = modern.output("list_traces", day).await;
        assert_eq!(from_session, stateless);
        assert_eq!(stateless["data"][0]["span_count"], 2);
        let trace = json!({ "trace_id": crate::world::trace(1).to_string() });
        assert_eq!(
            legacy.output("get_trace", trace.clone()).await,
            modern.output("get_trace", trace).await
        );
    });
}

#[test]
fn the_stateless_era_answers_tool_calls_with_plain_json() {
    run(|env| async move {
        let world = one_trace();
        let mcp = mcp(&env, &world);
        let mut modern = Client::modern(&mcp, &key(1));
        let reply = modern
            .request(
                "tools/call",
                json!({ "name": "get_data_bounds", "arguments": {} }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("content-type"), Some("application/json"));
        assert_eq!(reply.header("mcp-session-id"), None);
        assert_eq!(reply.body["result"]["isError"], false);

        // The session era streams its answers on the session.
        let legacy = Client::legacy(&mcp, &key(1)).await;
        assert!(legacy.session.is_some());
        let call = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": "get_data_bounds", "arguments": {} },
        });
        let headers = [
            ("authorization", legacy.authorization.clone()),
            ("mcp-session-id", legacy.session.clone().unwrap()),
            ("mcp-protocol-version", LEGACY.to_owned()),
        ];
        let reply = send(&mcp, Method::POST, &headers, Some(&call)).await;
        assert_eq!(reply.header("content-type"), Some("text/event-stream"));
        assert_eq!(reply.body["result"]["isError"], false, "{:#}", reply.body);

        // A session-era request without its session is refused by the transport.
        let reply = send(&mcp, Method::POST, &headers[..1], Some(&call)).await;
        assert!(reply.status.is_client_error(), "{reply:?}");
    });
}

#[test]
fn a_session_reads_the_project_of_each_requests_key() {
    run(|env| async move {
        let world = one_trace();
        let mcp = mcp(&env, &world);
        let mut client = Client::legacy(&mcp, &key(1)).await;
        let day = window(NOW.saturating_add_micros(-DAY), NOW);
        let page = client.output("list_traces", day.clone()).await;
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        // The same session with project 2's key reads project 2, which has no traces: the scope comes from
        // each request's credential, never from the session.
        client.authorization = format!("Bearer {}", key(2));
        let page = client.output("list_traces", day.clone()).await;
        assert_eq!(page["data"], json!([]));
        client.authorization = format!("Bearer {}", key(1));
        let page = client.output("list_traces", day).await;
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
    });
}
