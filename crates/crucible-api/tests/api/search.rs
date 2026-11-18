//! `POST projects/{project_id}/search`: the hits with their highlights, pages through signed cursors, and the
//! problems of a body that is not a search (contract 8.7, 12.3, 12.4 and 12.6).

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use crucible_records::TableId;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::world::{NOW, World, span, span_payload_row};
use crate::{Reply, api, api_without_store, key, path, run, send};

const MINUTE: i64 = 60_000_000;
const DAY: i64 = 86_400_000_000;

fn at(micros: i64) -> String {
    NOW.saturating_add_micros(micros).to_string()
}

/// A POST of `body` with project 1's key, or without a key.
async fn post(router: &Router, body: &str, key: Option<&str>) -> Reply {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path(1, "/search"))
        .header(CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        request = request.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn search(router: &Router, body: &Value) -> Reply {
    post(router, &body.to_string(), Some(&key())).await
}

/// Three spans of trace 1 a minute apart, newest first; the second's input does not mention a refund.
fn refunds() -> World {
    let mut world = World::new(100);
    let inputs = [
        "The customer asked for a Refund of order 7",
        "Order 8 shipped",
        "refund issued for order 9; a refund was due",
    ];
    let entries: Vec<_> = (0..3)
        .map(|i| {
            span(
                1,
                10 + i,
                0,
                NOW.saturating_add_micros(-(i as i64) * MINUTE),
            )
        })
        .collect();
    let lsn = world.ack(entries.clone());
    for (entry, input) in entries.iter().zip(inputs) {
        let row = span_payload_row(entry, lsn, NOW, input, &["tenant=acme"]);
        world.store.insert(TableId::SpanPayload, row);
    }
    world.settle();
    world
}

fn request(query: &str) -> Value {
    json!({ "query": query, "from": at(-DAY), "to": at(DAY) })
}

#[test]
fn hits_come_newest_first_with_highlights_and_page_by_cursor() {
    run(|env| async move {
        let world = refunds();
        let api = api(&env, &world);
        let mut body = request("REFUND");
        body["limit"] = json!(1);
        let first = search(&api, &body).await;
        assert_eq!(first.status, StatusCode::OK, "{:#}", first.body);
        assert_eq!(first.header("content-type"), Some("application/json"));
        insta::assert_snapshot!("search_first_page", first.pretty());
        assert_eq!(first.body["has_more"], true);

        body["cursor"] = first.body["next_cursor"].clone();
        let second = search(&api, &body).await;
        assert_eq!(second.status, StatusCode::OK, "{:#}", second.body);
        let hit = &second.body["data"][0];
        assert_eq!(hit["span"]["span_id"], "000000000000000c");
        let fragments: Vec<(&str, bool)> = hit["matches"][0]["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["text"].as_str().unwrap(),
                    f["highlighted"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            fragments,
            [
                ("refund", true),
                (" issued for order 9; a ", false),
                ("refund", true),
                (" was due", false)
            ]
        );
        assert_eq!(second.body["has_more"], false);
        assert_eq!(second.body["next_cursor"], Value::Null);

        // A cursor is bound to its query: another query restarts from the first page.
        let mut other = request("order");
        other["cursor"] = first.body["next_cursor"].clone();
        search(&api, &other)
            .await
            .assert_problem(400, "cursor_invalid");
        // Metadata entries match whole.
        let mut entries = request("tenant=acme");
        entries["fields"] = json!(["metadata"]);
        let found = search(&api, &entries).await;
        assert_eq!(
            found.body["data"].as_array().unwrap().len(),
            3,
            "{:#}",
            found.body
        );
        assert_eq!(found.body["data"][0]["matches"][0]["field"], "metadata");
        // A phrase.
        let mut phrase = request("refund issued");
        phrase["mode"] = json!("phrase");
        let found = search(&api, &phrase).await;
        assert_eq!(
            found.body["data"].as_array().unwrap().len(),
            1,
            "{:#}",
            found.body
        );
    });
}

#[test]
fn bodies_that_are_not_a_search_are_validation_problems_with_pointers() {
    run(|env| async move {
        let world = refunds();
        let api = api(&env, &world);
        let refused = |reply: &Reply| -> Vec<(String, String)> {
            reply.assert_problem(422, "validation_failed");
            reply
                .field_errors()
                .into_iter()
                .map(|(pointer, code)| (pointer.to_owned(), code.to_owned()))
                .collect()
        };
        let pair = |pointer: &str, code: &str| (pointer.to_owned(), code.to_owned());
        assert_eq!(
            refused(&post(&api, "{", Some(&key())).await),
            [pair("", "invalid")]
        );
        assert_eq!(
            refused(&post(&api, "{} []", Some(&key())).await)[0].1,
            "invalid"
        );
        let mut extra = request("refund");
        extra["filters"] = json!({});
        assert_eq!(refused(&search(&api, &extra).await)[0].1, "invalid");
        let mut name = request("refund");
        name["fields"] = json!(["input", "name"]);
        assert_eq!(
            refused(&search(&api, &name).await),
            [pair("/fields/1", "invalid")]
        );
        let mut bad = request("!!! ...");
        bad["fields"] = json!(["input", "input"]);
        bad["limit"] = json!(201);
        bad["from"] = json!("yesterday");
        assert_eq!(
            refused(&search(&api, &bad).await),
            [
                pair("/from", "invalid_timestamp"),
                pair("/fields", "repeated"),
                pair("/query", "no_tokens"),
                pair("/limit", "out_of_range"),
            ]
        );
        let mut backwards = request("refund");
        backwards["to"] = json!(at(-2 * DAY));
        assert_eq!(
            refused(&search(&api, &backwards).await),
            [pair("/to", "invalid_window")]
        );
        let long = request(&"a ".repeat(600));
        assert_eq!(
            refused(&search(&api, &long).await),
            [pair("/query", "too_long")]
        );
        let words: Vec<String> = (0..65).map(|i| format!("w{i}")).collect();
        assert_eq!(
            refused(&search(&api, &request(&words.join(" "))).await),
            [pair("/query", "too_many_tokens")]
        );
        // Metadata takes `key=value` entries; text fields with them need a letter or digit too, but one kind
        // with tokens is enough.
        let mut entries = request("refund");
        entries["fields"] = json!(["metadata"]);
        assert_eq!(
            refused(&search(&api, &entries).await),
            [pair("/query", "no_tokens")]
        );
        entries["fields"] = json!(["metadata", "input"]);
        assert_eq!(search(&api, &entries).await.status, StatusCode::OK);
        // A window above the project's maximum is too broad, with the limit.
        let mut wide = request("refund");
        wide["from"] = json!(at(-40 * DAY));
        let reply = search(&api, &wide).await;
        reply.assert_problem(422, "query_too_broad");
        assert_eq!(reply.body["limit"]["name"], "max_window_days");
        // A tampered cursor.
        let mut tampered = request("refund");
        tampered["cursor"] = json!("v1.test.AAAA.BBBB");
        search(&api, &tampered)
            .await
            .assert_problem(400, "cursor_invalid");
    });
}

#[test]
fn the_route_takes_post_only_and_checks_the_key_and_the_store() {
    run(|env| async move {
        let world = refunds();
        let api = api(&env, &world);
        let reply = send(&api, Method::GET, &path(1, "/search"), Some(&key())).await;
        reply.assert_problem(405, "method_not_allowed");
        assert_eq!(reply.header("allow"), Some("POST"));
        post(&api, &request("refund").to_string(), None)
            .await
            .assert_problem(401, "unauthenticated");
        // Without a store the request is still checked, then refused as unavailable.
        let bare = api_without_store(&env);
        post(&bare, "{", Some(&key()))
            .await
            .assert_problem(422, "validation_failed");
        search(&bare, &request("refund"))
            .await
            .assert_problem(503, "telemetry_store_unavailable");
        // And with the store down.
        world.store.set_unavailable(true);
        search(&api, &request("refund"))
            .await
            .assert_problem(503, "telemetry_store_unavailable");
    });
}
