//! The caps: rows per call, 64 KiB per result whatever the text holds, continuation that reads every row once,
//! and the per-key rate limit.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crucible_mcp_server::McpConfig;
use crucible_recent::SpanEntry;
use serde_json::{Value, json};

use crate::world::{NOW, World, span, trace};
use crate::{Client, key, mcp, mcp_with, run, window};

const SECOND: i64 = 1_000_000;
const DAY: i64 = 86_400 * SECOND;

fn ago(micros: i64) -> crucible_records::Timestamp {
    NOW.saturating_add_micros(-micros)
}

/// A span name of 1,024 bytes (the cap of contract 3.1) that JSON escapes heavily: quotes, backslashes,
/// newlines and control characters, after a prefix that tells the spans apart.
pub(crate) fn hostile(id: u64) -> String {
    let mut name = format!("{id:06} ");
    while name.len() < 1_024 {
        name.push_str("\"\\\n\u{1}");
    }
    name.truncate(1_024);
    name
}

pub(crate) fn named(
    trace_n: u64,
    id: u64,
    parent: u64,
    start: crucible_records::Timestamp,
) -> SpanEntry {
    let mut entry = span(trace_n, id, parent, start);
    entry.summary.name = Arc::from(hostile(id));
    entry
}

fn day() -> Value {
    window(NOW.saturating_add_micros(-DAY), NOW)
}

/// Calls `tool` from `arguments` on, then with each `next_cursor`, until none is left; returns every row and
/// the number of calls. Each call's result is checked against the 64 KiB cap by `Client::output`.
pub(crate) async fn read_all(
    client: &mut Client,
    tool: &str,
    arguments: Value,
    rows: &str,
) -> (Vec<Value>, usize) {
    let (mut all, mut calls) = (Vec::new(), 0);
    let mut arguments = arguments;
    loop {
        let output = client.output(tool, arguments.clone()).await;
        calls += 1;
        let page = output[rows].as_array().unwrap();
        assert!(
            !page.is_empty(),
            "{tool} returned an empty page: {output:#}"
        );
        all.extend(page.iter().cloned());
        match output["next_cursor"].as_str() {
            Some(cursor) => {
                assert!(
                    output["truncated"] == true || output["has_more"] == true,
                    "{output:#}"
                );
                arguments["cursor"] = json!(cursor);
            }
            None => {
                assert_eq!(output["truncated"], false, "{output:#}");
                return (all, calls);
            }
        }
    }
}

pub(crate) fn ids<'a>(rows: &'a [Value], field: &str) -> Vec<&'a str> {
    rows.iter()
        .map(|row| row[field].as_str().unwrap())
        .collect()
}

pub(crate) fn assert_once(ids: &[&str], expected: usize) {
    let distinct: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(distinct.len(), ids.len(), "a row came back twice");
    assert_eq!(ids.len(), expected);
}

#[test]
fn list_traces_returns_at_most_50_traces_per_call_and_continues() {
    run(|env| async move {
        let mut world = World::new(1_000);
        world.ack(
            (1..=60)
                .map(|n| span(n, n, 0, ago(i64::try_from(n).unwrap() * SECOND)))
                .collect(),
        );
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let first = client.output("list_traces", day()).await;
        assert_eq!(first["data"].as_array().unwrap().len(), 50);
        assert_eq!(first["has_more"], true);
        assert_eq!(first["truncated"], false);
        let (traces, calls) = read_all(&mut client, "list_traces", day(), "data").await;
        assert_eq!(calls, 2);
        assert_once(&ids(&traces, "trace_id"), 60);
        // Newest first across the pages.
        assert_eq!(traces[0]["trace_id"], trace(1).to_string());
        assert_eq!(traces[59]["trace_id"], trace(60).to_string());
    });
}

#[test]
fn a_list_too_large_for_one_result_is_cut_under_64_kib_and_continues() {
    run(|env| async move {
        let mut world = World::new(10_000);
        world.ack(
            (1..=40)
                .map(|n| named(n, n, 0, ago(i64::try_from(n).unwrap() * SECOND)))
                .collect(),
        );
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let first = client.output("list_traces", day()).await;
        let rows = first["data"].as_array().unwrap().len();
        assert!((1..40).contains(&rows), "{rows} rows in one result");
        assert_eq!(first["truncated"], true);
        assert_eq!(first["has_more"], true);
        let (traces, calls) = read_all(&mut client, "list_traces", day(), "data").await;
        assert!(calls > 1);
        assert_once(&ids(&traces, "trace_id"), 40);
        for (n, row) in (1..=40).zip(&traces) {
            assert_eq!(row["name"], hostile(n), "the names come back whole");
        }
    });
}

#[test]
fn get_trace_returns_at_most_200_spans_per_call_and_continues_in_start_order() {
    run(|env| async move {
        let mut world = World::new(2_000);
        let start = ago(DAY / 2);
        let mut spans = vec![span(1, 1, 0, start)];
        spans.extend((2..=450).map(|id| {
            span(
                1,
                id,
                1,
                start.saturating_add_micros(i64::try_from(id).unwrap()),
            )
        }));
        world.ack(spans);
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let trace_id = json!({ "trace_id": trace(1).to_string() });
        let first = client.output("get_trace", trace_id.clone()).await;
        // A span row takes about 800 bytes of the result (its JSON is carried twice), so the 64 KiB cap cuts
        // a page before the 200-span cap does.
        let rows = first["spans"].as_array().unwrap().len();
        assert!((50..=200).contains(&rows), "{rows} spans in one result");
        assert_eq!(first["span_count"], 450);
        assert_eq!(first["truncated"], true);
        let (spans, calls) = read_all(&mut client, "get_trace", trace_id, "spans").await;
        assert!(calls >= 3);
        let ids = ids(&spans, "span_id");
        assert_once(&ids, 450);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "start order");
    });
}

#[test]
fn spans_too_large_for_one_result_are_cut_under_64_kib_and_continue() {
    run(|env| async move {
        let mut world = World::new(10_000);
        let start = ago(DAY / 2);
        let mut spans = vec![named(1, 1, 0, start)];
        spans.extend((2..=60).map(|id| {
            named(
                1,
                id,
                1,
                start.saturating_add_micros(i64::try_from(id).unwrap()),
            )
        }));
        world.ack(spans);
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));

        let trace_id = json!({ "trace_id": trace(1).to_string() });
        let first = client.output("get_trace", trace_id.clone()).await;
        let rows = first["spans"].as_array().unwrap().len();
        assert!((1..60).contains(&rows), "{rows} spans in one result");
        assert_eq!(first["truncated"], true);
        let (spans, _) = read_all(&mut client, "get_trace", trace_id, "spans").await;
        assert_once(&ids(&spans, "span_id"), 60);

        let children = json!({
            "trace_id": trace(1).to_string(),
            "parent_span_id": "0000000000000001",
            "limit": 200,
        });
        let first = client.output("get_span_children", children.clone()).await;
        assert_eq!(first["truncated"], true);
        let (data, _) = read_all(&mut client, "get_span_children", children, "data").await;
        assert_once(&ids(&data, "span_id"), 59);
        for span in &data {
            assert_eq!(span["child_count"], 0);
        }
    });
}

#[test]
fn the_rate_limit_refuses_calls_past_the_burst_and_says_when_to_call_again() {
    run(|env| async move {
        let world = World::new(10);
        let config = McpConfig {
            calls_per_second: 1,
            burst: 2,
        };
        let limited = mcp_with(&env, &world, config);
        let mut client = Client::modern(&limited, &key(1));
        for _ in 0..2 {
            client.output("get_data_bounds", json!({})).await;
        }
        let problem = client.error("get_data_bounds", json!({})).await;
        assert_eq!(problem["code"], "rate_limited", "{problem:#}");
        assert_eq!(problem["status"], 429);
        assert_eq!(problem["retry_after_s"], 1);
        // Listing tools is not a read, and another key has its own budget.
        let listed = client.request("tools/list", json!({})).await;
        assert!(listed.body["result"]["tools"].is_array());
        Client::modern(&limited, &key(2))
            .output("get_data_bounds", json!({}))
            .await;
        // After the wait the key may call again.
        env.clock.sleep(Duration::from_secs(1)).await;
        client.output("get_data_bounds", json!({})).await;
        // The default limit is generous: a burst of 200 calls passes.
        let generous = mcp(&env, &world);
        let mut client = Client::modern(&generous, &key(1));
        for _ in 0..200 {
            client.output("get_data_bounds", json!({})).await;
        }
        assert_eq!(
            client.error("get_data_bounds", json!({})).await["code"],
            "rate_limited"
        );
    });
}
