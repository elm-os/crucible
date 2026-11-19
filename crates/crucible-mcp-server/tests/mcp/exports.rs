//! The export tools (contracts 9.10 and 12.8): `create_export` answers with a queued export that `get_export`
//! reads back, both refuse what they cannot do with a stable code that names the argument, `create_export` is
//! the one tool annotated as not read-only, and without a control plane both name the setting they need.

use std::sync::Arc;

use crucible_control::{JsonDoc, Project, Repos as _};
use crucible_export::Exports;
use crucible_sim::pg_model::{PgFaults, PgModel};
use serde_json::{Value, json};

use crate::world::{World, project};
use crate::{Client, key, mcp, mcp_exporting, run};

fn arguments() -> Value {
    json!({
        "format": "otlp-json",
        "from": "2026-11-17T09:00:00Z",
        "to": "2026-11-17T10:00:00Z",
        "user_id": "user-42",
    })
}

#[test]
fn an_export_is_created_and_followed() {
    run(|env| async move {
        let world = World::new(10);
        let model = PgModel::new(
            env.clock.clone(),
            env.rng.fork("pg").as_ref(),
            PgFaults::default(),
        );
        model
            .projects()
            .insert(&Project {
                id: project(1),
                name: "One".to_owned(),
                slug: "one".to_owned(),
                settings: JsonDoc::empty(),
                retention_days: None,
                payload_retention_days: None,
                mcp_content_retention_days: None,
                config_version: 1,
                version: 1,
                deleted_at: None,
            })
            .await
            .unwrap();
        let exports = Exports::new(env.clone(), Arc::new(model));
        let router = mcp_exporting(&env, &world, exports);
        let mut session = Client::modern(&router, &key(1));

        let listed = session.request("tools/list", json!({})).await;
        let tools = listed.body["result"]["tools"].as_array().unwrap();
        let annotations = |name: &str| {
            tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"].clone()
        };
        assert_eq!(annotations("create_export"), json!(false));
        assert_eq!(annotations("get_export"), json!(true));

        let created = session.output("create_export", arguments()).await;
        assert_eq!(created["state"], "queued");
        assert_eq!(created["format"], "otlp-json");
        assert_eq!(created["from"], "2026-11-17T09:00:00.000000Z");
        assert_eq!(created["spans"], 0);
        let read = session
            .output("get_export", json!({"id": created["id"]}))
            .await;
        assert_eq!(read["id"], created["id"]);
        assert_eq!(read["state"], "queued");

        let missing = session
            .error(
                "get_export",
                json!({"id": "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"}),
            )
            .await;
        assert_eq!(missing["code"], "not_found");
        let invalid = session.error("get_export", json!({"id": "export-1"})).await;
        assert_eq!(invalid["code"], "validation_failed");
        assert_eq!(invalid["errors"][0]["pointer"], "/id");
        let window = session
            .error(
                "create_export",
                json!({"format": "crucible-jsonl", "from": "today", "to": "2026-11-17T10:00:00Z"}),
            )
            .await;
        assert_eq!(window["code"], "validation_failed");
        assert_eq!(window["errors"][0]["pointer"], "/from");
        let unknown = session
            .error(
                "create_export",
                json!({"format": "csv", "from": "a", "to": "b"}),
            )
            .await;
        assert_eq!(unknown["code"], "validation_failed");
    });
}

#[test]
fn without_a_control_plane_the_export_tools_name_the_setting() {
    run(|env| async move {
        let world = World::new(10);
        let router = mcp(&env, &world);
        let mut session = Client::modern(&router, &key(1));
        for (tool, args) in [
            ("create_export", arguments()),
            (
                "get_export",
                json!({"id": "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"}),
            ),
        ] {
            let problem = session.error(tool, args).await;
            assert_eq!(problem["code"], "feature_disabled", "{tool}");
            assert!(
                problem["detail"].as_str().unwrap().contains("postgres.url"),
                "{tool}"
            );
        }
    });
}
