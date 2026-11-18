//! Each endpoint's success shape (with snapshots of its JSON), paging and live-tail cursors, and the `meta`
//! object when the store is down or stage B lags (contract 8.6 to 8.8 and 12.4).

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::Value;

use crate::world::{GRACE, NOW, World, span, trace};
use crate::{api, get, path, run, window};

const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;
const DAY: i64 = 24 * HOUR;

fn ago(seconds: i64) -> crucible_records::Timestamp {
    NOW.saturating_add_micros(-seconds * SECOND)
}

/// The window of every list request here: the day before [`NOW`] and the hour after it.
fn day() -> String {
    window(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(HOUR),
    )
}

fn traces_uri(extra: &str) -> String {
    format!("{}?{}{extra}", path(1, "/traces"), day())
}

/// Lets the grace pass once more, so the index evicts what the store serves.
fn forget_settled(world: &mut World) {
    world.clock.advance(GRACE);
    world.writer.maintain();
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
    forget_settled(&mut world);
    world.ack(vec![span(2, 20, 0, ago(30)), span(2, 21, 20, ago(29))]);
    world
}

fn ids(list: &Value, field: &str) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item[field].as_str().unwrap().to_owned())
        .collect()
}

fn assert_whole(body: &Value) {
    assert_eq!(body["meta"]["partial"], false, "{body:#}");
    assert_eq!(body["meta"]["consistency"], "strong", "{body:#}");
    assert!(body["meta"].get("partial_reason").is_none(), "{body:#}");
}

#[test]
fn every_endpoint_answers_with_its_wire_shape() {
    run(|env| async move {
        let world = two_traces();
        let api = api(&env, &world);

        let list = get(&api, &traces_uri("")).await;
        assert_eq!(list.status, StatusCode::OK, "{:#}", list.body);
        assert_eq!(list.header("content-type"), Some("application/json"));
        assert_eq!(
            ids(&list.body["data"], "trace_id"),
            [trace(2).to_string(), trace(1).to_string()]
        );
        assert_eq!(list.body["has_more"], false);
        assert_eq!(list.body["next_cursor"], Value::Null);
        assert!(
            list.body["head_cursor"]
                .as_str()
                .unwrap()
                .starts_with("tmp1.h.")
        );
        assert!(list.body.get("new_count").is_none());
        assert_whole(&list.body);
        insta::assert_snapshot!("list_traces", list.pretty());

        let detail = get(&api, &path(1, &format!("/traces/{}", trace(1)))).await;
        assert_eq!(detail.status, StatusCode::OK, "{:#}", detail.body);
        assert_eq!(detail.body["span_count"], 4);
        assert_eq!(detail.body["paged"], false);
        let spans = &detail.body["spans"];
        assert_eq!(
            ids(spans, "span_id"),
            [
                "000000000000000a",
                "000000000000000b",
                "000000000000000c",
                "000000000000000d"
            ]
        );
        assert_eq!(spans[0]["parent_span_id"], Value::Null);
        assert_eq!(spans[3]["parent_span_id"], "000000000000000b");
        assert_eq!(spans[0]["child_count"], Value::Null);
        assert_whole(&detail.body);
        insta::assert_snapshot!("trace_detail", detail.pretty());
        // `hint_time` is accepted in place of a window, and changes nothing.
        let hinted = path(1, &format!("/traces/{}?hint_time={}", trace(1), ago(60)));
        assert_eq!(get(&api, &hinted).await.body, detail.body);

        let children = get(
            &api,
            &path(
                1,
                &format!("/traces/{}/spans?parent_span_id=000000000000000a", trace(1)),
            ),
        )
        .await;
        assert_eq!(children.status, StatusCode::OK, "{:#}", children.body);
        assert_eq!(
            ids(&children.body["data"], "span_id"),
            ["000000000000000b", "000000000000000c"]
        );
        assert_eq!(children.body["data"][0]["child_count"], 1);
        assert_eq!(children.body["data"][1]["child_count"], 0);
        assert_whole(&children.body);
        insta::assert_snapshot!("span_children", children.pretty());

        let bounds = get(&api, &path(1, "/data-bounds")).await;
        assert_eq!(bounds.status, StatusCode::OK, "{:#}", bounds.body);
        assert_eq!(bounds.body["max_window_days"], 30);
        assert_eq!(bounds.body["retention_horizon"], Value::Null);
        assert_eq!(bounds.body["server_time"], bounds.body["meta"]["as_of"]);
        assert_eq!(bounds.body["mcp"]["client"]["first"], Value::Null);
        assert_whole(&bounds.body);
        insta::assert_snapshot!("data_bounds", bounds.pretty());
    });
}

#[test]
fn pages_follow_next_cursor_and_cursors_stay_with_their_request() {
    run(|env| async move {
        let mut world = World::new(100);
        for n in 1..=5 {
            world.ack(vec![span(n, 10 * n, 0, ago(100 - n as i64))]);
        }
        world.settle();
        let api = api(&env, &world);

        let mut seen = Vec::new();
        let mut cursor = None::<String>;
        loop {
            let extra =
                cursor.map_or_else(|| "&limit=2".to_owned(), |c| format!("&limit=2&cursor={c}"));
            let page = get(&api, &traces_uri(&extra)).await;
            assert_eq!(page.status, StatusCode::OK, "{:#}", page.body);
            seen.extend(ids(&page.body["data"], "trace_id"));
            // Only the first page issues a head cursor.
            assert_eq!(page.body["head_cursor"].is_string(), seen.len() <= 2);
            match page.body["next_cursor"].as_str() {
                Some(next) => {
                    assert_eq!(page.body["has_more"], true);
                    cursor = Some(next.to_owned());
                }
                None => break,
            }
        }
        let expected: Vec<String> = (1..=5).rev().map(|n| trace(n).to_string()).collect();
        assert_eq!(seen, expected);

        // A cursor is bound to its window.
        let first = get(&api, &traces_uri("&limit=2")).await;
        let next = first.body["next_cursor"].as_str().unwrap();
        let other_window = window(
            NOW.saturating_add_micros(-HOUR),
            NOW.saturating_add_micros(HOUR),
        );
        let moved = get(
            &api,
            &format!("{}?{other_window}&cursor={next}", path(1, "/traces")),
        )
        .await;
        moved.assert_problem(400, "cursor_invalid");
        // A head cursor is not a page cursor.
        let head = first.body["head_cursor"].as_str().unwrap();
        get(&api, &traces_uri(&format!("&cursor={head}")))
            .await
            .assert_problem(400, "cursor_invalid");
    });
}

#[test]
fn live_tail_returns_what_arrived_after_the_head_cursor() {
    run(|env| async move {
        let mut world = World::new(100);
        world.ack(vec![span(1, 10, 0, ago(60))]);
        let api = api(&env, &world);
        let first = get(&api, &traces_uri("")).await;
        let head = first.body["head_cursor"].as_str().unwrap().to_owned();

        world.ack(vec![span(2, 20, 0, ago(30))]);
        world.ack(vec![span(3, 30, 0, ago(20))]);
        let tail = get(&api, &traces_uri(&format!("&since={head}"))).await;
        assert_eq!(tail.status, StatusCode::OK, "{:#}", tail.body);
        assert_eq!(
            ids(&tail.body["data"], "trace_id"),
            [trace(3).to_string(), trace(2).to_string()]
        );
        assert_eq!(tail.body["new_count"], 2);
        assert_eq!(tail.body["has_more"], false);
        assert_eq!(tail.body["next_cursor"], Value::Null);
        let next_head = tail.body["head_cursor"].as_str().unwrap();
        assert_ne!(next_head, head);
        insta::assert_snapshot!("live_tail", tail.pretty());

        // Nothing new since the last response.
        let quiet = get(&api, &traces_uri(&format!("&since={next_head}"))).await;
        assert_eq!(quiet.body["new_count"], 0);
        assert_eq!(quiet.body["data"], Value::Array(Vec::new()));
        // `since` and `cursor` do not combine.
        let both = get(&api, &traces_uri(&format!("&since={head}&cursor={head}"))).await;
        both.assert_problem(422, "validation_failed");
        assert_eq!(both.field_errors(), [("/since", "conflict")]);
    });
}

#[test]
fn meta_says_when_the_store_is_down_and_when_stage_b_lags() {
    run(|env| async move {
        let mut world = two_traces();
        let api = api(&env, &world);
        world.store.set_unavailable(true);

        // The index still answers for trace 2, and the response says rows may be missing.
        let list = get(&api, &traces_uri("")).await;
        assert_eq!(list.status, StatusCode::OK, "{:#}", list.body);
        assert_eq!(ids(&list.body["data"], "trace_id"), [trace(2).to_string()]);
        assert_eq!(list.body["meta"]["partial"], true);
        assert_eq!(list.body["meta"]["partial_reason"], "store_unavailable");
        let detail = get(&api, &path(1, &format!("/traces/{}", trace(2)))).await;
        assert_eq!(detail.body["meta"]["partial_reason"], "store_unavailable");
        assert_eq!(detail.body["span_count"], 2);
        let bounds = get(&api, &path(1, "/data-bounds")).await;
        assert_eq!(bounds.body["meta"]["partial"], true);

        // Nothing in the index for trace 1: the store's outage is the answer.
        get(&api, &path(1, &format!("/traces/{}", trace(1))))
            .await
            .assert_problem(503, "telemetry_store_unavailable");

        // Stage B has not applied an acknowledged frame: lists and bounds say they may trail.
        world.store.set_unavailable(false);
        world.ack_only(vec![span(3, 30, 0, ago(10))]);
        let list = get(&api, &traces_uri("")).await;
        assert_eq!(
            list.body["meta"]["consistency"], "lagging",
            "{:#}",
            list.body
        );
        assert_eq!(list.body["meta"]["partial"], false);
        let bounds = get(&api, &path(1, "/data-bounds")).await;
        assert_eq!(bounds.body["meta"]["consistency"], "lagging");
        let children = get(
            &api,
            &path(
                1,
                &format!("/traces/{}/spans?parent_span_id=0000000000000014", trace(2)),
            ),
        )
        .await;
        assert_eq!(
            children.body["meta"]["consistency"], "lagging",
            "{:#}",
            children.body
        );
    });
}

#[test]
fn a_read_stuck_behind_the_read_barrier_ends_with_request_timeout() {
    run(|env| async move {
        let mut world = World::new(100);
        world.ack(vec![span(1, 10, 0, ago(60))]);
        // Acknowledged, and stage B never applies it: trace by id waits on the barrier, whose clock never moves.
        world.ack_only(vec![span(1, 11, 10, ago(59))]);
        let api = api(&env, &world);
        let started = env.clock.now();
        let stuck = get(&api, &path(1, &format!("/traces/{}", trace(1)))).await;
        stuck.assert_problem(408, "request_timeout");
        let waited = env.clock.now().saturating_duration_since(started);
        assert!(
            (Duration::from_secs(30)..Duration::from_secs(31)).contains(&waited),
            "{waited:?}"
        );
    });
}
