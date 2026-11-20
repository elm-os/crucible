//! The list tools of contract 12.5 (`list_spans`, `list_sessions`, `list_users`, `list_user_sessions`, and the
//! filter of `list_traces`): their shapes, filters, cursors that open only for their own list, the 64 KiB cap
//! with continuation, argument checks, and the per-key rate limit they share with every tool.

use std::sync::Arc;

use crucible_mcp_server::McpConfig;
use crucible_recent::SpanEntry;
use crucible_records::Level;
use serde_json::{Value, json};

use crate::caps::{assert_once, hostile, ids, named, read_all};
use crate::world::{DAY, NOW, World, span, trace};
use crate::{Client, key, mcp, mcp_with, run, window};

const MS: i64 = 1_000;

fn at(ms: i64) -> crucible_records::Timestamp {
    NOW.saturating_add_micros(ms * MS)
}

/// The day that holds [`NOW`], plus a day either side.
fn days() -> Value {
    window(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    )
}

/// A span of `session` and `user` (empty for none).
fn of(mut entry: SpanEntry, session: &str, user: &str) -> SpanEntry {
    entry.summary.session_id = Arc::from(session);
    entry.summary.user_id = Arc::from(user);
    entry
}

/// Three sessions: s1 (two traces of ada, one with an error span), s2 (one trace of bob), s3 (one trace of
/// ada).
fn three_sessions() -> World {
    let mut world = World::new(100);
    let mut failed = of(span(2, 21, 20, at(-50)), "s1", "ada");
    failed.summary.level = Level::Error;
    world.ack(vec![
        of(span(1, 10, 0, at(-60)), "s1", "ada"),
        of(span(1, 11, 10, at(-59)), "s1", "ada"),
    ]);
    world.ack(vec![of(span(2, 20, 0, at(-51)), "s1", "ada"), failed]);
    world.ack(vec![of(span(3, 30, 0, at(-40)), "s2", "bob")]);
    world.ack(vec![of(span(4, 40, 0, at(-30)), "s3", "ada")]);
    world.settle();
    world
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

fn with(mut arguments: Value, name: &str, value: Value) -> Value {
    arguments[name] = value;
    arguments
}

#[test]
fn the_list_tools_answer_with_their_shapes() {
    run(|env| async move {
        let world = three_sessions();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));

        let spans = client.output("list_spans", days()).await;
        let data = spans["data"].as_array().unwrap();
        assert_eq!(data.len(), 6);
        // Newest first, across traces.
        assert_eq!(data[0]["trace_id"], trace(4).to_string());
        assert_eq!(data[5]["span_id"], "000000000000000a");
        assert_eq!(data[5]["parent_span_id"], Value::Null);
        assert_eq!(spans["has_more"], false);
        assert_eq!(spans["truncated"], false);
        assert_eq!(spans["next_cursor"], Value::Null);
        insta::assert_snapshot!("list_spans", pretty(&spans));

        let errors = with(days(), "filter", json!({ "level": ["error"] }));
        let failed = client.output("list_spans", errors).await;
        let data = failed["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["span_id"], "0000000000000015");
        assert_eq!(data[0]["level"], "error");

        let sessions = client.output("list_sessions", days()).await;
        let data = sessions["data"].as_array().unwrap();
        assert_eq!(ids(data, "session_id"), ["s3", "s2", "s1"]);
        assert_eq!(data[2]["trace_count"], 2);
        assert_eq!(data[2]["error_count"], 1);
        assert_eq!(data[2]["user_ids"], json!(["ada"]));
        insta::assert_snapshot!("list_sessions", pretty(&sessions));

        let users = client.output("list_users", days()).await;
        let data = users["data"].as_array().unwrap();
        assert_eq!(ids(data, "user_id"), ["ada", "bob"]);
        assert_eq!(data[0]["session_count"], 2);
        assert_eq!(data[0]["trace_count"], 3);
        insta::assert_snapshot!("list_users", pretty(&users));

        let of_ada = with(days(), "user_id", json!("ada"));
        let ada = client.output("list_user_sessions", of_ada).await;
        let data = ada["data"].as_array().unwrap();
        assert_eq!(ids(data, "session_id"), ["s3", "s1"]);
        assert_eq!(data[1]["trace_count"], 2);
        insta::assert_snapshot!("list_user_sessions", pretty(&ada));

        // The trace list takes the same filter: the traces of session s1.
        let s1 = with(days(), "filter", json!({ "session_id": ["s1"] }));
        let traces = client.output("list_traces", s1).await;
        let data = traces["data"].as_array().unwrap();
        assert_eq!(
            ids(data, "trace_id"),
            [trace(2).to_string(), trace(1).to_string()]
        );
    });
}

#[test]
fn list_cursors_page_through_every_row_and_open_only_for_their_own_call() {
    run(|env| async move {
        let world = three_sessions();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let one = |arguments: Value| with(arguments, "limit", json!(1));

        let (spans, calls) = read_all(&mut client, "list_spans", one(days()), "data").await;
        assert_eq!(calls, 6);
        assert_once(&ids(&spans, "span_id"), 6);
        let (sessions, calls) = read_all(&mut client, "list_sessions", one(days()), "data").await;
        assert_eq!(calls, 3);
        assert_eq!(ids(&sessions, "session_id"), ["s3", "s2", "s1"]);
        let (users, _) = read_all(&mut client, "list_users", one(days()), "data").await;
        assert_eq!(ids(&users, "user_id"), ["ada", "bob"]);
        let of_ada = one(with(days(), "user_id", json!("ada")));
        let (ada, _) = read_all(&mut client, "list_user_sessions", of_ada.clone(), "data").await;
        assert_eq!(ids(&ada, "session_id"), ["s3", "s1"]);

        // A cursor continues only the list, filter, user and window it came from.
        let first = |tool: &'static str, arguments: Value| {
            let mut client = Client::modern(&mcp, &key(1));
            async move {
                let output = client.output(tool, arguments).await;
                output["next_cursor"].as_str().unwrap().to_owned()
            }
        };
        let sessions = first("list_sessions", one(days())).await;
        let spans = first("list_spans", one(days())).await;
        let ada = first("list_user_sessions", of_ada.clone()).await;
        let refusals = [
            ("list_users", with(days(), "cursor", json!(sessions))),
            ("list_spans", with(days(), "cursor", json!(sessions))),
            ("list_traces", with(days(), "cursor", json!(spans))),
            (
                "list_spans",
                with(
                    with(days(), "filter", json!({ "level": ["error"] })),
                    "cursor",
                    json!(spans),
                ),
            ),
            (
                "list_user_sessions",
                with(with(days(), "user_id", json!("bob")), "cursor", json!(ada)),
            ),
            (
                "list_sessions",
                with(
                    window(NOW.saturating_add_micros(-DAY), NOW),
                    "cursor",
                    json!(sessions),
                ),
            ),
        ];
        for (tool, arguments) in refusals {
            let problem = client.error(tool, arguments).await;
            assert_eq!(problem["code"], "cursor_invalid", "{tool}: {problem:#}");
            assert_eq!(problem["status"], 400);
        }
        // Another project's key opens none of them.
        let mut other = Client::modern(&mcp, &key(2));
        let problem = other
            .error("list_sessions", with(days(), "cursor", json!(sessions)))
            .await;
        assert_eq!(problem["code"], "cursor_invalid");
    });
}

#[test]
fn a_span_list_too_large_for_one_result_is_cut_under_64_kib_and_continues() {
    run(|env| async move {
        let mut world = World::new(10_000);
        world.ack(
            (1..=40)
                .map(|n| named(n, n, 0, at(-i64::try_from(n).unwrap() * 1_000)))
                .collect(),
        );
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let first = client.output("list_spans", days()).await;
        let rows = first["data"].as_array().unwrap().len();
        assert!((1..40).contains(&rows), "{rows} rows in one result");
        assert_eq!(first["truncated"], true);
        assert_eq!(first["has_more"], true);
        let (spans, calls) = read_all(&mut client, "list_spans", days(), "data").await;
        assert!(calls > 1);
        assert_once(&ids(&spans, "trace_id"), 40);
        for (n, row) in (1..=40).zip(&spans) {
            assert_eq!(row["name"], hostile(n), "the names come back whole");
        }
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
fn list_arguments_are_checked_and_each_refusal_names_what_to_change() {
    run(|env| async move {
        let world = three_sessions();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let cases = [
            (
                "list_spans",
                with(days(), "filter", json!({ "level": ["fatal"] })),
                vec![("/filter/level", "invalid_value")],
            ),
            (
                "list_spans",
                with(
                    days(),
                    "filter",
                    json!({ "observation_type": ["llm"], "metadata": ["=x"] }),
                ),
                vec![
                    ("/filter/observation_type", "invalid_value"),
                    ("/filter/metadata", "invalid_value"),
                ],
            ),
            (
                "list_traces",
                with(days(), "filter", json!({ "tags": vec!["t"; 101] })),
                vec![("/filter/tags", "too_many_values")],
            ),
            (
                "list_traces",
                with(days(), "filter", json!({ "tags": vec!["t"; 21] })),
                vec![("/filter/tags", "too_many_values")],
            ),
            (
                "list_traces",
                with(
                    days(),
                    "filter",
                    json!({ "metadata": ["plan=pro"], "observation_type": ["tool"] }),
                ),
                vec![("/filter/observation_type", "conflict")],
            ),
            (
                "list_spans",
                with(days(), "filter", json!({ "colour": ["red"] })),
                vec![("", "invalid_arguments")],
            ),
            (
                "list_sessions",
                with(days(), "limit", json!(51)),
                vec![("/limit", "out_of_range")],
            ),
            (
                "list_users",
                json!({ "from": NOW.to_string(), "to": NOW.to_string() }),
                vec![("/to", "invalid_window")],
            ),
            (
                "list_user_sessions",
                with(days(), "user_id", json!("")),
                vec![("/user_id", "invalid_id")],
            ),
            (
                "list_user_sessions",
                days(),
                vec![("", "invalid_arguments")],
            ),
        ];
        for (tool, arguments, expected) in cases {
            let problem = client.error(tool, arguments.clone()).await;
            assert_eq!(problem["code"], "validation_failed", "{tool} {arguments}");
            assert_eq!(pointers(&problem), expected, "{tool} {arguments}");
        }
        // A window longer than the project's maximum says which setting sets it.
        let wide = window(NOW.saturating_add_micros(-40 * DAY), NOW);
        let problem = client.error("list_sessions", wide).await;
        assert_eq!(problem["code"], "query_too_broad");
        assert_eq!(problem["limit"]["name"], "max_window_days");
    });
}

#[test]
fn the_list_tools_share_the_per_key_rate_limit() {
    run(|env| async move {
        let world = three_sessions();
        let config = McpConfig {
            calls_per_second: 1,
            burst: 4,
        };
        let limited = mcp_with(&env, &world, config);
        let mut client = Client::modern(&limited, &key(1));
        client.output("list_spans", days()).await;
        client.output("list_sessions", days()).await;
        client.output("list_users", days()).await;
        let of_ada = with(days(), "user_id", json!("ada"));
        client.output("list_user_sessions", of_ada).await;
        let problem = client.error("list_spans", days()).await;
        assert_eq!(problem["code"], "rate_limited", "{problem:#}");
    });
}
