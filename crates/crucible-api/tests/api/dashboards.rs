//! The dashboard routes over `PgModel` (contracts 11.1, 11.6 and 12.6): dashboards and their widgets under one
//! version, layouts checked whole, widget queries checked against the metrics schema, read-only built-in
//! dashboards that duplicate into rows, and the built-in dashboards without a control plane.

use axum::http::Method;
use crucible_plan::{Source, SourceSchema};
use serde_json::{Value, json};

use crate::identity::{As, PASSWORD, call, platform_with, session};
use crate::world::{NOW, World, project};
use crate::{Reply, api, post, run, send};

fn path(rest: &str) -> String {
    format!("/api/v1/projects/{}/dashboards{rest}", project(1))
}

fn etag(reply: &Reply) -> String {
    reply.header("etag").expect("an ETag").to_owned()
}

fn stat(title: &str, x: u32, y: u32) -> Value {
    json!({
        "query": {"source": "spans", "measures": [{"measure": "count", "aggregation": "sum"}]},
        "display": {"kind": "stat", "title": title},
        "layout": {"x": x, "y": y, "w": 3, "h": 2},
    })
}

fn chart(x: u32, y: u32) -> Value {
    json!({
        "query": {
            "source": "spans",
            "measures": [{"measure": "duration", "aggregation": "p50"}, {"measure": "duration", "aggregation": "p95"}],
        },
        "display": {"kind": "timeseries", "title": "Duration", "chart": "line"},
        "layout": {"x": x, "y": y, "w": 6, "h": 3},
    })
}

struct Client<'a> {
    router: &'a axum::Router,
}

impl Client<'_> {
    async fn send(
        &self,
        method: Method,
        rest: &str,
        who: &As,
        body: Option<Value>,
        version: Option<&str>,
    ) -> Reply {
        let extra: Vec<(&str, &str)> = version.iter().map(|v| ("if-match", *v)).collect();
        call(self.router, method, &path(rest), who, body, &extra).await
    }
}

#[test]
fn dashboards_are_created_changed_and_deleted_under_their_version() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let c = Client { router: &p.router };

        let body = json!({"name": "Agents", "widgets": [chart(0, 2), stat("Spans", 0, 0)]});
        let created = c.send(Method::POST, "", &p.owner, Some(body), None).await;
        assert_eq!(created.status.as_u16(), 201, "{}", created.pretty());
        assert_eq!(etag(&created), "\"1\"");
        let dashboard = created.body.clone();
        let id = dashboard["id"].as_str().unwrap().to_owned();
        assert_eq!(dashboard["version"], 1);
        assert!(dashboard.get("builtin_key").is_none());
        // Reading order: the stat on row 0 comes before the chart on row 2.
        let widgets = dashboard["widgets"].as_array().unwrap();
        assert_eq!(widgets[0]["display"]["title"], "Spans");
        assert_eq!(widgets[1]["display"]["title"], "Duration");
        let (spans, duration) = (widgets[0]["id"].clone(), widgets[1]["id"].clone());

        let listed = c.send(Method::GET, "", &p.owner, None, None).await;
        assert_eq!(listed.status.as_u16(), 200, "{}", listed.pretty());
        let data = listed.body["data"].as_array().unwrap();
        assert_eq!(data[0]["id"], id.as_str());
        assert_eq!(data[0]["widget_count"], 2);
        let builtin: Vec<&str> = data[1..]
            .iter()
            .map(|d| d["builtin_key"].as_str().unwrap())
            .collect();
        let mcp = SourceSchema::of(Source::McpOps).is_some();
        let mut expected = vec!["overview", "cost-and-tokens", "latency", "errors"];
        if mcp {
            expected.push("mcp");
        }
        assert_eq!(builtin, expected);

        let read = c
            .send(Method::GET, &format!("/{id}"), &p.key, None, None)
            .await;
        assert_eq!(read.status.as_u16(), 200, "{}", read.pretty());
        assert_eq!(etag(&read), "\"1\"");
        assert_eq!(read.body, dashboard);

        // Changes need the version that was read.
        let rename = json!({"name": "Agent health"});
        let bare = c
            .send(
                Method::PATCH,
                &format!("/{id}"),
                &p.owner,
                Some(rename.clone()),
                None,
            )
            .await;
        bare.assert_problem(428, "precondition_required");
        let stale = c
            .send(
                Method::PATCH,
                &format!("/{id}"),
                &p.owner,
                Some(rename),
                Some("\"7\""),
            )
            .await;
        stale.assert_problem(412, "precondition_failed");

        // One change trades the widgets' places, which one widget at a time could not do.
        let mut top = chart(0, 0);
        top["id"] = duration.clone();
        let mut below = stat("Spans", 0, 3);
        below["id"] = spans.clone();
        let traded = c
            .send(
                Method::PATCH,
                &format!("/{id}"),
                &p.owner,
                Some(json!({"name": "Agent health", "widgets": [top, below]})),
                Some("\"1\""),
            )
            .await;
        assert_eq!(traded.status.as_u16(), 200, "{}", traded.pretty());
        assert_eq!(etag(&traded), "\"2\"");
        assert_eq!(traded.body["name"], "Agent health");
        let widgets = traded.body["widgets"].as_array().unwrap();
        assert_eq!(widgets[0]["id"], duration);
        assert_eq!(widgets[1]["id"], spans);

        // A widget id must be one of this dashboard's, and widgets must not overlap.
        let mut foreign = stat("Elsewhere", 0, 0);
        foreign["id"] = json!("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b");
        let refused = c
            .send(
                Method::PATCH,
                &format!("/{id}"),
                &p.owner,
                Some(json!({"widgets": [foreign]})),
                Some("\"2\""),
            )
            .await;
        refused.assert_problem(422, "validation_failed");
        assert_eq!(refused.field_errors(), [("/widgets/0/id", "unknown")]);
        let overlapping = c
            .send(
                Method::PATCH,
                &format!("/{id}"),
                &p.owner,
                Some(json!({"widgets": [stat("A", 0, 0), stat("B", 2, 1)]})),
                Some("\"2\""),
            )
            .await;
        overlapping.assert_problem(422, "validation_failed");
        assert_eq!(
            overlapping.field_errors(),
            [("/widgets/1/layout", "overlaps")]
        );

        // Viewers read and do not change.
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let seen = c
            .send(Method::GET, &format!("/{id}"), &viewer, None, None)
            .await;
        assert_eq!(seen.status.as_u16(), 200);
        let denied = c
            .send(
                Method::DELETE,
                &format!("/{id}"),
                &viewer,
                None,
                Some("\"2\""),
            )
            .await;
        denied.assert_problem(403, "forbidden");

        let deleted = c
            .send(
                Method::DELETE,
                &format!("/{id}"),
                &p.owner,
                None,
                Some("\"2\""),
            )
            .await;
        assert_eq!(deleted.status.as_u16(), 204);
        let gone = c
            .send(Method::GET, &format!("/{id}"), &p.owner, None, None)
            .await;
        gone.assert_problem(404, "not_found");
        let again = c
            .send(
                Method::DELETE,
                &format!("/{id}"),
                &p.owner,
                None,
                Some("\"2\""),
            )
            .await;
        again.assert_problem(404, "not_found");
    });
}

#[test]
fn widgets_are_added_changed_and_removed_under_the_dashboards_version() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let c = Client { router: &p.router };
        let created = c
            .send(
                Method::POST,
                "",
                &p.owner,
                Some(json!({"name": "Empty"})),
                None,
            )
            .await;
        assert_eq!(created.status.as_u16(), 201, "{}", created.pretty());
        let id = created.body["id"].as_str().unwrap().to_owned();
        let widgets = format!("/{id}/widgets");

        // Adding takes no If-Match and moves the dashboard's version on.
        let added = c
            .send(
                Method::POST,
                &widgets,
                &p.owner,
                Some(stat("Spans", 0, 0)),
                None,
            )
            .await;
        assert_eq!(added.status.as_u16(), 201, "{}", added.pretty());
        assert_eq!(etag(&added), "\"2\"");
        let widget_id = added.body["id"].as_str().unwrap().to_owned();
        let on_top = c
            .send(
                Method::POST,
                &widgets,
                &p.owner,
                Some(stat("Over", 1, 1)),
                None,
            )
            .await;
        on_top.assert_problem(422, "validation_failed");
        assert_eq!(on_top.field_errors(), [("/layout", "overlaps")]);
        let with_id = {
            let mut body = stat("Id", 6, 0);
            body["id"] = json!(widget_id);
            c.send(Method::POST, &widgets, &p.owner, Some(body), None)
                .await
        };
        assert_eq!(with_id.field_errors(), [("/id", "invalid")]);

        // A change to a widget takes the dashboard's version; a markdown display drops the query.
        let one = format!("{widgets}/{widget_id}");
        let markdown =
            json!({"display": {"kind": "markdown", "title": "Notes", "text": "Read *me*"}});
        let stale = c
            .send(
                Method::PATCH,
                &one,
                &p.owner,
                Some(markdown.clone()),
                Some("\"1\""),
            )
            .await;
        stale.assert_problem(412, "precondition_failed");
        let changed = c
            .send(Method::PATCH, &one, &p.owner, Some(markdown), Some("\"2\""))
            .await;
        assert_eq!(changed.status.as_u16(), 200, "{}", changed.pretty());
        assert_eq!(etag(&changed), "\"3\"");
        assert!(changed.body.get("query").is_none(), "{}", changed.pretty());
        assert_eq!(changed.body["display"]["text"], "Read *me*");
        let unknown = c
            .send(
                Method::PATCH,
                &format!("{widgets}/0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"),
                &p.owner,
                Some(json!({})),
                Some("\"3\""),
            )
            .await;
        unknown.assert_problem(404, "not_found");

        let removed = c
            .send(Method::DELETE, &one, &p.owner, None, Some("\"3\""))
            .await;
        assert_eq!(removed.status.as_u16(), 204);
        assert_eq!(etag(&removed), "\"4\"");
        let read = c
            .send(Method::GET, &format!("/{id}"), &p.owner, None, None)
            .await;
        assert_eq!(read.body["widgets"], json!([]));
        assert_eq!(read.body["version"], 4);

        // Fifty widgets is the most.
        let full: Vec<Value> = (0..50).map(|i| stat(&format!("W{i}"), 0, i * 2)).collect();
        let replaced = c
            .send(
                Method::PATCH,
                &format!("/{id}"),
                &p.owner,
                Some(json!({"widgets": full})),
                Some("\"4\""),
            )
            .await;
        assert_eq!(replaced.status.as_u16(), 200, "{}", replaced.pretty());
        let more = c
            .send(
                Method::POST,
                &widgets,
                &p.owner,
                Some(stat("More", 6, 0)),
                None,
            )
            .await;
        more.assert_problem(422, "validation_failed");
        assert_eq!(more.field_errors(), [("", "too_many")]);
    });
}

#[test]
fn a_widget_query_is_checked_against_the_metrics_schema_and_its_kind() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let c = Client { router: &p.router };
        let mut unknown = stat("Unknown", 0, 0);
        unknown["query"]["measures"][0]["measure"] = json!("nope");
        let mut split = stat("Split", 3, 0);
        split["query"]["dimensions"] = json!(["model"]);
        let mut crowded = chart(6, 0);
        crowded["query"] = json!({
            "source": "spans",
            "measures": [{"measure": "count", "aggregation": "sum"}],
            "dimensions": ["model"],
        });
        let body = json!({"name": "Checked", "widgets": [unknown, split, crowded]});
        let refused = c.send(Method::POST, "", &p.owner, Some(body), None).await;
        refused.assert_problem(422, "validation_failed");
        assert_eq!(
            refused.field_errors(),
            [
                ("/widgets/0/query/measures/0/measure", "invalid"),
                ("/widgets/1/query/dimensions", "invalid"),
                ("/widgets/2/query/top_n", "invalid"),
            ]
        );
        let untitled = json!({"name": " "});
        let refused = c
            .send(Method::POST, "", &p.owner, Some(untitled), None)
            .await;
        assert_eq!(refused.field_errors(), [("/name", "invalid")]);
    });
}

/// A time series draws as lines, bars or areas; no other kind has a chart form.
#[test]
fn a_time_series_draws_as_areas() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let c = Client { router: &p.router };
        let mut areas = chart(0, 0);
        areas["display"]["chart"] = json!("area");
        let body = json!({"name": "Areas", "widgets": [areas]});
        let created = c.send(Method::POST, "", &p.owner, Some(body), None).await;
        assert_eq!(created.status.as_u16(), 201, "{}", created.pretty());
        assert_eq!(created.body["widgets"][0]["display"]["chart"], "area");
        let mut stat = stat("Spans", 0, 0);
        stat["display"]["chart"] = json!("area");
        let body = json!({"name": "Stat", "widgets": [stat]});
        let refused = c.send(Method::POST, "", &p.owner, Some(body), None).await;
        assert_eq!(
            refused.field_errors(),
            [("/widgets/0/display/chart", "invalid")]
        );
    });
}

#[test]
fn built_in_dashboards_are_read_only_and_duplicate_into_rows() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let c = Client { router: &p.router };

        let overview = c.send(Method::GET, "/overview", &p.key, None, None).await;
        assert_eq!(overview.status.as_u16(), 200, "{}", overview.pretty());
        assert_eq!(overview.body["builtin_key"], "overview");
        assert_eq!(overview.body["created_at"], Value::Null);
        let count = overview.body["widgets"].as_array().unwrap().len();
        let mcp = c.send(Method::GET, "/mcp", &p.owner, None, None).await;
        assert_eq!(
            mcp.status.as_u16() == 200,
            SourceSchema::of(Source::McpOps).is_some()
        );
        let nope = c
            .send(Method::GET, "/no-such-starter", &p.owner, None, None)
            .await;
        nope.assert_problem(404, "not_found");

        for (method, rest, body, version) in [
            (
                Method::PATCH,
                "/overview",
                Some(json!({"name": "X"})),
                Some("\"1\""),
            ),
            (Method::DELETE, "/overview", None, Some("\"1\"")),
            (
                Method::POST,
                "/overview/widgets",
                Some(stat("X", 0, 20)),
                None,
            ),
        ] {
            let refused = c.send(method, rest, &p.owner, body, version).await;
            refused.assert_problem(409, "conflict");
        }

        let copy = c
            .send(
                Method::POST,
                "/overview/duplicate",
                &p.owner,
                Some(json!({})),
                None,
            )
            .await;
        assert_eq!(copy.status.as_u16(), 201, "{}", copy.pretty());
        assert_eq!(copy.body["name"], "Agent overview (copy)");
        assert!(copy.body.get("builtin_key").is_none());
        let widgets = copy.body["widgets"].as_array().unwrap();
        assert_eq!(widgets.len(), count);
        assert!(
            widgets
                .iter()
                .all(|w| w["id"].as_str().unwrap().len() == 36)
        );

        // A copy of a copy, with a name of its own.
        let id = copy.body["id"].as_str().unwrap();
        let named = c
            .send(
                Method::POST,
                &format!("/{id}/duplicate"),
                &p.owner,
                Some(json!({"name": "Mine"})),
                None,
            )
            .await;
        assert_eq!(named.status.as_u16(), 201, "{}", named.pretty());
        assert_eq!(named.body["name"], "Mine");
        assert_ne!(named.body["widgets"][0]["id"], widgets[0]["id"]);
        // Keys read dashboards but never change one.
        let by_key = c
            .send(
                Method::POST,
                "/overview/duplicate",
                &p.key,
                Some(json!({})),
                None,
            )
            .await;
        by_key.assert_problem(403, "forbidden");
    });
}

#[test]
fn without_a_control_plane_the_built_in_dashboards_are_served() {
    run(|env| async move {
        let world = World::new(10);
        let router = api(&env, &world);
        let base = format!("/api/v1/projects/{}/dashboards", project(1));
        let listed = send(&router, Method::GET, &base, Some(&crate::key())).await;
        assert_eq!(listed.status.as_u16(), 200, "{}", listed.pretty());
        assert!(
            listed.body["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["builtin_key"].is_string())
        );
        let latency = send(
            &router,
            Method::GET,
            &format!("{base}/latency"),
            Some(&crate::key()),
        )
        .await;
        assert_eq!(latency.status.as_u16(), 200, "{}", latency.pretty());
        let stored = format!("{base}/0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b");
        let missing = send(&router, Method::GET, &stored, Some(&crate::key())).await;
        missing.assert_problem(404, "not_found");
    });
}

// Every query a listed starter holds runs as its widget will: through `POST metrics/query` over a window. A
// source that gains required members (as `mcp_ops` does with its side) fails here until the starter sets them.
#[test]
fn every_listed_starter_widget_runs_through_the_metrics_query() {
    run(|env| async move {
        let world = World::new(10);
        let router = api(&env, &world);
        let base = format!("/api/v1/projects/{}", project(1));
        let key = crate::key();
        let listed = send(
            &router,
            Method::GET,
            &format!("{base}/dashboards"),
            Some(&key),
        )
        .await;
        assert_eq!(listed.status.as_u16(), 200, "{}", listed.pretty());
        let starters: Vec<String> = listed.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["builtin_key"].as_str().map(str::to_owned))
            .collect();
        assert!(starters.len() >= 4, "{starters:?}");
        let (from, to) = (NOW.saturating_add_micros(-86_400_000_000), NOW);
        for starter in starters {
            let one = send(
                &router,
                Method::GET,
                &format!("{base}/dashboards/{starter}"),
                Some(&key),
            )
            .await;
            assert_eq!(one.status.as_u16(), 200, "{}", one.pretty());
            for widget in one.body["widgets"].as_array().unwrap() {
                let Some(query) = widget.get("query") else {
                    continue;
                };
                let mut body = query.clone();
                body["from"] = json!(from.to_string());
                body["to"] = json!(to.to_string());
                let reply = post(
                    &router,
                    &format!("{base}/metrics/query"),
                    &body.to_string(),
                    &[],
                )
                .await;
                assert_eq!(
                    reply.status.as_u16(),
                    200,
                    "{starter}/{}: {}",
                    widget["id"],
                    reply.pretty()
                );
            }
        }
    });
}
