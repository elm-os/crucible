//! The control-plane routes over `PgModel` (contracts 11.3 to 11.6 and 12.6): the authorization route walk,
//! sessions with CSRF and origin checks, and keys that work and stop at once.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use crucible_api::{Access, StaticProjectSettings};
use crucible_auth::csrf::Origins;
use crucible_auth::password::HashCost;
use crucible_auth::service::{Identity, IdentityConfig, SessionVerdict};
use crucible_auth::{DbKeys, LookupKey, OutageMode, Sealer};
use crucible_control::{JsonDoc, OrgRole, Project, ProjectRole, Repos as _};
use crucible_env::Env;
use crucible_sim::pg_model::{PgFaults, PgModel};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::document::{access, concrete, operations};
use crate::world::{World, project};
use crate::{Reply, cursor_keys, run};

const HOST: &str = "crucible.test";
pub(crate) const PASSWORD: &str = "correct horse battery";

/// How a request authenticates.
#[derive(Clone, Debug)]
pub(crate) enum As {
    Nobody,
    Session { cookie: String, csrf: String },
    Key(String),
}

pub(crate) async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    who: &As,
    body: Option<Value>,
    extra: &[(&str, &str)],
) -> Reply {
    let mut request = Request::builder()
        .method(method.clone())
        .uri(uri)
        .header(header::HOST, HOST);
    match who {
        As::Nobody => {}
        As::Session { cookie, csrf } => {
            request = request.header(header::COOKIE, format!("crucible_session={cookie}"));
            if method != Method::GET && method != Method::HEAD {
                request = request.header("x-crucible-csrf", csrf);
                if !extra.iter().any(|(name, _)| *name == "origin") {
                    request = request.header(header::ORIGIN, format!("http://{HOST}"));
                }
            }
        }
        As::Key(secret) => {
            request = request.header(header::AUTHORIZATION, format!("Bearer {secret}"));
        }
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// The session cookie a response sets.
fn cookie_of(reply: &Reply) -> String {
    reply
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find_map(|c| c.strip_prefix("crucible_session="))
        .map(|rest| rest.split(';').next().unwrap().to_owned())
        .expect("a session cookie")
}

pub(crate) struct Platform {
    pub(crate) router: Router,
    pub(crate) identity: Arc<Identity>,
    model: PgModel,
    pub(crate) owner: As,
    pub(crate) key: As,
}

impl Platform {
    /// The control store behind the platform.
    pub(crate) fn model(&self) -> &PgModel {
        &self.model
    }
}

pub(crate) fn session(new: &crucible_auth::service::NewSession) -> As {
    As::Session {
        cookie: new.token.as_str().to_owned(),
        csrf: new.principal.session.csrf_token.clone(),
    }
}

async fn platform(env: &Env, world: &World) -> Platform {
    platform_with(env, world, false).await
}

/// The platform over `PgModel`: an owner, the viewer `vic@example.com`, project 1 and a key with the scopes of
/// a new key. With `sql`, the SQL workspace runs over the simulation's scripted store, its probe clean.
pub(crate) async fn platform_with(env: &Env, world: &World, sql: bool) -> Platform {
    let model = PgModel::new(
        Arc::clone(&env.clock),
        env.rng.fork("pg").as_ref(),
        PgFaults::default(),
    );
    let lookup = LookupKey::new([1; 32]);
    let keys = Arc::new(DbKeys::new(env, lookup.clone(), OutageMode::default()));
    keys.load(&model.key_feed().snapshot().await.unwrap());
    let mut config = IdentityConfig::new(Origins::default(), None);
    config.hash_cost = HashCost {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    };
    let identity = Arc::new(
        Identity::new(
            env,
            Arc::new(model.clone()),
            Sealer::new("master-0", [2; 32]),
            lookup,
            keys,
            config,
        )
        .unwrap(),
    );
    let link = identity.issue_setup_token().await.unwrap().unwrap();
    let owner = identity
        .setup(
            link.token.as_str(),
            "Acme",
            "owner@example.com",
            "Olivia",
            PASSWORD,
        )
        .await
        .unwrap();
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
    identity.forget_sessions();
    let SessionVerdict::Valid(principal) = identity.session(owner.token.as_str()).await else {
        panic!("the owner's session");
    };
    let invited = identity
        .invite(&principal, "vic@example.com", OrgRole::Viewer, Vec::new())
        .await
        .unwrap();
    identity
        .accept_invitation(invited.token.as_str(), "Vic", PASSWORD)
        .await
        .unwrap();
    let created = identity
        .create_key(project(1), None, None, None, false)
        .await
        .unwrap();
    let writer: Arc<dyn crucible_api::FactWriter> = crate::facts::FakeWriter::new(world);
    let workspace = if sql {
        let workspace = crucible_sqlws::SqlWorkspace::new(
            env.clone(),
            crucible_sim::sql_stub::ScriptedSqlStore::new(env),
            Arc::new(model.clone()),
            true,
        );
        workspace.probe().await;
        Some(workspace)
    } else {
        None
    };
    let router = crucible_api::platform_router(
        env,
        Some((world.service.clone(), writer)),
        Arc::clone(&identity),
        Arc::new(StaticProjectSettings::default()),
        cursor_keys(),
        workspace,
        Some(crate::alerts::api(env, &model)),
    );
    Platform {
        router,
        owner: session(&owner),
        key: As::Key(created.secret),
        identity,
        model,
    }
}

/// A response from the route's handler, not from the fallback.
fn reached(reply: &Reply) -> bool {
    reply.status != StatusCode::METHOD_NOT_ALLOWED
        && reply.body["detail"] != "no resource exists at this path"
}

fn refused(reply: &Reply, status: u16) -> bool {
    reply.status.as_u16() == status
}

#[test]
fn every_route_refuses_callers_without_its_permission() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform(&env, &world).await;
        let project_1 = project(1).to_string();
        for (method, path) in operations() {
            let uri = concrete(&path).replace("01010101-0101-0101-0101-010101010101", &project_1);
            let label = format!("{method} {path}");
            let access = access(&method, &path);
            let body = (method != Method::GET).then(|| json!({}));
            let anonymous = call(
                &p.router,
                method.clone(),
                &uri,
                &As::Nobody,
                body.clone(),
                &[],
            )
            .await;
            // A fresh viewer session per route: some routes (sign-out) end it.
            let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
            let as_viewer = call(&p.router, method.clone(), &uri, &viewer, body.clone(), &[]).await;
            let as_key = call(&p.router, method.clone(), &uri, &p.key, body.clone(), &[]).await;
            match access {
                Access::Public => {
                    assert!(reached(&anonymous), "{label}: {:#}", anonymous.body);
                    assert!(!refused(&anonymous, 401), "{label}");
                }
                Access::Session => {
                    assert!(refused(&anonymous, 401), "{label}: {:#}", anonymous.body);
                    assert!(
                        reached(&as_viewer)
                            && !refused(&as_viewer, 401)
                            && !refused(&as_viewer, 403),
                        "{label}: {:#}",
                        as_viewer.body
                    );
                    assert!(refused(&as_key, 403), "{label}: keys never act as users");
                }
                Access::Org(permission) => {
                    assert!(refused(&anonymous, 401), "{label}");
                    let allowed = permission.allowed_for(OrgRole::Viewer);
                    assert_eq!(
                        refused(&as_viewer, 403),
                        !allowed,
                        "{label}: {:#}",
                        as_viewer.body
                    );
                    assert!(refused(&as_key, 403), "{label}: keys never act as users");
                }
                Access::Project(permission) => {
                    assert!(refused(&anonymous, 401), "{label}");
                    let allowed = permission.allowed_for(ProjectRole::Viewer);
                    assert_eq!(
                        refused(&as_viewer, 403),
                        !allowed,
                        "{label}: {:#}",
                        as_viewer.body
                    );
                    // The key has the scopes of a new key: ingest, read and write.
                    let key_allowed = permission
                        .key_scope()
                        .is_some_and(|scope| crucible_auth::Scopes::DEFAULT.has(scope));
                    assert_eq!(
                        refused(&as_key, 403),
                        !key_allowed,
                        "{label}: {:#}",
                        as_key.body
                    );
                }
            }
        }
    });
}

#[test]
fn sign_in_sets_a_session_that_needs_csrf_and_origin_for_writes() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform(&env, &world).await;
        let login = json!({"email": "owner@example.com", "password": PASSWORD});
        let prelogin = [
            ("x-crucible-csrf", "prelogin"),
            ("origin", "http://crucible.test"),
        ];
        // Pre-session POSTs need the prelogin header and a same-origin request.
        call(
            &p.router,
            Method::POST,
            "/api/v1/auth/login",
            &As::Nobody,
            Some(login.clone()),
            &[("origin", "http://crucible.test")],
        )
        .await
        .assert_problem(403, "csrf_invalid");
        call(
            &p.router,
            Method::POST,
            "/api/v1/auth/login",
            &As::Nobody,
            Some(login.clone()),
            &[
                ("x-crucible-csrf", "prelogin"),
                ("origin", "http://evil.example"),
            ],
        )
        .await
        .assert_problem(403, "origin_invalid");
        let wrong = json!({"email": "owner@example.com", "password": "wrong password!!"});
        call(
            &p.router,
            Method::POST,
            "/api/v1/auth/login",
            &As::Nobody,
            Some(wrong),
            &prelogin,
        )
        .await
        .assert_problem(401, "invalid_credentials");
        let signed_in = call(
            &p.router,
            Method::POST,
            "/api/v1/auth/login",
            &As::Nobody,
            Some(login),
            &prelogin,
        )
        .await;
        assert_eq!(signed_in.status, StatusCode::OK, "{:#}", signed_in.body);
        let set_cookie = signed_in.header("set-cookie").unwrap().to_owned();
        assert!(
            set_cookie.contains("HttpOnly")
                && set_cookie.contains("SameSite=Lax")
                && !set_cookie.contains("Secure"),
            "{set_cookie}"
        );
        assert_eq!(signed_in.body["org"]["role"], "owner");
        assert_eq!(signed_in.body["projects"][0]["permissions"][0], "read");
        let me = As::Session {
            cookie: cookie_of(&signed_in),
            csrf: signed_in.body["csrf_token"].as_str().unwrap().to_owned(),
        };
        let info = call(
            &p.router,
            Method::GET,
            "/api/v1/auth/session",
            &me,
            None,
            &[],
        )
        .await;
        assert_eq!(info.body["user"]["email"], "owner@example.com");
        // A cookie-authenticated write without the CSRF token, or from another origin, is refused.
        let As::Session { cookie, .. } = &me else {
            unreachable!()
        };
        let bare = As::Session {
            cookie: cookie.clone(),
            csrf: "wrong".to_owned(),
        };
        call(
            &p.router,
            Method::PATCH,
            "/api/v1/account",
            &bare,
            Some(json!({"name": "O"})),
            &[],
        )
        .await
        .assert_problem(403, "csrf_invalid");
        call(
            &p.router,
            Method::PATCH,
            "/api/v1/account",
            &me,
            Some(json!({"name": "O"})),
            &[("origin", "http://evil.example")],
        )
        .await
        .assert_problem(403, "origin_invalid");
        call(
            &p.router,
            Method::PATCH,
            "/api/v1/account",
            &me,
            Some(json!({"name": "O"})),
            &[("sec-fetch-site", "cross-site")],
        )
        .await
        .assert_problem(403, "origin_invalid");
        let renamed = call(
            &p.router,
            Method::PATCH,
            "/api/v1/account",
            &me,
            Some(json!({"name": "Olive"})),
            &[],
        )
        .await;
        assert_eq!(renamed.body["name"], "Olive", "{:#}", renamed.body);
        // A session reads telemetry like a key does.
        let bounds = call(
            &p.router,
            Method::GET,
            &format!("/api/v1/projects/{}/data-bounds", project(1)),
            &me,
            None,
            &[],
        )
        .await;
        assert_eq!(bounds.status, StatusCode::OK, "{:#}", bounds.body);
        // Sign-out ends the session.
        let out = call(
            &p.router,
            Method::POST,
            "/api/v1/auth/logout",
            &me,
            None,
            &[],
        )
        .await;
        assert_eq!(out.status, StatusCode::NO_CONTENT);
        assert!(out.header("set-cookie").unwrap().contains("Max-Age=0"));
        call(
            &p.router,
            Method::GET,
            "/api/v1/auth/session",
            &me,
            None,
            &[],
        )
        .await
        .assert_problem(401, "unauthenticated");
    });
}

#[test]
fn keys_made_through_the_api_work_at_once_and_stop_at_once() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform(&env, &world).await;
        let keys_path = format!("/api/v1/projects/{}/api-keys", project(1));
        let created = call(
            &p.router,
            Method::POST,
            &keys_path,
            &p.owner,
            Some(json!({"note": "ci", "scopes": ["read"]})),
            &[],
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:#}", created.body);
        let secret = created.body["secret_key"].as_str().unwrap().to_owned();
        let id = created.body["key"]["id"].as_str().unwrap().to_owned();
        let etag = created.header("etag").unwrap().to_owned();
        let bounds = format!("/api/v1/projects/{}/data-bounds", project(1));
        let key = As::Key(secret.clone());
        assert_eq!(
            call(&p.router, Method::GET, &bounds, &key, None, &[])
                .await
                .status,
            StatusCode::OK
        );
        // The list never shows a secret.
        let list = call(&p.router, Method::GET, &keys_path, &p.owner, None, &[]).await;
        assert!(!list.pretty().contains(&secret));
        // A revocation needs If-Match, and stops the key before the response returns.
        let one = format!("{keys_path}/{id}");
        call(&p.router, Method::DELETE, &one, &p.owner, None, &[])
            .await
            .assert_problem(428, "precondition_required");
        call(
            &p.router,
            Method::DELETE,
            &one,
            &p.owner,
            None,
            &[("if-match", "\"9\"")],
        )
        .await
        .assert_problem(412, "precondition_failed");
        let revoked = call(
            &p.router,
            Method::DELETE,
            &one,
            &p.owner,
            None,
            &[("if-match", etag.as_str())],
        )
        .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{:#}", revoked.body);
        call(&p.router, Method::GET, &bounds, &key, None, &[])
            .await
            .assert_problem(401, "unauthenticated");
        // During a Postgres outage an open session survives a page reload: the sign-in mode and the session both
        // answer, from this node's last read and its session cache.
        let owner_view = call(
            &p.router,
            Method::GET,
            "/api/v1/auth/providers",
            &As::Nobody,
            None,
            &[],
        )
        .await;
        assert_eq!(owner_view.status, StatusCode::OK);
        p.model.set_faults(PgFaults {
            unavailable_ppm: 1_000_000,
            ..PgFaults::default()
        });
        env.clock.sleep(std::time::Duration::from_secs(6)).await;
        let reload = call(
            &p.router,
            Method::GET,
            "/api/v1/auth/providers",
            &As::Nobody,
            None,
            &[],
        )
        .await;
        assert_eq!(reload.status, StatusCode::OK, "{:#}", reload.body);
        let session = call(
            &p.router,
            Method::GET,
            "/api/v1/auth/session",
            &p.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(session.status, StatusCode::OK, "{:#}", session.body);
        // During a Postgres outage an unknown key is 503 with Retry-After, and a known one keeps working.
        env.clock.sleep(crucible_auth::OUTAGE_AFTER).await;
        let unknown = call(
            &p.router,
            Method::GET,
            &bounds,
            &As::Key(format!("csk_{}", "7".repeat(43))),
            None,
            &[],
        )
        .await;
        unknown.assert_problem(503, "control_plane_unavailable");
        assert_eq!(unknown.header("retry-after"), Some("5"));
        assert_eq!(
            call(&p.router, Method::GET, &bounds, &p.key, None, &[])
                .await
                .status,
            StatusCode::OK
        );
    });
}

#[test]
fn invitations_through_the_api() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform(&env, &world).await;
        let org = call(&p.router, Method::GET, "/api/v1/org", &p.owner, None, &[]).await;
        assert_eq!(org.header("etag"), Some("\"1\""));
        let invite = json!({"email": "ada@example.com", "org_role": "member", "project_roles": [{"project_id": project(1).to_string(), "role": "admin"}]});
        let created = call(
            &p.router,
            Method::POST,
            "/api/v1/org/invitations",
            &p.owner,
            Some(invite),
            &[],
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:#}", created.body);
        let path = created.body["accept_path"].as_str().unwrap();
        let token = path.strip_prefix("/invite/?token=").unwrap();
        let accept = json!({"token": token, "name": "Ada", "password": PASSWORD});
        let prelogin = [
            ("x-crucible-csrf", "prelogin"),
            ("origin", "http://crucible.test"),
        ];
        let accepted = call(
            &p.router,
            Method::POST,
            "/api/v1/auth/invitations/accept",
            &As::Nobody,
            Some(accept.clone()),
            &prelogin,
        )
        .await;
        assert_eq!(accepted.status, StatusCode::OK, "{:#}", accepted.body);
        let granted = accepted.body["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == project(1).to_string().as_str())
            .unwrap();
        assert_eq!(granted["role"], "admin");
        // Every project is visible to an org member (project access `all`), with the org role elsewhere.
        assert_eq!(accepted.body["projects"].as_array().unwrap().len(), 2);
        call(
            &p.router,
            Method::POST,
            "/api/v1/auth/invitations/accept",
            &As::Nobody,
            Some(accept),
            &prelogin,
        )
        .await
        .assert_problem(422, "token_invalid");
        let members = call(
            &p.router,
            Method::GET,
            "/api/v1/org/members",
            &p.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(members.body["data"].as_array().unwrap().len(), 3);
    });
}
