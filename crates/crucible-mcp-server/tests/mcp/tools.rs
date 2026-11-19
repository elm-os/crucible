//! Each tool's result shape (with snapshots of its structured output), previews, argument checks, unknown tools,
//! cursors and store outages (contract 8.6 to 8.8, 12.3 and 12.4).

use serde_json::{Value, json};

use crate::world::{GRACE, NOW, World, span, trace};
use crate::{Client, key, mcp, run, window};

const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;
const DAY: i64 = 24 * HOUR;

fn ago(seconds: i64) -> crucible_records::Timestamp {
    NOW.saturating_add_micros(-seconds * SECOND)
}

/// The day before [`NOW`] and the hour after it.
fn day() -> Value {
    window(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(HOUR),
    )
}

/// Trace 1 is served by the store only: root 10 with children 11 and 12, and 13 under 11. Trace 2 is only in
/// the recent index: root 20 and its child 21.
fn two_traces() -> World {
    let mut world = World::new(100);
    world.ack(vec![
        span(1, 10, 0, ago(60)),
        span(1, 11, 10, ago(59)),
        span(1, 12, 10, ago(58)),
        span(1, 13, 11, ago(57)),
    ]);
    world.settle();
    // Lets the grace pass once more, so the index lets go of what the store serves.
    world.clock.advance(GRACE);
    world.writer.maintain();
    world.ack(vec![span(2, 20, 0, ago(30)), span(2, 21, 20, ago(29))]);
    world
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

fn assert_whole(output: &Value) {
    assert_eq!(output["meta"]["partial"], false, "{output:#}");
    assert_eq!(output["meta"]["consistency"], "strong", "{output:#}");
}

#[test]
fn every_tool_answers_with_its_shape() {
    run(|env| async move {
        let world = two_traces();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));

        let bounds = client.output("get_data_bounds", json!({})).await;
        assert_eq!(bounds["max_window_days"], 30);
        assert_eq!(bounds["retention_horizon"], Value::Null);
        assert_eq!(bounds["server_time"], bounds["meta"]["as_of"]);
        assert_whole(&bounds);
        insta::assert_snapshot!("get_data_bounds", pretty(&bounds));

        let traces = client.output("list_traces", day()).await;
        let data = traces["data"].as_array().unwrap();
        assert_eq!(data[0]["trace_id"], trace(2).to_string());
        assert_eq!(data[1]["trace_id"], trace(1).to_string());
        // The list carries no previews: get_trace has them.
        assert!(data.iter().all(|row| row.get("input_preview").is_none()));
        assert_eq!(data[1]["span_count"], 4);
        assert_eq!(traces["has_more"], false);
        assert_eq!(traces["truncated"], false);
        assert_eq!(traces["next_cursor"], Value::Null);
        assert_whole(&traces);
        insta::assert_snapshot!("list_traces", pretty(&traces));

        let stored = client
            .output("get_trace", json!({ "trace_id": trace(1).to_string() }))
            .await;
        assert_eq!(stored["span_count"], 4);
        assert_eq!(stored["paged"], false);
        assert_eq!(stored["truncated"], false);
        let spans = stored["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0]["parent_span_id"], Value::Null);
        assert_eq!(spans[3]["parent_span_id"], "000000000000000b");
        assert_eq!(spans[0]["input_preview"], "preview");
        assert_eq!(spans[0]["output_preview"], "");
        assert_whole(&stored);
        insta::assert_snapshot!("get_trace", pretty(&stored));

        let recent = client
            .output("get_trace", json!({ "trace_id": trace(2).to_string() }))
            .await;
        let spans = recent["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|span| span["input_preview"].is_null()));
        assert!(spans.iter().all(|span| span["output_preview"].is_null()));

        let children = client
            .output(
                "get_span_children",
                json!({ "trace_id": trace(1).to_string(), "parent_span_id": "000000000000000a" }),
            )
            .await;
        let data = children["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["child_count"], 1);
        assert_eq!(data[1]["child_count"], 0);
        assert_eq!(children["has_more"], false);
        assert_whole(&children);
        insta::assert_snapshot!("get_span_children", pretty(&children));
    });
}

fn pointers(problem: &Value) -> Vec<(&str, &str)> {
    problem["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["pointer"].as_str().unwrap(), e["code"].as_str().unwrap()))
        .collect()
}

#[test]
fn arguments_are_checked_and_each_refusal_names_what_to_change() {
    run(|env| async move {
        let world = two_traces();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let now = NOW.to_string();
        let hour_ago = NOW.saturating_add_micros(-HOUR).to_string();
        let trace_id = trace(1).to_string();
        let cases = [
            ("list_traces", json!({}), vec![("", "invalid_arguments")]),
            (
                "list_traces",
                json!({ "from": "yesterday", "to": now }),
                vec![("/from", "invalid_timestamp")],
            ),
            (
                "list_traces",
                json!({ "from": now, "to": hour_ago }),
                vec![("/to", "invalid_window")],
            ),
            (
                "list_traces",
                json!({ "from": hour_ago, "to": now, "limit": 0 }),
                vec![("/limit", "out_of_range")],
            ),
            (
                "list_traces",
                json!({ "from": hour_ago, "to": now, "limit": 51 }),
                vec![("/limit", "out_of_range")],
            ),
            (
                "list_traces",
                json!({ "from": hour_ago, "to": now, "project_id": "another" }),
                vec![("", "invalid_arguments")],
            ),
            (
                "get_trace",
                json!({ "trace_id": "xyz" }),
                vec![("/trace_id", "invalid_id")],
            ),
            (
                "get_span_children",
                json!({ "trace_id": trace_id, "parent_span_id": "nope", "limit": 201 }),
                vec![
                    ("/parent_span_id", "invalid_id"),
                    ("/limit", "out_of_range"),
                ],
            ),
            (
                "get_data_bounds",
                json!({ "from": now }),
                vec![("", "invalid_arguments")],
            ),
        ];
        for (tool, arguments, expected) in cases {
            let problem = client.error(tool, arguments.clone()).await;
            assert_eq!(
                problem["code"], "validation_failed",
                "{tool} {arguments}: {problem:#}"
            );
            assert_eq!(problem["status"], 422);
            assert_eq!(
                pointers(&problem),
                expected,
                "{tool} {arguments}: {problem:#}"
            );
            assert!(
                problem["request_id"]
                    .as_str()
                    .is_some_and(|id| id.len() == 36)
            );
        }
        let problem = client.error("list_traces", json!({})).await;
        let message = problem["errors"][0]["message"].as_str().unwrap();
        assert!(message.contains("missing field `from`"), "{message}");

        // A window longer than the project allows says which limit it passed.
        let wide = window(NOW.saturating_add_micros(-31 * DAY), NOW);
        let problem = client.error("list_traces", wide).await;
        assert_eq!(problem["code"], "query_too_broad");
        assert_eq!(
            problem["limit"],
            json!({ "name": "max_window_days", "value": 30 })
        );
        // A window of exactly the maximum passes.
        let widest = window(NOW.saturating_add_micros(-30 * DAY), NOW);
        client.output("list_traces", widest).await;
    });
}

#[test]
fn an_unknown_tool_is_a_protocol_error() {
    run(|env| async move {
        let world = World::new(10);
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let reply = client
            .request(
                "tools/call",
                json!({ "name": "drop_project", "arguments": {} }),
            )
            .await;
        assert_eq!(reply.body["error"]["code"], -32602, "{:#}", reply.body);
        let message = reply.body["error"]["message"].as_str().unwrap();
        assert!(message.contains("tools/list"), "{message}");
    });
}

#[test]
fn a_cursor_continues_only_the_call_that_returned_it() {
    run(|env| async move {
        let mut world = World::new(1_000);
        // 60 traces, one root span each.
        let roots = (1..=60).map(|n| span(n, 1_000 + n, 0, ago(i64::try_from(n).unwrap())));
        world.ack(roots.collect());
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let page = client.output("list_traces", day()).await;
        let cursor = page["next_cursor"].as_str().unwrap().to_owned();
        // Another window, another tool, a changed byte, or another project's key: each restarts.
        let mut other_window = window(NOW.saturating_add_micros(-DAY), NOW);
        other_window["cursor"] = json!(cursor);
        let mut tampered = day();
        let flipped = if cursor.ends_with('A') { 'B' } else { 'A' };
        tampered["cursor"] = json!(format!("{}{flipped}", &cursor[..cursor.len() - 1]));
        for arguments in [other_window, tampered] {
            let problem = client.error("list_traces", arguments).await;
            assert_eq!(problem["code"], "cursor_invalid", "{problem:#}");
            assert!(
                problem["detail"]
                    .as_str()
                    .unwrap()
                    .contains("without `cursor`")
            );
        }
        let problem = client
            .error(
                "get_trace",
                json!({ "trace_id": trace(1).to_string(), "cursor": cursor }),
            )
            .await;
        assert_eq!(problem["code"], "cursor_invalid");
        let mut foreign = day();
        foreign["cursor"] = json!(cursor);
        let problem = Client::modern(&mcp, &key(2))
            .error("list_traces", foreign)
            .await;
        assert_eq!(problem["code"], "cursor_invalid");
    });
}

#[test]
fn a_store_outage_answers_from_the_recent_index_or_says_to_retry() {
    run(|env| async move {
        let world = two_traces();
        world.store.set_unavailable(true);
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        // Trace 2 is in the recent index: the answer is partial, and says why.
        let recent = client
            .output("get_trace", json!({ "trace_id": trace(2).to_string() }))
            .await;
        assert_eq!(recent["spans"].as_array().unwrap().len(), 2);
        assert_eq!(recent["meta"]["partial"], true);
        assert_eq!(recent["meta"]["partial_reason"], "store_unavailable");
        // Trace 1 is only in the store: nothing can answer.
        let problem = client
            .error("get_trace", json!({ "trace_id": trace(1).to_string() }))
            .await;
        assert_eq!(
            problem["code"], "telemetry_store_unavailable",
            "{problem:#}"
        );
        assert_eq!(problem["status"], 503);
        assert!(problem["detail"].as_str().unwrap().contains("retry"));
    });
}
