//! The SQL workspace's routes (contracts 11.6 and 12.6). Without a workspace the schema still answers and says
//! why runs are unavailable, and every other route answers `sql_workspace_unavailable` with its reason, after
//! the permission check. Over the scripted store and `PgModel`: a viewer runs project-visible saved queries and
//! nothing else, saved-query writes need a member's session, and an API key runs SQL only with the `sql`
//! scope. The service itself is exercised by `crucible-sqlws`'s tests, and the real binary by `sqliso` layer C
//! (`crates/crucible/tests/sqliso_api.rs`).

use axum::http::{Method, StatusCode};
use crucible_auth::roles::ProjectPermission;
use crucible_control::KeyScope;
use serde_json::json;

use crate::identity::{As, PASSWORD, call, key_refusal, platform_with, session};
use crate::world::World;
use crate::{api, get, key, other_key, path, run, send};

#[test]
fn the_schema_answers_without_a_workspace_and_says_why_runs_are_unavailable() {
    run(|env| async move {
        let api = api(&env, &World::new(10));
        let reply = get(&api, &path(1, "/sql/schema")).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.pretty());
        assert_eq!(reply.body["status"]["enabled"], false);
        assert_eq!(reply.body["status"]["reason"], "not_configured");
        let relations = reply.body["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 15);
        let spans = relations
            .iter()
            .find(|relation| relation["name"] == "crucible_sql.spans")
            .unwrap();
        assert_eq!(spans["time_column"], "start_time");
        assert_eq!(spans["window_required"], true);
        assert!(
            spans["example"]
                .as_str()
                .unwrap()
                .contains("{from:DateTime64(6)}")
        );
        assert!(
            spans["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|column| column["name"] == "trace_id" && column["type"] == "String")
        );
        assert_eq!(reply.body["limits"]["max_rows"], 10_000);
        assert_eq!(reply.body["limits"]["max_cell_bytes"], 65_536);
        // Another project's key, or none, learns nothing.
        let other = send(
            &api,
            Method::GET,
            &path(1, "/sql/schema"),
            Some(&other_key()),
        )
        .await;
        other.assert_problem(404, "not_found");
        let none = send(&api, Method::GET, &path(1, "/sql/schema"), None).await;
        none.assert_problem(401, "unauthenticated");
    });
}

#[test]
fn without_a_workspace_the_permission_is_checked_first_and_then_the_reason_is_given() {
    run(|env| async move {
        let api = api(&env, &World::new(10));
        let id = "01010101-0101-7101-8101-010101010101";
        // A key from the keys file has the scopes of a new key: ingest, read and write.
        for rest in [
            "/sql/runs".to_owned(),
            "/sql/saved-queries".to_owned(),
            format!("/sql/saved-queries/{id}"),
        ] {
            let reply = send(&api, Method::GET, &path(1, &rest), Some(&key())).await;
            reply.assert_problem(503, "sql_workspace_unavailable");
            assert_eq!(reply.body["reason"], "not_configured", "{rest}");
            assert!(
                reply.body["detail"]
                    .as_str()
                    .unwrap()
                    .contains("crucible migrate"),
                "the refusal says what an operator changes: {}",
                reply.pretty()
            );
        }
        // A key from the keys file is told the scope it lacks, and which setting gives keys other scopes.
        let (run_sql, saved, edit) = (
            ProjectPermission::RunSql,
            ProjectPermission::RunSavedQuery,
            ProjectPermission::EditContent,
        );
        for (method, rest, permission) in [
            (Method::POST, "/sql/run".to_owned(), run_sql),
            (Method::POST, "/sql/export".to_owned(), run_sql),
            (Method::DELETE, format!("/sql/runs/{id}"), saved),
            (Method::POST, format!("/sql/saved-queries/{id}/run"), saved),
            (Method::POST, "/sql/saved-queries".to_owned(), edit),
            (Method::DELETE, format!("/sql/saved-queries/{id}"), edit),
        ] {
            let reply = send(&api, method.clone(), &path(1, &rest), Some(&key())).await;
            reply.assert_problem(403, "forbidden");
            assert_eq!(
                reply.body["detail"],
                key_refusal(permission, false),
                "{method} {rest}: {}",
                reply.pretty()
            );
        }
        // Another project's key learns nothing, whatever the route needs.
        let reply = send(&api, Method::POST, &path(1, "/sql/run"), Some(&other_key())).await;
        reply.assert_problem(404, "not_found");
    });
}

#[test]
fn a_wrong_method_is_refused_with_the_methods_of_the_route() {
    run(|env| async move {
        let api = api(&env, &World::new(10));
        for (method, rest, allow) in [
            (Method::GET, "/sql/run", "POST"),
            (Method::POST, "/sql/schema", "GET"),
            (Method::PUT, "/sql/saved-queries", "POST"),
            (Method::POST, "/sql/runs/x", "DELETE"),
        ] {
            let reply = send(&api, method.clone(), &path(1, rest), Some(&key())).await;
            reply.assert_problem(405, "method_not_allowed");
            assert!(
                reply.header("allow").is_some_and(|v| v.contains(allow)),
                "{method} {rest}: {:?}",
                reply.header("allow")
            );
        }
    });
}

#[test]
fn roles_and_key_scopes_decide_who_runs_and_who_saves() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, true).await;
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let sql_key = p
            .identity
            .create_key(
                crate::world::project(1),
                Some(vec![KeyScope::Read, KeyScope::Sql]),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let sql_key = As::Key(sql_key.secret);
        let window = json!({ "from": "2026-11-17T00:00:00Z", "to": "2026-11-18T00:00:00Z" });
        let text = "SELECT name, n FROM crucible_sql.spans WHERE start_time >= {from:DateTime64(6)} AND \
                    start_time < {to:DateTime64(6)}";
        let run_body = json!({ "sql": text, "from": window["from"], "to": window["to"] });
        let export_body =
            json!({ "sql": text, "from": window["from"], "to": window["to"], "format": "csv" });
        let saved_body = json!({ "name": "Spans", "visibility": "project", "sql": text });
        let post = async |who: &As, rest: &str, body: &serde_json::Value| {
            call(
                &p.router,
                Method::POST,
                &path(1, rest),
                who,
                Some(body.clone()),
                &[],
            )
            .await
        };

        // The session says the workspace is on; without one it says why not.
        let me = call(
            &p.router,
            Method::GET,
            "/api/v1/auth/session",
            &p.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(
            me.body["features"]["sql_workspace"],
            json!({ "enabled": true }),
            "{}",
            me.pretty()
        );
        let bare = platform_with(&env, &world, false).await;
        let me = call(
            &bare.router,
            Method::GET,
            "/api/v1/auth/session",
            &bare.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(
            me.body["features"]["sql_workspace"],
            json!({ "enabled": false, "reason": "not_configured" })
        );

        // The owner is a project admin: runs, and saves a project-visible query.
        let reply = post(&p.owner, "/sql/run", &run_body).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.pretty());
        let saved = post(&p.owner, "/sql/saved-queries", &saved_body).await;
        assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.pretty());
        assert_eq!(saved.body["owner"]["kind"], "user");
        let saved_run = format!(
            "/sql/saved-queries/{}/run",
            saved.body["id"].as_str().unwrap()
        );
        let saved_one = format!("/sql/saved-queries/{}", saved.body["id"].as_str().unwrap());

        // A viewer reads, and runs the project-visible saved query with its declared parameters; nothing else.
        for (rest, body, permission) in [
            ("/sql/run", &run_body, "run_sql"),
            ("/sql/export", &export_body, "run_sql"),
            ("/sql/saved-queries", &saved_body, "edit_content"),
        ] {
            let reply = post(&viewer, rest, body).await;
            reply.assert_problem(403, "forbidden");
            assert!(
                reply.body["detail"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("`{permission}`")),
                "{rest}: {}",
                reply.pretty()
            );
        }
        let reply = post(&viewer, &saved_run, &window).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.pretty());
        let listed = call(
            &p.router,
            Method::GET,
            &path(1, "/sql/saved-queries"),
            &viewer,
            None,
            &[],
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.pretty());
        let seen = &listed.body["data"][0];
        assert_eq!(
            (&seen["can_run"], &seen["can_edit"]),
            (&json!(true), &json!(false))
        );
        let reply = call(
            &p.router,
            Method::DELETE,
            &path(1, &saved_one),
            &viewer,
            None,
            &[],
        )
        .await;
        reply.assert_problem(403, "forbidden");

        // A key without the `sql` scope reads and nothing else, and is told the scope it lacks; with it, it runs
        // ad hoc and saved queries. No key saves: `edit_content` is never a key's.
        for (rest, body) in [("/sql/run", &run_body), (saved_run.as_str(), &window)] {
            let reply = post(&p.key, rest, body).await;
            reply.assert_problem(403, "forbidden");
            assert_eq!(
                reply.body["detail"],
                key_refusal(ProjectPermission::RunSql, true),
                "{rest}"
            );
        }
        let reply = post(&sql_key, "/sql/run", &run_body).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.pretty());
        let reply = post(&sql_key, "/sql/export", &export_body).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.pretty());
        let reply = post(&sql_key, &saved_run, &window).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.pretty());
        let reply = post(&sql_key, "/sql/saved-queries", &saved_body).await;
        reply.assert_problem(403, "forbidden");
        assert_eq!(
            reply.body["detail"],
            key_refusal(ProjectPermission::EditContent, true)
        );

        // The history names who ran what. An admin sees everyone's runs, the user's and the key's; a key sees
        // its own.
        let history = call(
            &p.router,
            Method::GET,
            &path(1, "/sql/runs"),
            &p.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(history.status, StatusCode::OK, "{}", history.pretty());
        let kinds: Vec<&str> = history.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["actor"]["kind"].as_str().unwrap())
            .collect();
        assert!(
            kinds.contains(&"user") && kinds.contains(&"api_key"),
            "{kinds:?}"
        );
        let own = call(
            &p.router,
            Method::GET,
            &path(1, "/sql/runs"),
            &sql_key,
            None,
            &[],
        )
        .await;
        let own = own.body["data"].as_array().unwrap();
        assert!(!own.is_empty() && own.iter().all(|run| run["actor"]["kind"] == "api_key"));
    });
}
