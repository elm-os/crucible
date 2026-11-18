//! The OpenAPI document (contract 12.1): it matches the committed file, it is OpenAPI 3.1, and every operation
//! it documents is a route of the router.

use axum::http::{Method, StatusCode};

use crate::world::World;
use crate::{api, run, send};

const COMMITTED: &str = include_str!("../../../../specs/openapi/crucible-v1.json");

#[test]
fn the_document_matches_the_committed_file() {
    let generated = crucible_api::openapi_json().unwrap();
    assert!(
        generated == COMMITTED,
        "specs/openapi/crucible-v1.json is stale; regenerate it with \
         `cargo run -p crucible -- openapi > specs/openapi/crucible-v1.json`"
    );
    let document: serde_json::Value = serde_json::from_str(&generated).unwrap();
    let version = document["openapi"].as_str().unwrap();
    assert!(version.starts_with("3.1."), "{version}");
    assert_eq!(document["info"]["version"], env!("CARGO_PKG_VERSION"));
}

/// A documented path with its parameters filled in.
fn concrete(path: &str) -> String {
    path.replace("{project_id}", "01010101-0101-0101-0101-010101010101")
        .replace("{trace_id}", "a5a5a5a5a5a5a5a50000000000000001")
        .replace("{run_id}", "01010101-0101-7101-8101-010101010102")
        .replace("{query_id}", "01010101-0101-7101-8101-010101010103")
}

#[test]
fn every_documented_operation_reaches_a_handler() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let document = crucible_api::openapi();
        let mut operations = 0;
        for (path, item) in &document.paths.paths {
            assert!(path.starts_with(crucible_api::BASE_PATH), "{path}");
            let methods = [
                (Method::GET, item.get.is_some()),
                (Method::POST, item.post.is_some()),
                (Method::PATCH, item.patch.is_some()),
                (Method::DELETE, item.delete.is_some()),
            ];
            for (method, documented) in methods {
                if !documented {
                    continue;
                }
                operations += 1;
                // Without a key, a route's handler answers 401; the fallback would answer 404 or 405.
                let reply = send(&api, method.clone(), &concrete(path), None).await;
                assert_eq!(
                    reply.status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path}: {:#}",
                    reply.body
                );
            }
        }
        // Four telemetry reads and the eleven operations of the SQL workspace.
        assert_eq!(operations, 15);
    });
}
