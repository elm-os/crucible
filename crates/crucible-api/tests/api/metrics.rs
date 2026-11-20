//! The metrics routes (contract 8.8 and 12.6): the schema, a query with its `other` series, `query_too_broad`
//! with `suggested_granularity`, the problems of a query, and the filter-values lookup.

use axum::http::StatusCode;
use crucible_plan::Value;
use crucible_records::{Level, TableId, Timestamp};
use serde_json::json;

use crate::store::stored_row;
use crate::world::{NOW, World, project};
use crate::{api, get, path, post, run};

const MINUTE: i64 = 60_000_000;

fn ago(minutes: i64) -> Timestamp {
    NOW.saturating_add_micros(-minutes * MINUTE)
}

/// Span rollup partials of three environments in the hour before [`NOW`].
fn rollup() -> World {
    let world = World::new(10);
    for (minutes, environment, level, count) in [
        (50, "prod", Level::Default, 5u64),
        (50, "dev", Level::Error, 2),
        (10, "prod", Level::Default, 3),
        (10, "staging", Level::Default, 1),
    ] {
        let row = stored_row(
            TableId::SpanMetrics,
            &[
                ("project_id", Value::Uuid(project(1).uuid())),
                ("minute", Value::Time(ago(minutes))),
                ("environment", Value::String(environment.to_owned())),
                ("level", Value::Enum(level.value())),
                ("count", Value::UInt(count)),
                (
                    "error_count",
                    Value::UInt(u64::from(level == Level::Error) * count),
                ),
                ("total_cost_nusd", Value::Int(count as i64 * 1_500_000_000)),
            ],
        );
        world.store.insert(TableId::SpanMetrics, row);
    }
    world
}

fn query(extra: serde_json::Value) -> String {
    let mut body = json!({
        "source": "spans",
        "measures": [{"measure": "count", "aggregation": "sum"}, {"measure": "total_cost", "aggregation": "sum"}],
        "from": ago(60).to_string(),
        "to": NOW.to_string(),
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body.to_string()
}

#[test]
fn the_schema_lists_what_a_query_can_ask_for() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let schema = get(&api, &path(1, "/metrics/schema")).await;
        assert_eq!(schema.status, StatusCode::OK, "{:#}", schema.body);
        insta::assert_snapshot!("metrics_schema", schema.pretty());
    });
}

#[test]
fn a_query_returns_its_series_and_the_other_series() {
    run(|env| async move {
        let world = rollup();
        let api = api(&env, &world);
        let extra =
            json!({"dimensions": ["environment"], "top_n": 1, "granularity": "fifteen_minutes"});
        let reply = post(&api, &path(1, "/metrics/query"), &query(extra), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        insta::assert_snapshot!("metrics_query", reply.pretty());
        assert_eq!(
            reply.body["series"][0]["key"],
            json!([{"dimension": "environment", "value": "prod"}])
        );
        assert_eq!(reply.body["series"][1]["unit"], "usd");
        assert_eq!(reply.body["series"][2]["other"], true);
        assert_eq!(reply.body["truncated"], false);

        // A KPI: totals without dimensions, granularity picked automatically.
        let reply = post(&api, &path(1, "/metrics/query"), &query(json!({})), &[]).await;
        assert_eq!(reply.body["granularity"], "minute");
        assert_eq!(reply.body["source_tier"], "rollup");
        let total: f64 = reply.body["series"][0]["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["value"].as_f64().unwrap())
            .sum();
        assert_eq!(total, 11.0);
    });
}

#[test]
fn queries_outside_the_registry_and_the_caps_are_refused() {
    run(|env| async move {
        let world = rollup();
        let api = api(&env, &world);
        let month = json!({"from": NOW.saturating_add_micros(-30 * 24 * 60 * MINUTE).to_string(), "granularity": "minute"});
        let reply = post(&api, &path(1, "/metrics/query"), &query(month), &[]).await;
        reply.assert_problem(422, "query_too_broad");
        assert_eq!(reply.body["suggested_granularity"], "day");
        assert_eq!(
            reply.body["limit"],
            json!({"name": "max_buckets", "value": 500})
        );
        let cases = [
            (
                json!({"measures": [{"measure": "latency", "aggregation": "sum"}]}),
                "/measures/0/measure",
            ),
            (
                json!({"measures": [{"measure": "duration", "aggregation": "sum"}]}),
                "/measures",
            ),
            (json!({"dimensions": ["user"]}), "/dimensions/0"),
            (json!({"source": "logs"}), "/source"),
            (json!({"timezone": "Europe/Paris"}), "/timezone"),
            (
                json!({"filter": {"in": {"field": "user_id", "values": ["u"]}}}),
                "/filter",
            ),
            (json!({"to": "yesterday"}), "/to"),
        ];
        for (extra, pointer) in cases {
            let reply = post(&api, &path(1, "/metrics/query"), &query(extra.clone()), &[]).await;
            reply.assert_problem(422, "validation_failed");
            assert_eq!(reply.field_errors()[0].0, pointer, "{extra}");
        }
        // A filter on a field of the source.
        let filtered =
            json!({"filter": {"not": {"in": {"field": "environment", "values": ["dev"]}}}});
        let reply = post(&api, &path(1, "/metrics/query"), &query(filtered), &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
    });
}

#[test]
fn filter_values_are_the_most_frequent_first() {
    run(|env| async move {
        let world = rollup();
        let api = api(&env, &world);
        let uri = |extra: &str| {
            format!(
                "{}?source=spans&from={}&to={}{extra}",
                path(1, "/filter-values"),
                ago(60),
                NOW
            )
        };
        let reply = get(&api, &uri("&field=environment&limit=2")).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        insta::assert_snapshot!("filter_values", reply.pretty());
        let levels = get(&api, &uri("&field=level&prefix=err")).await;
        assert_eq!(levels.body["data"], json!([{"value": "error", "count": 2}]));
        let unknown = get(&api, &uri("&field=user_id")).await;
        unknown.assert_problem(422, "validation_failed");
        assert_eq!(unknown.field_errors(), [("/field", "unknown_field")]);
    });
}
