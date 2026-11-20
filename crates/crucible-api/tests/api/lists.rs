//! The sessions, users and span list routes, and the list filters of the trace list and the span list: their
//! wire shapes (with snapshots), paging and cursor binding, validation problems with pointers, and the answer
//! before a store is configured (contract 8.7, 8.8, 12.3, 12.4 and 12.6).

use std::sync::Arc;

use axum::http::StatusCode;
use crucible_recent::SpanEntry;
use crucible_records::{Level, ObservationType, Timestamp};
use serde_json::Value;

use crate::world::{NOW, World, span, trace};
use crate::{api, api_without_store, get, path, run, window};

const SECOND: i64 = 1_000_000;
const DAY: i64 = 86_400 * SECOND;

fn ago(seconds: i64) -> Timestamp {
    NOW.saturating_add_micros(-seconds * SECOND)
}

/// The window of every request here: the day before [`NOW`] and the hour after it.
fn day() -> String {
    window(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(3_600 * SECOND),
    )
}

fn uri(rest: &str, extra: &str) -> String {
    format!("{}?{}{extra}", path(1, rest), day())
}

fn of(mut entry: SpanEntry, session: &str, user: &str) -> SpanEntry {
    entry.summary.session_id = Arc::from(session);
    entry.summary.user_id = Arc::from(user);
    entry
}

/// Trace 1 (ada, session `s 1`, a root and an erring tool span), trace 2 (bob, `s2`, a generation), trace 3
/// (ada, `s2`); all in the store.
fn world() -> World {
    let mut world = World::new(100);
    let mut tool = of(span(1, 11, 10, ago(59)), "s 1", "ada");
    tool.summary.observation_type = ObservationType::Tool;
    tool.summary.level = Level::Error;
    world.ack(vec![of(span(1, 10, 0, ago(60)), "s 1", "ada"), tool]);
    let mut generation = of(span(2, 21, 20, ago(39)), "s2", "bob");
    generation.summary.observation_type = ObservationType::Generation;
    world.ack(vec![of(span(2, 20, 0, ago(40)), "s2", "bob"), generation]);
    world.ack(vec![of(span(3, 30, 0, ago(20)), "s2", "ada")]);
    world.settle();
    world
}

fn field(list: &Value, name: &str) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item[name].as_str().unwrap().to_owned())
        .collect()
}

fn assert_whole(body: &Value) {
    assert_eq!(body["meta"]["partial"], false, "{body:#}");
    assert_eq!(body["meta"]["consistency"], "strong", "{body:#}");
}

#[test]
fn sessions_and_users_answer_with_their_wire_shapes() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);

        let sessions = get(&api, &uri("/sessions", "")).await;
        assert_eq!(sessions.status, StatusCode::OK, "{:#}", sessions.body);
        assert_eq!(field(&sessions.body["data"], "session_id"), ["s2", "s 1"]);
        assert_eq!(sessions.body["next_cursor"], Value::Null);
        assert_whole(&sessions.body);
        insta::assert_snapshot!("list_sessions", sessions.pretty());

        // A session id is a path segment, percent-encoded.
        let one = get(&api, &uri("/sessions/s%201", "")).await;
        assert_eq!(one.status, StatusCode::OK, "{:#}", one.body);
        assert_eq!(one.body["session"], sessions.body["data"][1]);
        assert_whole(&one.body);
        insta::assert_snapshot!("get_session", one.pretty());
        get(&api, &uri("/sessions/nope", ""))
            .await
            .assert_problem(404, "not_found");

        let users = get(&api, &uri("/users", "")).await;
        assert_eq!(users.status, StatusCode::OK, "{:#}", users.body);
        assert_eq!(field(&users.body["data"], "user_id"), ["ada", "bob"]);
        assert_eq!(users.body["data"][0]["session_count"], 2);
        assert_whole(&users.body);
        insta::assert_snapshot!("list_users", users.pretty());
        let ada = get(&api, &uri("/users/ada", "")).await;
        assert_eq!(ada.body["user"], users.body["data"][0]);
        insta::assert_snapshot!("get_user", ada.pretty());
        get(&api, &uri("/users/zed", ""))
            .await
            .assert_problem(404, "not_found");

        let theirs = get(&api, &uri("/users/ada/sessions", "")).await;
        assert_eq!(theirs.status, StatusCode::OK, "{:#}", theirs.body);
        assert_eq!(field(&theirs.body["data"], "session_id"), ["s2", "s 1"]);
        insta::assert_snapshot!("list_user_sessions", theirs.pretty());

        // The user's traces and the session's traces are the trace list under a filter.
        let traces = get(&api, &uri("/traces", "&user_id=ada")).await;
        assert_eq!(
            field(&traces.body["data"], "trace_id"),
            [trace(3).to_string(), trace(1).to_string()]
        );
        let traces = get(&api, &uri("/traces", "&session_id=s2")).await;
        assert_eq!(
            field(&traces.body["data"], "trace_id"),
            [trace(3).to_string(), trace(2).to_string()]
        );
    });
}

#[test]
fn the_span_list_and_the_trace_filters_answer_with_their_wire_shapes() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);

        let spans = get(&api, &uri("/spans", "&limit=3")).await;
        assert_eq!(spans.status, StatusCode::OK, "{:#}", spans.body);
        assert_eq!(
            field(&spans.body["data"], "span_id"),
            ["000000000000001e", "0000000000000015", "0000000000000014"]
        );
        assert_eq!(spans.body["has_more"], true);
        assert_whole(&spans.body);
        insta::assert_snapshot!("list_spans", spans.pretty());

        let errors = get(&api, &uri("/spans", "&level=error")).await;
        assert_eq!(field(&errors.body["data"], "span_id"), ["000000000000000b"]);
        let typed = get(
            &api,
            &uri(
                "/spans",
                "&observation_type=tool&observation_type=generation",
            ),
        )
        .await;
        assert_eq!(
            field(&typed.body["data"], "span_id"),
            ["0000000000000015", "000000000000000b"]
        );
        let named = get(&api, &uri("/spans", "&name=SPAN-3")).await;
        assert_eq!(field(&named.body["data"], "span_id"), ["000000000000001e"]);

        // On the trace list, an observation type keeps the traces with such a span.
        let traces = get(&api, &uri("/traces", "&observation_type=tool")).await;
        assert_eq!(traces.status, StatusCode::OK, "{:#}", traces.body);
        assert_eq!(
            field(&traces.body["data"], "trace_id"),
            [trace(1).to_string()]
        );
        let both = get(&api, &uri("/traces", "&user_id=ada&environment=default")).await;
        assert_eq!(both.body["data"].as_array().unwrap().len(), 2);
    });
}

#[test]
fn list_cursors_page_and_are_bound_to_their_filters_and_scope() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);

        // Sessions page by last seen.
        let first = get(&api, &uri("/sessions", "&limit=1")).await;
        let next = first.body["next_cursor"].as_str().unwrap().to_owned();
        let second = get(&api, &uri("/sessions", &format!("&limit=1&cursor={next}"))).await;
        assert_eq!(field(&second.body["data"], "session_id"), ["s 1"]);
        assert_eq!(second.body["next_cursor"], Value::Null);
        // A session cursor is not a user cursor.
        get(&api, &uri("/users", &format!("&cursor={next}")))
            .await
            .assert_problem(400, "cursor_invalid");

        // The sessions of a user are bound to the user.
        let first = get(&api, &uri("/users/ada/sessions", "&limit=1")).await;
        let next = first.body["next_cursor"].as_str().unwrap().to_owned();
        let second = get(
            &api,
            &uri("/users/ada/sessions", &format!("&limit=1&cursor={next}")),
        )
        .await;
        assert_eq!(field(&second.body["data"], "session_id"), ["s 1"]);
        get(
            &api,
            &uri("/users/bob/sessions", &format!("&cursor={next}")),
        )
        .await
        .assert_problem(400, "cursor_invalid");

        // Span and trace cursors are bound to the filter: the same filter written differently opens them,
        // another filter does not.
        let first = get(&api, &uri("/spans", "&limit=1&user_id=ada&user_id=bob")).await;
        let next = first.body["next_cursor"].as_str().unwrap().to_owned();
        let reordered = get(
            &api,
            &uri(
                "/spans",
                &format!("&limit=1&user_id=bob&user_id=ada&cursor={next}"),
            ),
        )
        .await;
        assert_eq!(reordered.status, StatusCode::OK, "{:#}", reordered.body);
        get(&api, &uri("/spans", &format!("&limit=1&cursor={next}")))
            .await
            .assert_problem(400, "cursor_invalid");
        let first = get(&api, &uri("/traces", "&limit=1&user_id=ada")).await;
        let next = first.body["next_cursor"].as_str().unwrap().to_owned();
        let head = first.body["head_cursor"].as_str().unwrap().to_owned();
        let second = get(
            &api,
            &uri("/traces", &format!("&limit=1&user_id=ada&cursor={next}")),
        )
        .await;
        assert_eq!(
            field(&second.body["data"], "trace_id"),
            [trace(1).to_string()]
        );
        get(&api, &uri("/traces", &format!("&limit=1&cursor={next}")))
            .await
            .assert_problem(400, "cursor_invalid");
        let tail = get(&api, &uri("/traces", &format!("&user_id=ada&since={head}"))).await;
        assert_eq!(tail.status, StatusCode::OK, "{:#}", tail.body);
        assert_eq!(tail.body["new_count"], 0);
        get(&api, &uri("/traces", &format!("&user_id=bob&since={head}")))
            .await
            .assert_problem(400, "cursor_invalid");
    });
}

#[test]
fn invalid_filters_and_ids_are_validation_problems_with_pointers() {
    run(|env| async move {
        let world = world();
        let api = api(&env, &world);
        let cases = [
            ("/traces", "&level=fatal", vec![("/level", "invalid_value")]),
            (
                "/spans",
                "&observation_type=robot",
                vec![("/observation_type", "invalid_value")],
            ),
            (
                "/spans",
                "&metadata=no-equals",
                vec![("/metadata", "invalid_value")],
            ),
            (
                "/spans",
                "&metadata==value",
                vec![("/metadata", "invalid_value")],
            ),
            ("/traces", "&name=a&name=b", vec![("/name", "repeated")]),
            (
                "/traces",
                "&metadata=k=v&observation_type=tool",
                vec![("/metadata", "conflict")],
            ),
            (
                "/sessions",
                "&level=error",
                vec![("/level", "unknown_parameter")],
            ),
            ("/users", "&limit=0", vec![("/limit", "out_of_range")]),
        ];
        for (route, extra, expected) in cases {
            let reply = get(&api, &uri(route, extra)).await;
            reply.assert_problem(422, "validation_failed");
            assert_eq!(reply.field_errors(), expected, "{route}{extra}");
        }
        let many = "&tags=t".repeat(101);
        let reply = get(&api, &uri("/spans", &many)).await;
        assert_eq!(reply.field_errors(), [("/tags", "too_many_values")]);
        // The trace list resolves each tag over the traces' tag edits, so it takes at most 20.
        let tags = |n: usize| (0..n).map(|i| format!("&tags=t{i}")).collect::<String>();
        let reply = get(&api, &uri("/traces", &tags(21))).await;
        reply.assert_problem(422, "validation_failed");
        assert_eq!(reply.field_errors(), [("/tags", "too_many_values")]);
        assert!(
            reply.body.to_string().contains("at most 20 tags"),
            "{:#}",
            reply.body
        );
        assert_eq!(
            get(&api, &uri("/traces", &tags(20))).await.status.as_u16(),
            200
        );
        let long = "x".repeat(513);
        let reply = get(&api, &uri(&format!("/users/{long}"), "")).await;
        assert_eq!(reply.field_errors(), [("/user_id", "invalid_id")]);
        // Detail routes need a window too.
        let reply = get(&api, &path(1, "/sessions/s2")).await;
        assert_eq!(
            reply.field_errors(),
            [("/from", "required"), ("/to", "required")]
        );
    });
}

#[test]
fn without_a_store_list_requests_are_checked_and_then_unavailable() {
    run(|env| async move {
        let api = api_without_store(&env);
        for rest in [
            "/sessions",
            "/sessions/s2",
            "/users",
            "/users/ada",
            "/users/ada/sessions",
            "/spans",
        ] {
            get(&api, &uri(rest, ""))
                .await
                .assert_problem(503, "telemetry_store_unavailable");
        }
        get(&api, &uri("/spans", "&level=fatal"))
            .await
            .assert_problem(422, "validation_failed");
        get(&api, &uri("/sessions", "&cursor=v1.x.y.z"))
            .await
            .assert_problem(400, "cursor_invalid");
    });
}
