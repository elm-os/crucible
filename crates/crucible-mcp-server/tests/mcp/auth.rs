//! Authentication before rmcp, project isolation, and the server before a store is up (contract 11.6, 12.2 and
//! 12.3).

use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::world::{NOW, World, span, trace};
use crate::{Client, key, mcp, mcp_without_store, public_key, run, send};

const SECOND: i64 = 1_000_000;

fn initialize() -> serde_json::Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": crate::LEGACY,
            "capabilities": {},
            "clientInfo": { "name": "crucible-test", "version": "1.0.0" },
        },
    })
}

/// The key pair as Basic credentials (contract 12.2).
fn basic(public: &str, secret: &str) -> String {
    use base64::Engine as _;
    let pair = format!("{public}:{secret}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

#[test]
fn requests_without_a_valid_key_are_refused_with_what_to_fix() {
    run(|env| async move {
        let world = World::new(10);
        let mcp = mcp(&env, &world);
        let unknown = key(3);
        let cases = [
            (None, "send the project's secret key"),
            (
                Some("Token abc".to_owned()),
                "must be `Bearer <secret key>`",
            ),
            (
                Some("Bearer csk_short".to_owned()),
                "is not a Crucible secret key",
            ),
            (
                Some(format!("Bearer {unknown}")),
                "does not accept the secret key",
            ),
            (
                Some(format!("Bearer {}", public_key(1))),
                "a public key alone is not accepted",
            ),
            (
                Some(basic(&public_key(2), &key(1))),
                "does not belong to the same project",
            ),
        ];
        for (authorization, says) in cases {
            for method in [Method::POST, Method::GET, Method::DELETE] {
                let headers: Vec<(&str, String)> = authorization
                    .iter()
                    .map(|value| ("authorization", value.clone()))
                    .collect();
                let body = (method == Method::POST).then(initialize);
                let reply = send(&mcp, method.clone(), &headers, body.as_ref()).await;
                reply.assert_problem(401, "unauthenticated");
                assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
                let detail = reply.body["detail"].as_str().unwrap();
                assert!(
                    detail.contains(says),
                    "{method} {authorization:?}: {detail}"
                );
                assert!(!detail.contains(&unknown[4..]) && !detail.contains(&key(1)[4..]));
            }
        }
        // The key pair as Basic is accepted, like a Bearer secret.
        let headers = [("authorization", basic(&public_key(1), &key(1)))];
        let reply = send(&mcp, Method::POST, &headers, Some(&initialize())).await;
        assert_eq!(reply.status, StatusCode::OK, "{:#}", reply.body);
        assert!(reply.header("mcp-session-id").is_some());
    });
}

#[test]
fn another_projects_trace_is_not_found() {
    run(|env| async move {
        let mut world = World::new(100);
        let start = NOW.saturating_add_micros(-60 * SECOND);
        world.ack(vec![span(1, 10, 0, start), span(1, 11, 10, start)]);
        world.settle();
        world.ack(vec![span(2, 20, 0, start)]);
        let mcp = mcp(&env, &world);
        let mut own = Client::modern(&mcp, &key(1));
        let mut other = Client::modern(&mcp, &key(2));
        for id in [trace(1), trace(2)] {
            let trace_id = json!({ "trace_id": id.to_string() });
            let found = own.output("get_trace", trace_id.clone()).await;
            assert!(!found["spans"].as_array().unwrap().is_empty());
            let problem = other.error("get_trace", trace_id).await;
            assert_eq!(problem["code"], "not_found", "{problem:#}");
            assert_eq!(problem["status"], 404);
            let children =
                json!({ "trace_id": id.to_string(), "parent_span_id": "000000000000000a" });
            let problem = other.error("get_span_children", children).await;
            assert_eq!(problem["code"], "not_found", "{problem:#}");
        }
    });
}

#[test]
fn without_a_store_the_server_authenticates_then_says_which_setting_provides_one() {
    run(|env| async move {
        let mcp = mcp_without_store(&env);
        send(&mcp, Method::POST, &[], Some(&initialize()))
            .await
            .assert_problem(401, "unauthenticated");
        let headers = [("authorization", format!("Bearer {}", key(1)))];
        let reply = send(&mcp, Method::POST, &headers, Some(&initialize())).await;
        reply.assert_problem(503, "telemetry_store_unavailable");
        let detail = reply.body["detail"].as_str().unwrap();
        assert!(detail.contains("--clickhouse-url"), "{detail}");
    });
}

/// A verifier that cannot decide any key now, as while Postgres is unreachable and the key is not in the
/// snapshot (contract 11.8).
#[derive(Debug)]
struct Undecided;

impl crucible_auth::KeyVerifier for Undecided {
    fn verify(&self, _secret: &str) -> Option<crucible_auth::KeyPrincipal> {
        None
    }

    fn verify_pair(
        &self,
        _public: &str,
        _secret: &str,
    ) -> Result<crucible_auth::KeyPrincipal, crucible_auth::PairError> {
        Err(crucible_auth::PairError::Secret)
    }

    fn retry_after(&self, _secret: &str) -> Option<std::time::Duration> {
        Some(crucible_auth::RETRY_AFTER)
    }
}

/// A key the control plane knows that lacks the `read` scope is refused with 403 `key_scope_missing` and what to
/// do, by secret and by pair (contracts 1.7 and 11.3); a key it does not know keeps the 401, and a key with the
/// scope passes on to the server.
#[test]
fn a_known_key_without_the_read_scope_is_403_with_the_scope_it_lacks() {
    use crucible_auth::{DbKeys, LookupKey, OutageMode, ScopeGap, Scopes};
    use crucible_control::{ApiKey, KeyScope, KeySnapshot};
    use crucible_records::{ApiKeyId, Uuid};

    use crate::world::project;

    run(|env| async move {
        let lookup = LookupKey::new([4; 32]);
        let rng = crucible_env::ChaChaRng::from_u64(4);
        let (sender, reader) = (
            crucible_auth::mint(&rng, &lookup),
            crucible_auth::mint(&rng, &lookup),
        );
        let row = |n: u8, minted: &crucible_auth::MintedKey, scopes: &[KeyScope]| ApiKey {
            id: ApiKeyId::from_uuid(Uuid::from_v7_parts(1_800_000_000_000, u128::from(n))),
            project_id: project(1),
            public_key: minted.public_key.clone(),
            secret_salt: minted.salt,
            secret_hash: minted.hash,
            lookup_tag: minted.lookup_tag,
            scopes: scopes.to_vec(),
            import: false,
            note: String::new(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
            version: 1,
        };
        let keys = DbKeys::new(&env, lookup, OutageMode::default());
        keys.load(&KeySnapshot {
            max_seq: 1,
            keys: vec![
                row(1, &sender, &[KeyScope::Ingest]),
                row(2, &reader, &[KeyScope::Read]),
            ],
        });
        let mcp = crucible_mcp_server::router_without_store(
            &env,
            std::sync::Arc::new(std::sync::Arc::new(keys).with_scope(KeyScope::Read)),
        );
        let says = ScopeGap {
            needed: KeyScope::Read,
            has: Scopes::of(&[KeyScope::Ingest]),
        }
        .to_string();
        let sender_pair = basic(&sender.public_key, &sender.secret);
        for authorization in [format!("Bearer {}", sender.secret), sender_pair] {
            let headers = [("authorization", authorization)];
            let reply = send(&mcp, Method::POST, &headers, Some(&initialize())).await;
            reply.assert_problem(403, "key_scope_missing");
            assert_eq!(reply.body["detail"], says.as_str());
            assert_eq!(reply.header("www-authenticate"), None);
        }
        let headers = [("authorization", format!("Bearer {}", key(9)))];
        let reply = send(&mcp, Method::POST, &headers, Some(&initialize())).await;
        reply.assert_problem(401, "unauthenticated");
        let detail = reply.body["detail"].as_str().unwrap();
        assert!(detail.contains("does not accept the secret key"), "{detail}");
        let headers = [("authorization", format!("Bearer {}", reader.secret))];
        let reply = send(&mcp, Method::POST, &headers, Some(&initialize())).await;
        reply.assert_problem(503, "telemetry_store_unavailable");
    });
}

#[test]
fn a_key_the_control_plane_cannot_check_now_is_503_with_retry_after() {
    run(|env| async move {
        let mcp = crucible_mcp_server::router_without_store(&env, std::sync::Arc::new(Undecided));
        let headers = [("authorization", format!("Bearer {}", key(1)))];
        let reply = send(&mcp, Method::POST, &headers, Some(&initialize())).await;
        reply.assert_problem(503, "control_plane_unavailable");
        let seconds = crucible_auth::RETRY_AFTER.as_secs().to_string();
        assert_eq!(reply.header("retry-after"), Some(seconds.as_str()));
    });
}
