//! Retention settings and deletion requests over `PgModel` (contracts 12.6 and 13): who may call them, what
//! each refusal says, the `If-Match` rule, and the idempotency of a deletion request.

use axum::http::Method;
use crucible_auth::service::SessionVerdict;
use crucible_control::{KeyScope, OrgRole};
use serde_json::{Value, json};

use crate::identity::{As, PASSWORD, call, platform_with, session};
use crate::world::{World, project};
use crate::{Reply, run};

fn base() -> String {
    format!("/api/v1/projects/{}", project(1))
}

fn detail(reply: &Reply) -> &str {
    reply.body["detail"].as_str().unwrap_or_default()
}

#[test]
fn retention_is_read_by_everyone_and_changed_by_project_admins_with_if_match() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let uri = format!("{}/retention", base());
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());

        let first = call(&p.router, Method::GET, &uri, &viewer, None, &[]).await;
        assert_eq!(first.status, 200, "{:#}", first.body);
        assert_eq!(first.header("etag"), Some("\"1\""));
        assert_eq!(
            first.body,
            json!({
                "retention_days": null,
                "payload_retention_days": null,
                "mcp_content_retention_days": null,
                "hidden_before": {"retention": null, "payload": null, "mcp_content": null},
                "version": 1,
            })
        );
        let by_key = call(&p.router, Method::GET, &uri, &p.key, None, &[]).await;
        assert_eq!(by_key.status, 200);

        let ninety = json!({
            "retention_days": 90,
            "payload_retention_days": 30,
            "mcp_content_retention_days": null,
        });
        let put = |who: &As, body: Value, headers: &'static [(&'static str, &'static str)]| {
            let (router, uri, who) = (p.router.clone(), uri.clone(), who.clone());
            async move { call(&router, Method::PUT, &uri, &who, Some(body), headers).await }
        };
        // Who: a viewer and an API key are refused, and each refusal says who can.
        let refused = put(&viewer, ninety.clone(), &[("if-match", "\"1\"")]).await;
        refused.assert_problem(403, "forbidden");
        assert!(
            detail(&refused).contains("`manage` permission"),
            "{}",
            detail(&refused)
        );
        put(&p.key, ninety.clone(), &[("if-match", "\"1\"")])
            .await
            .assert_problem(403, "forbidden");

        // If-Match.
        put(&p.owner, ninety.clone(), &[])
            .await
            .assert_problem(428, "precondition_required");
        let stale = put(&p.owner, ninety.clone(), &[("if-match", "\"7\"")]).await;
        stale.assert_problem(412, "precondition_failed");
        assert!(detail(&stale).contains("version 1"), "{}", detail(&stale));

        // Values: each refusal names the member and says what is accepted.
        let too_long = put(
            &p.owner,
            json!({"retention_days": 30, "payload_retention_days": 31, "mcp_content_retention_days": null}),
            &[("if-match", "\"1\"")],
        )
        .await;
        too_long.assert_problem(422, "validation_failed");
        assert_eq!(
            too_long.field_errors(),
            [("/payload_retention_days", "invalid_value")]
        );
        assert!(
            too_long.body["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("at most retention_days (30)")
        );
        let zero = put(
            &p.owner,
            json!({"retention_days": 0, "payload_retention_days": null, "mcp_content_retention_days": null}),
            &[("if-match", "\"1\"")],
        )
        .await;
        zero.assert_problem(422, "validation_failed");
        assert_eq!(zero.field_errors(), [("/retention_days", "invalid_value")]);
        put(
            &p.owner,
            json!({"retention_days": 30, "days": 1}),
            &[("if-match", "\"1\"")],
        )
        .await
        .assert_problem(422, "validation_failed");

        let stored = put(&p.owner, ninety, &[("if-match", "\"1\"")]).await;
        assert_eq!(stored.status, 200, "{:#}", stored.body);
        assert_eq!(stored.header("etag"), Some("\"2\""));
        assert_eq!(stored.body["retention_days"], 90);
        assert_eq!(stored.body["payload_retention_days"], 30);
        assert_eq!(stored.body["hidden_before"]["retention"], Value::Null);

        // Lengthening never shows what the shorter setting had hidden: the floor stays.
        let forever = json!({
            "retention_days": null,
            "payload_retention_days": null,
            "mcp_content_retention_days": null,
        });
        let longer = put(&p.owner, forever, &[("if-match", "\"2\"")]).await;
        assert_eq!(longer.status, 200, "{:#}", longer.body);
        assert_eq!(longer.body["retention_days"], Value::Null);
        assert!(longer.body["hidden_before"]["retention"].is_string());
        assert!(longer.body["hidden_before"]["payload"].is_string());
        // MCP content had no setting of its own, so it followed `retention_days`.
        assert_eq!(
            longer.body["hidden_before"]["mcp_content"],
            longer.body["hidden_before"]["retention"]
        );
        let read = call(&p.router, Method::GET, &uri, &p.owner, None, &[]).await;
        assert_eq!(read.body, longer.body);
        assert_eq!(read.header("etag"), Some("\"3\""));

        // A method the route does not take lists the ones it does.
        let wrong = call(&p.router, Method::DELETE, &uri, &p.owner, None, &[]).await;
        wrong.assert_problem(405, "method_not_allowed");
        assert_eq!(wrong.header("allow"), Some("GET,HEAD,PUT"));
    });
}

#[test]
fn a_deletion_request_is_durable_idempotent_and_bound_to_roles_and_scopes() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let uri = format!("{}/deletion-requests", base());
        let post = |who: &As, body: Value, headers: &'static [(&'static str, &'static str)]| {
            let (router, uri, who) = (p.router.clone(), uri.clone(), who.clone());
            async move { call(&router, Method::POST, &uri, &who, Some(body), headers).await }
        };
        let trace = "0af7651916cd43dd8448eb211c80319c";
        let by_trace = json!({"kind": "trace", "trace_ids": [trace]});

        // Who. A viewer cannot delete, and a key needs the `delete` scope; each refusal says what is missing.
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let refused = post(&viewer, by_trace.clone(), &[]).await;
        refused.assert_problem(403, "forbidden");
        assert!(
            detail(&refused).contains("`delete_traces` permission"),
            "{}",
            detail(&refused)
        );
        post(&p.key, by_trace.clone(), &[])
            .await
            .assert_problem(403, "forbidden");

        // The body: exactly the members of its kind.
        for (body, pointer, code) in [
            (json!({"kind": "trace"}), "/trace_ids", "required"),
            (
                json!({"kind": "trace", "trace_ids": ["xyz"]}),
                "/trace_ids/0",
                "invalid_id",
            ),
            (
                json!({"kind": "trace", "trace_ids": []}),
                "/trace_ids",
                "invalid_value",
            ),
            (
                json!({"kind": "user", "user_id": "ada", "trace_ids": [trace]}),
                "/trace_ids",
                "unexpected_member",
            ),
            (json!({"kind": "user"}), "/user_id", "required"),
            (
                json!({"kind": "user", "user_id": ""}),
                "/user_id",
                "invalid_value",
            ),
            (
                json!({"kind": "session", "issuer": "x", "session_id": "s"}),
                "/issuer",
                "unexpected_member",
            ),
            (json!({"kind": "session"}), "/session_id", "required"),
        ] {
            let reply = post(&p.owner, body.clone(), &[]).await;
            reply.assert_problem(422, "validation_failed");
            assert_eq!(reply.field_errors(), [(pointer, code)], "{body}");
        }
        post(&p.owner, json!({"kind": "project"}), &[])
            .await
            .assert_problem(422, "validation_failed");
        post(
            &p.owner,
            by_trace.clone(),
            &[("idempotency-key", "not a key")],
        )
        .await
        .assert_problem(422, "validation_failed");

        // Accepted: durable on this node, pending, with where to read it.
        let made = post(&p.owner, by_trace.clone(), &[("idempotency-key", "k-1")]).await;
        assert_eq!(made.status, 201, "{:#}", made.body);
        let id = made.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            made.header("location"),
            Some(format!("{uri}/{id}").as_str())
        );
        assert_eq!(made.body["kind"], "trace");
        assert_eq!(made.body["state"], "pending");
        assert_eq!(made.body["verified"], false);
        assert_eq!(made.body["trace_count"], 1);
        assert_eq!(made.body["trace_ids"], json!([trace]));
        assert!(
            made.body["requested_by"]
                .as_str()
                .unwrap()
                .starts_with("user:")
        );
        assert_eq!(made.body["places"], json!([]));

        // The same key and body is the same request; another body under the key is refused.
        let again = post(&p.owner, by_trace.clone(), &[("idempotency-key", "k-1")]).await;
        assert_eq!(again.status, 201);
        assert_eq!(again.body["id"], id.as_str());
        let other = json!({"kind": "session", "session_id": "s-1"});
        let reused = post(&p.owner, other.clone(), &[("idempotency-key", "k-1")]).await;
        reused.assert_problem(409, "idempotency_key_reused");
        assert!(detail(&reused).contains("send a new key"));
        // Without a key every call is a new request.
        let second = post(&p.owner, other, &[]).await;
        assert_eq!(second.status, 201);
        assert_ne!(second.body["id"], id.as_str());
        assert_eq!(second.body["session_id"], "s-1");

        // A project member deletes traces, and only an admin deletes by user or session.
        let As::Session { cookie, .. } = &p.owner else {
            panic!("the owner signs in with a session");
        };
        let SessionVerdict::Valid(owner) = p.identity.session(cookie).await else {
            panic!("the owner's session");
        };
        let invited = p
            .identity
            .invite(&owner, "mel@example.com", OrgRole::Member, Vec::new())
            .await
            .unwrap();
        p.identity
            .accept_invitation(invited.token.as_str(), "Mel", PASSWORD)
            .await
            .unwrap();
        let member = session(&p.identity.login("mel@example.com", PASSWORD).await.unwrap());
        let by_member = post(&member, json!({"kind": "trace", "trace_ids": [trace]}), &[]).await;
        assert_eq!(by_member.status, 201, "{:#}", by_member.body);
        for body in [
            json!({"kind": "user", "user_id": "ada"}),
            json!({"kind": "session", "session_id": "s-1"}),
        ] {
            let refused = post(&member, body, &[]).await;
            refused.assert_problem(403, "forbidden");
            assert!(
                detail(&refused).contains("`delete_by_user` permission"),
                "{}",
                detail(&refused)
            );
        }

        // A key with the `delete` scope deletes traces and by user.
        let deleter = p
            .identity
            .create_key(
                project(1),
                Some(vec![KeyScope::Read, KeyScope::Delete]),
                None,
                None,
                false,
            )
            .await
            .unwrap();
        let deleter = As::Key(deleter.secret);
        let by_user = json!({"kind": "user", "user_id": "ada", "issuer": "https://issuer.example"});
        let third = post(&deleter, by_user, &[]).await;
        assert_eq!(third.status, 201, "{:#}", third.body);
        assert_eq!(third.body["user_id"], "ada");
        assert_eq!(third.body["issuer"], "https://issuer.example");
        assert!(
            third.body["requested_by"]
                .as_str()
                .unwrap()
                .starts_with("key:")
        );

        // Reading: one request, the list newest first, and its pages.
        let one = call(
            &p.router,
            Method::GET,
            &format!("{uri}/{id}"),
            &viewer,
            None,
            &[],
        )
        .await;
        assert_eq!(one.status, 200);
        assert_eq!(one.body["id"], id.as_str());
        let unknown = format!("{uri}/{}", project(9));
        call(&p.router, Method::GET, &unknown, &p.owner, None, &[])
            .await
            .assert_problem(404, "not_found");
        call(
            &p.router,
            Method::GET,
            &format!("{uri}/nope"),
            &p.owner,
            None,
            &[],
        )
        .await
        .assert_problem(404, "not_found");
        let list = call(&p.router, Method::GET, &uri, &p.key, None, &[]).await;
        assert_eq!(list.status, 200);
        let kinds: Vec<&str> = list.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|request| request["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["user", "trace", "session", "trace"]);
        assert_eq!(list.body["next_cursor"], Value::Null);
        let page = call(
            &p.router,
            Method::GET,
            &format!("{uri}?limit=2"),
            &p.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(page.body["data"].as_array().unwrap().len(), 2);
        let cursor = page.body["next_cursor"].as_str().unwrap();
        let rest = call(
            &p.router,
            Method::GET,
            &format!("{uri}?limit=2&cursor={cursor}"),
            &p.owner,
            None,
            &[],
        )
        .await;
        assert_eq!(rest.body["data"][1]["id"], id.as_str());
        assert_eq!(rest.body["next_cursor"], Value::Null);
        call(
            &p.router,
            Method::GET,
            &format!("{uri}?cursor=x"),
            &p.owner,
            None,
            &[],
        )
        .await
        .assert_problem(400, "cursor_invalid");
        call(
            &p.router,
            Method::GET,
            &format!("{uri}?limit=0"),
            &p.owner,
            None,
            &[],
        )
        .await
        .assert_problem(422, "validation_failed");
    });
}
