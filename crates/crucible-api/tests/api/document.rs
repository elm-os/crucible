//! The OpenAPI document (contract 12.1): it matches the committed file, it is OpenAPI 3.1, and every operation
//! it documents is a route of the router.

use axum::http::{Method, StatusCode};

use crucible_api::{Access, ROUTES};

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
pub(crate) fn concrete(path: &str) -> String {
    path.replace("{project_id}", "01010101-0101-0101-0101-010101010101")
        .replace("{trace_id}", "a5a5a5a5a5a5a5a50000000000000001")
        .replace("{session_id}", "s1")
        .replace("{run_id}", "01010101-0101-7101-8101-010101010102")
        .replace("{query_id}", "01010101-0101-7101-8101-010101010103")
        .replace("{span_id}", "0000000000000001")
        .replace("{key_id}", "02020202-0202-0202-0202-020202020202")
        .replace("{user_id}", "03030303-0303-0303-0303-030303030303")
        .replace("{invitation_id}", "04040404-0404-0404-0404-040404040404")
        .replace("{provider_id}", "05050505-0505-0505-0505-050505050505")
        .replace("{export_id}", "06060606-0606-7606-8606-060606060606")
        .replace("{request_id}", "07070707-0707-7707-8707-070707070707")
        .replace("{media_id}", "08080808-0808-7808-8808-080808080808")
}

/// Every documented operation, with its method.
pub(crate) fn operations() -> Vec<(Method, String)> {
    let document = crucible_api::openapi();
    let mut operations = Vec::new();
    for (path, item) in &document.paths.paths {
        assert!(path.starts_with(crucible_api::BASE_PATH), "{path}");
        for (method, documented) in [
            (Method::GET, item.get.is_some()),
            (Method::POST, item.post.is_some()),
            (Method::PATCH, item.patch.is_some()),
            (Method::PUT, item.put.is_some()),
            (Method::DELETE, item.delete.is_some()),
        ] {
            if documented {
                operations.push((method, path.clone()));
            }
        }
    }
    operations
}

/// The permission of a documented operation.
pub(crate) fn access(method: &Method, path: &str) -> Access {
    ROUTES
        .iter()
        .find(|r| r.method == method.as_str() && r.path == path)
        .map(|r| r.access)
        .unwrap_or_else(|| panic!("{method} {path} has no authorization entry"))
}

#[test]
fn without_a_control_plane_every_route_reaches_its_handler() {
    run(|env| async move {
        let world = World::new(10);
        let api = api(&env, &world);
        let operations = operations();
        for (method, path) in &operations {
            // Without credentials a project route's handler answers 401; routes that need the control plane
            // answer feature_disabled. The fallback would answer not_found or method_not_allowed.
            let reply = send(&api, method.clone(), &concrete(path), None).await;
            match access(method, path) {
                Access::Project(_) => assert_eq!(
                    reply.status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path}: {:#}",
                    reply.body
                ),
                Access::Public | Access::Session | Access::Org(_) => {
                    reply.assert_problem(404, "feature_disabled");
                }
            }
        }
        assert_eq!(operations.len(), ROUTES.len());
    });
}
