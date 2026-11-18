//! The SQL workspace's routes without a workspace configured (contract 12.6): the schema still answers and says
//! why runs are unavailable, every other route answers `sql_workspace_unavailable` with its reason, and a wrong
//! method names the right ones. The routes over a real workspace are exercised by `crucible-sqlws`'s tests
//! (the service) and by `sqliso` layer C (`crates/crucible/tests/sqliso_api.rs`, the real binary).

use axum::http::{Method, StatusCode};

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
fn every_other_route_says_the_workspace_is_not_configured() {
    run(|env| async move {
        let api = api(&env, &World::new(10));
        let id = "01010101-0101-7101-8101-010101010101";
        for (method, rest) in [
            (Method::GET, "/sql/runs".to_owned()),
            (Method::GET, "/sql/saved-queries".to_owned()),
            (Method::GET, format!("/sql/saved-queries/{id}")),
            (Method::DELETE, format!("/sql/saved-queries/{id}")),
            (Method::DELETE, format!("/sql/runs/{id}")),
        ] {
            let reply = send(&api, method.clone(), &path(1, &rest), Some(&key())).await;
            reply.assert_problem(503, "sql_workspace_unavailable");
            assert_eq!(reply.body["reason"], "not_configured", "{method} {rest}");
            assert!(
                reply.body["detail"]
                    .as_str()
                    .unwrap()
                    .contains("crucible migrate"),
                "the refusal says what an operator changes: {}",
                reply.pretty()
            );
        }
        // A body-taking route checks its body first: an empty body is a validation problem, not a 503.
        let reply = send(&api, Method::POST, &path(1, "/sql/run"), Some(&key())).await;
        reply.assert_problem(422, "validation_failed");
        // A malformed id in the path is `not_found`, like every other path the caller cannot name.
        let reply = send(
            &api,
            Method::DELETE,
            &path(1, "/sql/runs/not-an-id"),
            Some(&key()),
        )
        .await;
        reply.assert_problem(404, "not_found");
    });
}

#[test]
fn a_wrong_method_names_the_methods_of_the_sql_route() {
    run(|env| async move {
        let api = api(&env, &World::new(10));
        for (method, rest, allow) in [
            (Method::GET, "/sql/run", "POST"),
            (Method::GET, "/sql/export", "POST"),
            (Method::POST, "/sql/schema", "GET, HEAD"),
            (Method::PUT, "/sql/saved-queries", "GET, HEAD, POST"),
            (Method::POST, "/sql/runs/x", "DELETE"),
        ] {
            let reply = send(&api, method.clone(), &path(1, rest), Some(&key())).await;
            reply.assert_problem(405, "method_not_allowed");
            assert_eq!(reply.header("allow"), Some(allow), "{method} {rest}");
        }
        // The telemetry routes still name theirs.
        let reply = send(&api, Method::POST, &path(1, "/traces"), Some(&key())).await;
        assert_eq!(reply.header("allow"), Some("GET, HEAD"));
    });
}
