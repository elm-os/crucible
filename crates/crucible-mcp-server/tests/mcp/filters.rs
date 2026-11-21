//! One list filter, refused alike by every surface that takes it (contracts 8.8 and 12.5): the REST trace and span
//! lists as query parameters, REST search as its `filters`, and the MCP server's `list_traces` and `list_spans` as
//! their `filter`. Each refusal names the same member with the same code, under the pointer of the surface's own
//! frame: the query parameter, `/filters/<member>` in a search body, `/filter/<member>` in a tool's arguments.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use crucible_api::StaticProjectSettings;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::world::{NOW, World, project};
use crate::{Client, cursor_keys, key, keys, mcp, run};

const DAY: i64 = 86_400_000_000;

/// The member and the code of each field error of a refusal, the pointer's frame (`prefix`) taken off.
fn refusals(problem: &Value, prefix: &str) -> Vec<(String, String)> {
    assert_eq!(problem["code"], "validation_failed", "{problem:#}");
    problem["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("{problem:#}"))
        .iter()
        .map(|error| {
            let pointer = error["pointer"].as_str().unwrap();
            let member = pointer
                .strip_prefix(prefix)
                .unwrap_or_else(|| panic!("{pointer} is not under {prefix}"));
            (
                member.to_owned(),
                error["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn rest(router: &Router, request: Request<Body>) -> Value {
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// A REST list request with the filter as query parameters, each value of a list once.
async fn rest_list(router: &Router, list: &str, filter: &Value) -> Value {
    let (from, to) = (
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    );
    let mut query = format!("from={from}&to={to}");
    for (member, values) in filter.as_object().unwrap() {
        for value in values.as_array().unwrap() {
            query.push_str(&format!("&{member}={}", value.as_str().unwrap()));
        }
    }
    let uri = format!("/api/v1/projects/{}/{list}?{query}", project(1));
    let request = Request::get(uri)
        .header("authorization", format!("Bearer {}", key(1)))
        .body(Body::empty())
        .unwrap();
    rest(router, request).await
}

async fn rest_search(router: &Router, filter: &Value) -> Value {
    let body = json!({
        "query": "refund",
        "from": NOW.saturating_add_micros(-DAY).to_string(),
        "to": NOW.saturating_add_micros(DAY).to_string(),
        "filters": filter,
    });
    let request = Request::post(format!("/api/v1/projects/{}/search", project(1)))
        .header("authorization", format!("Bearer {}", key(1)))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    rest(router, request).await
}

fn arguments(filter: &Value) -> Value {
    json!({
        "from": NOW.saturating_add_micros(-DAY).to_string(),
        "to": NOW.saturating_add_micros(DAY).to_string(),
        "filter": filter,
    })
}

#[test]
fn one_invalid_filter_is_refused_alike_by_every_surface() {
    run(|env| async move {
        let world = World::new(10);
        let api = crucible_api::router_without_store(
            &env,
            keys(&env),
            Arc::new(StaticProjectSettings::default()),
            cursor_keys(),
        );
        let server = mcp(&env, &world);
        let mut client = Client::modern(&server, &key(1));
        let pair = |member: &str, code: &str| (member.to_owned(), code.to_owned());

        // Refused wherever the filter is taken: a metadata entry without `key=value`, and a list of too many values.
        let tags: Vec<String> = (0..101).map(|n| format!("t{n}")).collect();
        let everywhere = [
            (
                json!({ "metadata": ["team"] }),
                pair("metadata", "invalid_value"),
            ),
            (json!({ "tags": tags }), pair("tags", "too_many_values")),
        ];
        for (filter, expected) in everywhere {
            let answers = [
                refusals(&rest_list(&api, "traces", &filter).await, "/"),
                refusals(&rest_list(&api, "spans", &filter).await, "/"),
                refusals(&rest_search(&api, &filter).await, "/filters/"),
                refusals(
                    &client.error("list_traces", arguments(&filter)).await,
                    "/filter/",
                ),
                refusals(
                    &client.error("list_spans", arguments(&filter)).await,
                    "/filter/",
                ),
            ];
            for answer in answers {
                assert_eq!(answer, std::slice::from_ref(&expected), "{filter}");
            }
        }

        // Refused by the trace list alone, which keeps traces by one span table and tests at most 20 tags; the
        // span list, search and `list_spans` test each span and take them.
        let tags: Vec<String> = (0..21).map(|n| format!("t{n}")).collect();
        let traces_only = [
            (
                json!({ "metadata": ["plan=pro"], "observation_type": ["tool"] }),
                pair("metadata", "conflict"),
            ),
            (json!({ "tags": tags }), pair("tags", "too_many_values")),
        ];
        for (filter, expected) in traces_only {
            let answers = [
                refusals(&rest_list(&api, "traces", &filter).await, "/"),
                refusals(
                    &client.error("list_traces", arguments(&filter)).await,
                    "/filter/",
                ),
            ];
            for answer in answers {
                assert_eq!(answer, std::slice::from_ref(&expected), "{filter}");
            }
            // Past the filter, a server without a store answers that it has none.
            let spans = rest_list(&api, "spans", &filter).await;
            assert_eq!(spans["code"], "telemetry_store_unavailable", "{spans:#}");
            let found = rest_search(&api, &filter).await;
            assert_eq!(found["code"], "telemetry_store_unavailable", "{found:#}");
            let listed = client.output("list_spans", arguments(&filter)).await;
            assert!(listed["data"].is_array(), "{listed:#}");
        }
    });
}
