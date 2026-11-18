//! Problems: authentication, visibility, validation, unknown routes, the body limit and the API before a store
//! is configured; and the request id and security headers on every response (contract 11.6, 12.1 to 12.4).

use axum::body::Body;
use axum::http::header::CONTENT_LENGTH;
use axum::http::{Method, Request};
use crucible_records::{RequestId, Timestamp};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use crate::world::{NOW, World, trace};
use crate::{api, api_without_store, get, key, other_key, path, run, send, window};

const DAY: i64 = 86_400_000_000;

fn days(n: i64) -> Timestamp {
    NOW.saturating_add_micros(n * DAY)
}

fn traces(query: &str) -> String {
    format!("{}?{query}", path(1, "/traces"))
}

#[test]
fn requests_without_a_valid_bearer_key_are_unauthenticated() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let uri = traces(&window(days(-1), NOW));
        let unknown = format!("csk_{}", "3".repeat(43));
        for key in [None, Some("csk_short"), Some(unknown.as_str())] {
            let reply = send(&api, Method::GET, &uri, key).await;
            reply.assert_problem(401, "unauthenticated");
            assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
        }
        // Basic auth is not a milestone 1 form, and the scheme must be Bearer.
        let request = Request::get(uri.as_str())
            .header("authorization", format!("Basic {}", key()))
            .body(Body::empty())
            .unwrap();
        let response = api.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 401);
        // Authentication comes before the path: an invalid id is still 401 without a key.
        send(&api, Method::GET, "/api/v1/projects/nope/traces", None)
            .await
            .assert_problem(401, "unauthenticated");
    });
}

#[test]
fn a_project_the_key_cannot_see_is_not_found() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let query = window(days(-1), NOW);
        // Project 2 exists and has a key of its own; project 1's key cannot see it.
        for uri in [
            format!("{}?{query}", path(2, "/traces")),
            path(2, &format!("/traces/{}", trace(1))),
            path(2, "/data-bounds"),
            format!("/api/v1/projects/not-a-uuid/traces?{query}"),
            format!("/api/v1/projects/00000000-0000-0000-0000-000000000000/traces?{query}"),
        ] {
            get(&api, &uri).await.assert_problem(404, "not_found");
        }
        // The project's own key sees it.
        let own = send(
            &api,
            Method::GET,
            &path(2, "/data-bounds"),
            Some(&other_key()),
        )
        .await;
        assert_eq!(own.status, 200, "{:#}", own.body);
    });
}

#[test]
fn invalid_parameters_are_validation_problems_with_pointers() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);

        let missing = get(&api, &path(1, "/traces")).await;
        missing.assert_problem(422, "validation_failed");
        assert_eq!(
            missing.field_errors(),
            [("/from", "required"), ("/to", "required")]
        );

        let malformed = get(
            &api,
            &traces("from=yesterday&to=2026-11-17T09:30:00Z&limit=0"),
        )
        .await;
        assert_eq!(
            malformed.field_errors(),
            [("/from", "invalid_timestamp"), ("/limit", "out_of_range")]
        );

        let reversed = get(&api, &traces(&window(NOW, days(-1)))).await;
        assert_eq!(reversed.field_errors(), [("/to", "invalid_window")]);

        let unknown = get(
            &api,
            &traces(&format!("{}&sort=name&limit=201", window(days(-1), NOW))),
        )
        .await;
        assert_eq!(
            unknown.field_errors(),
            [("/limit", "out_of_range"), ("/sort", "unknown_parameter")]
        );

        let repeated = get(
            &api,
            &traces(&format!("{}&from={}", window(days(-1), NOW), days(-2))),
        )
        .await;
        assert_eq!(repeated.field_errors(), [("/from", "repeated")]);

        let ancient = get(
            &api,
            &traces("from=1969-12-31T00:00:00Z&to=1970-01-02T00:00:00Z"),
        )
        .await;
        assert_eq!(ancient.field_errors(), [("/from", "out_of_range")]);

        let detail = get(&api, &path(1, "/traces/xyz?hint_time=2026-11-17T09:30:00Z")).await;
        detail.assert_problem(422, "validation_failed");
        assert_eq!(
            detail.field_errors(),
            [
                ("/trace_id", "invalid_id"),
                ("/hint_time", "unknown_parameter")
            ]
        );

        let spans = format!("/traces/{}/spans", trace(1));
        let no_parent = get(&api, &path(1, &spans)).await;
        assert_eq!(no_parent.field_errors(), [("/parent_span_id", "required")]);
        let bad_parent = get(
            &api,
            &path(1, &format!("{spans}?parent_span_id=12&limit=2001")),
        )
        .await;
        assert_eq!(
            bad_parent.field_errors(),
            [
                ("/parent_span_id", "invalid_id"),
                ("/limit", "out_of_range")
            ]
        );

        let bounds = get(&api, &path(1, "/data-bounds?from=x")).await;
        assert_eq!(bounds.field_errors(), [("/from", "unknown_parameter")]);
    });
}

#[test]
fn windows_above_the_project_maximum_are_too_broad() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        // Exactly the maximum is allowed: the UI's widest range is 30 days.
        let thirty = get(&api, &traces(&window(days(-30), NOW))).await;
        assert_eq!(thirty.status, 200, "{:#}", thirty.body);
        let wider = get(
            &api,
            &traces(&window(days(-30).saturating_add_micros(-1), NOW)),
        )
        .await;
        wider.assert_problem(422, "query_too_broad");
        assert_eq!(wider.body["limit"]["name"], "max_window_days");
        assert_eq!(wider.body["limit"]["value"], 30);
        assert!(wider.body.get("errors").is_none());
    });
}

#[test]
fn malformed_cursors_are_cursor_invalid() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let query = window(days(-1), NOW);
        for cursor in ["garbage", "tmp1.p.AAAA", "v1.k.AAAA.BBBB"] {
            get(&api, &traces(&format!("{query}&cursor={cursor}")))
                .await
                .assert_problem(400, "cursor_invalid");
            get(&api, &traces(&format!("{query}&since={cursor}")))
                .await
                .assert_problem(400, "cursor_invalid");
        }
        let spans = path(
            1,
            &format!(
                "/traces/{}/spans?parent_span_id=000000000000000a&cursor=x",
                trace(1)
            ),
        );
        get(&api, &spans)
            .await
            .assert_problem(400, "cursor_invalid");
    });
}

#[test]
fn unknown_paths_and_methods_are_problems() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        get(&api, "/api/v1/nope")
            .await
            .assert_problem(404, "not_found");
        get(&api, &path(1, "/sessions"))
            .await
            .assert_problem(404, "not_found");
        let post = send(&api, Method::POST, &path(1, "/data-bounds"), Some(&key())).await;
        post.assert_problem(405, "method_not_allowed");
        assert_eq!(post.header("allow"), Some("GET, HEAD"));
        // HEAD runs the GET handler without a body.
        let head = send(&api, Method::HEAD, &path(1, "/data-bounds"), Some(&key())).await;
        assert_eq!(head.status, 200);
    });
}

#[test]
fn bodies_above_the_limit_are_refused() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let request = Request::get(path(1, "/data-bounds"))
            .header("authorization", format!("Bearer {}", key()))
            .header(CONTENT_LENGTH, crucible_api::BODY_LIMIT + 1)
            .body(Body::from(vec![b'x'; crucible_api::BODY_LIMIT + 1]))
            .unwrap();
        let response = api.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 413);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "payload_too_large");
    });
}

#[test]
fn without_a_store_requests_are_checked_and_then_unavailable() {
    run(|env| async move {
        let api = api_without_store(&env);
        let query = window(days(-1), NOW);
        let uri = traces(&query);
        send(&api, Method::GET, &uri, None)
            .await
            .assert_problem(401, "unauthenticated");
        get(&api, &format!("{}?{query}", path(2, "/traces")))
            .await
            .assert_problem(404, "not_found");
        get(&api, &path(1, "/traces"))
            .await
            .assert_problem(422, "validation_failed");
        for uri in [
            uri,
            path(1, &format!("/traces/{}", trace(1))),
            path(
                1,
                &format!("/traces/{}/spans?parent_span_id=000000000000000a", trace(1)),
            ),
            path(1, "/data-bounds"),
        ] {
            let reply = get(&api, &uri).await;
            reply.assert_problem(503, "telemetry_store_unavailable");
            assert_eq!(reply.body["detail"], "no telemetry store is configured");
        }
    });
}

#[test]
fn every_response_has_a_request_id_and_the_security_headers() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let mut ids = Vec::new();
        for (key, uri) in [
            (Some(key()), path(1, "/data-bounds")),
            (None, path(1, "/data-bounds")),
            (Some(key()), "/api/v1/nope".to_owned()),
        ] {
            let reply = send(&api, Method::GET, &uri, key.as_deref()).await;
            let id = reply.header("x-request-id").unwrap().to_owned();
            let uuid = RequestId::parse(&id).unwrap().uuid();
            assert!(uuid.v7_parts().is_some(), "{id} is not a UUIDv7");
            ids.push(id);
            for (name, value) in [
                ("x-content-type-options", "nosniff"),
                ("referrer-policy", "same-origin"),
                ("cross-origin-opener-policy", "same-origin"),
                ("cross-origin-resource-policy", "same-origin"),
                (
                    "content-security-policy",
                    "default-src 'none'; frame-ancestors 'none'",
                ),
            ] {
                assert_eq!(reply.header(name), Some(value), "{name} on {uri}");
            }
        }
        ids.dedup();
        assert_eq!(ids.len(), 3, "each request gets its own id");
        // An inbound X-Request-Id is not trusted.
        let request = Request::get(path(1, "/data-bounds"))
            .header("x-request-id", "client-chosen")
            .body(Body::empty())
            .unwrap();
        let response = api.clone().oneshot(request).await.unwrap();
        assert_ne!(response.headers()["x-request-id"], "client-chosen");
    });
}
