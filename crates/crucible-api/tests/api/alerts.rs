//! The alert and channel routes over `PgModel` (contracts 11.6 and 12.6): rules with versions and field
//! errors, the history and the preview, channels whose secrets are shown once, the egress refusal, and a
//! test-send that reports an outcome and nothing the receiver said.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::Method;
use crucible_alerts::{Alerts, Observed, ReadFailure, RuleSpec, SeriesSource};
use crucible_api::AlertsApi;
use crucible_auth::Sealer;
use crucible_control::{
    AlertEvent, AlertEventKind, AlertStatus, Channel, ControlStore, Project, Repos as _,
};
use crucible_egress::EgressPolicy;
use crucible_env::Env;
use crucible_notify::{Channels, Delivery, Notification, Transport};
use crucible_records::{AlertRuleId, TimeWindow, Timestamp, Uuid};
use crucible_sim::pg_model::PgModel;
use serde_json::{Value, json};

use crate::identity::{As, PASSWORD, call, platform_of, platform_with, session};
use crate::world::{World, project};
use crate::{Reply, run};

/// Two series of the dimension `environment`: `prod` at 12 and `dev` at 3.
#[derive(Debug)]
struct TwoSeries;

#[async_trait]
impl SeriesSource for TwoSeries {
    async fn read(
        &self,
        _: &Project,
        _: &RuleSpec,
        _: TimeWindow,
    ) -> Result<Observed, ReadFailure> {
        Ok(Observed {
            series: vec![(r#"["prod"]"#.to_owned(), 12), (r#"["dev"]"#.to_owned(), 3)],
            truncated: false,
        })
    }
}

/// A receiver that answers 500 with a body the API must never show. With `mail`, the server has a mail server.
#[derive(Debug)]
struct FailingReceiver {
    mail: bool,
}

#[async_trait]
impl Transport for FailingReceiver {
    async fn deliver(&self, _: &Channel, _: Option<&[u8]>, _: &Notification, _: Uuid) -> Delivery {
        Delivery {
            ok: false,
            status_class: Some("5xx"),
            error_code: Some("receiver_failed"),
            latency_ms: 7,
        }
    }

    fn has_mailer(&self) -> bool {
        self.mail
    }
}

/// Alerting over the model, with the scripted read and receiver above and the default egress policy. With
/// `mail`, the server has a mail server (`smtp.host` is set).
pub(crate) fn api(env: &Env, model: &PgModel, mail: bool) -> AlertsApi {
    let store: Arc<dyn ControlStore> = Arc::new(model.clone());
    AlertsApi {
        alerts: Alerts::new(env, Arc::clone(&store), Some(Arc::new(TwoSeries))),
        channels: Channels::new(
            env,
            store,
            Arc::new(Sealer::new("master-0", [2; 32])),
            Arc::new(FailingReceiver { mail }),
            EgressPolicy::default(),
        ),
    }
}

fn path(rest: &str) -> String {
    format!("/api/v1/projects/{}{rest}", project(1))
}

fn rule_body() -> Value {
    json!({
        "name": "Error budget",
        "measure": "error_count",
        "aggregation": "sum",
        "dimensions": ["environment"],
        "comparison": "gt",
        "threshold": 5,
        "window_secs": 300,
    })
}

fn etag(reply: &Reply) -> String {
    reply.header("etag").expect("an ETag").to_owned()
}

/// `features.smtp` in the session DTO says whether the server sends email: whether the channels' transport has
/// the mail server of `smtp.*`, which email channels need (contract 12.7).
#[test]
fn the_session_reports_whether_the_server_sends_email() {
    run(|env| async move {
        let world = World::new(10);
        for mail in [false, true] {
            let p = platform_of(&env, &world, false, mail).await;
            let info = call(
                &p.router,
                Method::GET,
                "/api/v1/auth/session",
                &p.owner,
                None,
                &[],
            )
            .await;
            assert_eq!(info.status.as_u16(), 200, "{:#}", info.body);
            assert_eq!(info.body["features"]["smtp"], mail, "{:#}", info.body);
        }
    });
}

#[test]
fn rules_are_created_read_changed_and_deleted_under_their_version() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let send = |method: Method, rest: &str, who: &As, body: Option<Value>, m: Option<&str>| {
            let (router, uri, who) = (p.router.clone(), path(rest), who.clone());
            let m = m.map(str::to_owned);
            async move {
                let extra: Vec<(&str, &str)> = m.iter().map(|m| ("if-match", m.as_str())).collect();
                call(&router, method, &uri, &who, body, &extra).await
            }
        };

        // The defaults of the members a rule may leave out.
        let created = send(Method::POST, "/alerts", &p.owner, Some(rule_body()), None).await;
        assert_eq!(created.status.as_u16(), 201, "{:#}", created.body);
        let rule = &created.body;
        assert_eq!(rule["interval_secs"], 60);
        assert_eq!(rule["for_secs"], 0);
        assert_eq!(rule["no_data"], "keep");
        assert_eq!(rule["renotify_secs"], 14_400);
        assert_eq!(rule["status"], "ok");
        assert_eq!(rule["unit"], "count");
        assert_eq!(etag(&created), "\"1\"");
        let id = rule["id"].as_str().unwrap().to_owned();
        let one = format!("/alerts/{id}");

        // A viewer and a key read; neither writes.
        for who in [&viewer, &p.key] {
            let list = send(Method::GET, "/alerts", who, None, None).await;
            assert_eq!(
                list.body["data"].as_array().unwrap().len(),
                1,
                "{:#}",
                list.body
            );
            let refused = send(Method::POST, "/alerts", who, Some(rule_body()), None).await;
            refused.assert_problem(403, "forbidden");
        }

        // Every invalid member is named at once, by its wire name.
        let mut bad = rule_body();
        bad["window_secs"] = json!(90);
        bad["interval_secs"] = json!(10);
        bad["renotify_secs"] = json!(60);
        bad["channels"] = json!([Uuid::from_v7_parts(1_800_000_000_000, 9).to_string()]);
        let refused = send(Method::POST, "/alerts", &p.owner, Some(bad), None).await;
        refused.assert_problem(422, "validation_failed");
        let pointers: Vec<&str> = refused.field_errors().iter().map(|(p, _)| *p).collect();
        assert_eq!(
            pointers,
            [
                "/window_secs",
                "/interval_secs",
                "/renotify_secs",
                "/channels"
            ]
        );
        let mut unknown = rule_body();
        unknown["measure"] = json!("nope");
        unknown["dimensions"] = json!(["environment", "nope"]);
        let refused = send(Method::POST, "/alerts", &p.owner, Some(unknown), None).await;
        let pointers: Vec<&str> = refused.field_errors().iter().map(|(p, _)| *p).collect();
        assert_eq!(pointers, ["/measure", "/dimensions/1"]);

        // A change needs the version that was read.
        let patch = json!({ "enabled": false, "threshold": 9 });
        send(Method::PATCH, &one, &p.owner, Some(patch.clone()), None)
            .await
            .assert_problem(428, "precondition_required");
        send(
            Method::PATCH,
            &one,
            &p.owner,
            Some(patch.clone()),
            Some("\"7\""),
        )
        .await
        .assert_problem(412, "precondition_failed");
        let changed = send(Method::PATCH, &one, &p.owner, Some(patch), Some("\"1\"")).await;
        assert_eq!(changed.status.as_u16(), 200, "{:#}", changed.body);
        assert_eq!(
            (
                &changed.body["enabled"],
                &changed.body["threshold"],
                &changed.body["name"]
            ),
            (&json!(false), &json!(9), &json!("Error budget"))
        );
        assert_eq!(etag(&changed), "\"2\"");

        // Another project's rule, and a rule that is not there, are both not found.
        let other = format!("/alerts/{}", Uuid::from_v7_parts(1_800_000_000_000, 3));
        send(Method::GET, &other, &p.owner, None, None)
            .await
            .assert_problem(404, "not_found");

        send(Method::DELETE, &one, &p.owner, None, Some("\"1\""))
            .await
            .assert_problem(412, "precondition_failed");
        let deleted = send(Method::DELETE, &one, &p.owner, None, Some("\"2\"")).await;
        assert_eq!(deleted.status.as_u16(), 204);
        send(Method::GET, &one, &p.owner, None, None)
            .await
            .assert_problem(404, "not_found");
    });
}

#[test]
fn the_preview_and_the_history_show_series_by_their_dimension_values() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let preview = call(
            &p.router,
            Method::POST,
            &path("/alerts/preview"),
            &p.key,
            Some(rule_body()),
            &[],
        )
        .await;
        assert_eq!(preview.status.as_u16(), 200, "{:#}", preview.body);
        assert_eq!(
            preview.body["series"],
            json!([
                { "key": [{ "dimension": "environment", "value": "prod" }], "value": 12, "breach": true },
                { "key": [{ "dimension": "environment", "value": "dev" }], "value": 3, "breach": false },
            ])
        );
        let (from, to) = (
            Timestamp::parse_rfc3339(preview.body["from"].as_str().unwrap()).unwrap(),
            Timestamp::parse_rfc3339(preview.body["to"].as_str().unwrap()).unwrap(),
        );
        assert_eq!(to.micros() - from.micros(), 300_000_000);
        assert_eq!(to, to.floor_minute());

        let created = call(
            &p.router,
            Method::POST,
            &path("/alerts"),
            &p.owner,
            Some(rule_body()),
            &[],
        )
        .await;
        let id = created.body["id"].as_str().unwrap().to_owned();
        let rule_id = AlertRuleId::parse(&id).unwrap();
        // Three history rows, as evaluations write them.
        for n in 0..3u64 {
            p.model()
                .alert_events()
                .insert(&AlertEvent {
                    id: Uuid::from_v7_parts(1_800_000_000_000 + n, u128::from(n)),
                    rule_id,
                    series_key: r#"["prod"]"#.to_owned(),
                    kind: AlertEventKind::Transition,
                    from_status: AlertStatus::Ok,
                    to_status: AlertStatus::Firing,
                    value: Some(12),
                    notified: true,
                    eval_seq: n,
                    at: Timestamp::from_micros(1_800_000_000_000_000 + i64::try_from(n).unwrap()),
                })
                .await
                .unwrap();
        }
        let history = path(&format!("/alerts/{id}/history"));
        let first = call(
            &p.router,
            Method::GET,
            &format!("{history}?limit=2"),
            &p.key,
            None,
            &[],
        )
        .await;
        assert_eq!(first.status.as_u16(), 200, "{:#}", first.body);
        assert_eq!(first.body["data"].as_array().unwrap().len(), 2);
        assert_eq!(first.body["has_more"], true);
        assert_eq!(first.body["data"][0]["to_status"], "firing");
        assert_eq!(
            first.body["data"][0]["key"],
            json!([{ "dimension": "environment", "value": "prod" }])
        );
        let cursor = first.body["next_cursor"].as_str().unwrap();
        let second = call(
            &p.router,
            Method::GET,
            &format!("{history}?limit=2&cursor={cursor}"),
            &p.key,
            None,
            &[],
        )
        .await;
        assert_eq!(second.body["data"].as_array().unwrap().len(), 1);
        assert_eq!(second.body["has_more"], false);
        assert!(second.body.get("next_cursor").is_none());
        call(
            &p.router,
            Method::GET,
            &format!("{history}?cursor=tampered"),
            &p.key,
            None,
            &[],
        )
        .await
        .assert_problem(400, "cursor_invalid");
    });
}

#[test]
fn channels_show_their_secret_once_and_refuse_targets_with_the_setting_that_allows_them() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let post = |who: &As, rest: &str, body: Value| {
            let (router, uri, who) = (p.router.clone(), path(rest), who.clone());
            async move { call(&router, Method::POST, &uri, &who, Some(body), &[]).await }
        };

        // A private receiver over plain HTTP works by default.
        let webhook =
            json!({ "name": "On call", "kind": "webhook", "url": "http://10.0.0.8:9000/hook" });
        let created = post(&p.owner, "/channels", webhook.clone()).await;
        assert_eq!(created.status.as_u16(), 201, "{:#}", created.body);
        let secret = created.body["signing_secret"].as_str().unwrap().to_owned();
        assert!(secret.starts_with("whsec_") && secret.len() == 70);
        let id = created.body["id"].as_str().unwrap().to_owned();
        let one = path(&format!("/channels/{id}"));
        let read = call(&p.router, Method::GET, &one, &p.owner, None, &[]).await;
        assert_eq!(read.body["url"], "http://10.0.0.8:9000/hook");
        assert!(read.body.get("signing_secret").is_none(), "{:#}", read.body);

        // Channels hold secrets: a viewer and a key neither list nor manage them.
        for who in [&viewer, &p.key] {
            call(&p.router, Method::GET, &path("/channels"), who, None, &[])
                .await
                .assert_problem(403, "forbidden");
            post(who, "/channels", webhook.clone())
                .await
                .assert_problem(403, "forbidden");
            call(&p.router, Method::GET, &one, who, None, &[])
                .await
                .assert_problem(403, "forbidden");
        }

        // A Slack URL is a credential: only its hint is stored in the open.
        let slack = json!({ "name": "Slack", "kind": "slack", "url": "https://hooks.slack.com/services/T0/B0/abcdWXYZ" });
        let created = post(&p.owner, "/channels", slack).await;
        assert_eq!(created.status.as_u16(), 201, "{:#}", created.body);
        assert_eq!(
            created.body["url_hint"],
            "https://hooks.slack.com/services/...WXYZ"
        );
        assert!(!created.body.to_string().contains("abcdWXYZ"));

        // The metadata address is refused under every setting, and the refusal says so.
        let metadata =
            json!({ "name": "x", "kind": "webhook", "url": "http://169.254.169.254/latest" });
        let refused = post(&p.owner, "/channels", metadata).await;
        refused.assert_problem(422, "ssrf_blocked");
        assert_eq!(refused.field_errors(), [("/url", "metadata_address")]);
        assert!(
            refused.body["detail"]
                .as_str()
                .unwrap()
                .contains("no setting allows it")
        );

        // Email needs the mail server, and the refusal names its settings.
        let email = json!({ "name": "Mail", "kind": "email", "to": ["ada@example.com"] });
        let refused = post(&p.owner, "/channels", email).await;
        refused.assert_problem(422, "validation_failed");
        assert_eq!(refused.field_errors(), [("/kind", "smtp_not_configured")]);
        assert!(refused.body.to_string().contains("smtp.host"));

        // Rotation shows the new secret once, under the version.
        let rotate = json!({ "rotate_secret": true });
        let rotated = call(
            &p.router,
            Method::PATCH,
            &one,
            &p.owner,
            Some(rotate),
            &[("if-match", "\"1\"")],
        )
        .await;
        assert_eq!(rotated.status.as_u16(), 200, "{:#}", rotated.body);
        let renewed = rotated.body["signing_secret"].as_str().unwrap();
        assert!(renewed.starts_with("whsec_") && renewed != secret);

        // A test-send reports the class of the answer, never the answer; the outcome stays on the channel.
        let test = format!("/channels/{id}/test");
        let outcome = post(&p.owner, &test, json!({})).await;
        assert_eq!(outcome.status.as_u16(), 200, "{:#}", outcome.body);
        assert_eq!(
            outcome.body,
            json!({ "ok": false, "status_class": "5xx", "latency_ms": 7, "error_code": "receiver_failed" })
        );
        let read = call(&p.router, Method::GET, &one, &p.owner, None, &[]).await;
        assert_eq!(read.body["last_test"]["error_code"], "receiver_failed");
        assert_eq!(
            read.body["failures"], 0,
            "a test does not count against the channel"
        );
        // Six a minute per channel.
        for _ in 0..5 {
            assert_eq!(post(&p.owner, &test, json!({})).await.status.as_u16(), 200);
        }
        let limited = post(&p.owner, &test, json!({})).await;
        limited.assert_problem(429, "rate_limited");
        assert!(limited.header("retry-after").is_some());

        // A rule that notifies the channel stays editable after the channel is deleted: the dead id is
        // dropped, while adding a channel that does not exist is still refused.
        let mut body = rule_body();
        body["channels"] = json!([id]);
        let created = post(&p.owner, "/alerts", body).await;
        assert_eq!(created.status.as_u16(), 201, "{:#}", created.body);
        let rule = path(&format!("/alerts/{}", created.body["id"].as_str().unwrap()));
        let deleted = call(
            &p.router,
            Method::DELETE,
            &one,
            &p.owner,
            None,
            &[("if-match", "\"2\"")],
        )
        .await;
        assert_eq!(deleted.status.as_u16(), 204);
        let off = call(
            &p.router,
            Method::PATCH,
            &rule,
            &p.owner,
            Some(json!({ "enabled": false })),
            &[("if-match", "\"1\"")],
        )
        .await;
        assert_eq!(off.status.as_u16(), 200, "{:#}", off.body);
        assert_eq!(off.body["channels"], json!([]));
        let stranger = Uuid::from_v7_parts(1_800_000_000_000, 77).to_string();
        call(
            &p.router,
            Method::PATCH,
            &rule,
            &p.owner,
            Some(json!({ "channels": [stranger] })),
            &[("if-match", "\"2\"")],
        )
        .await
        .assert_problem(422, "validation_failed");
    });
}

/// An `mcp_ops` rule: the server-side error rate per server, above 5%.
fn mcp_rule_body() -> Value {
    json!({
        "name": "MCP errors",
        "source": "mcp_ops",
        "side": "server",
        "measure": "mcp.error_rate",
        "aggregation": "rate",
        "dimensions": ["server_name"],
        "comparison": "gt",
        "threshold": 50_000,
        "window_secs": 300,
    })
}

#[test]
fn an_mcp_rule_names_its_side_and_capture_source() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let send = |method: Method, rest: &str, body: Value, m: Option<&str>| {
            let (router, uri, who) = (p.router.clone(), path(rest), p.owner.clone());
            let m = m.map(str::to_owned);
            async move {
                let extra: Vec<(&str, &str)> = m.iter().map(|m| ("if-match", m.as_str())).collect();
                call(&router, method, &uri, &who, Some(body), &extra).await
            }
        };
        let refused_at = |reply: &Reply| -> Vec<String> {
            reply.assert_problem(422, "validation_failed");
            reply
                .field_errors()
                .iter()
                .map(|(p, _)| (*p).to_owned())
                .collect()
        };

        // The capture source is `auto` unless one is named, and the unit of a rate is parts per million.
        let created = send(Method::POST, "/alerts", mcp_rule_body(), None).await;
        assert_eq!(created.status.as_u16(), 201, "{:#}", created.body);
        let rule = &created.body;
        assert_eq!(
            (&rule["source"], &rule["side"], &rule["capture_source"]),
            (&json!("mcp_ops"), &json!("server"), &json!("auto"))
        );
        assert_eq!(rule["unit"], "ppm");
        let one = format!("/alerts/{}", rule["id"].as_str().unwrap());

        // An MCP rule names its side, reads its own dimensions, and a capture source needs a side.
        let mut no_side = mcp_rule_body();
        no_side.as_object_mut().unwrap().remove("side");
        let reply = send(Method::POST, "/alerts", no_side.clone(), None).await;
        assert_eq!(refused_at(&reply), ["/side"]);
        no_side["capture_source"] = json!("proxy");
        let reply = send(Method::POST, "/alerts", no_side, None).await;
        assert_eq!(refused_at(&reply), ["/side"]);
        let mut spans_dimension = mcp_rule_body();
        spans_dimension["dimensions"] = json!(["environment"]);
        let reply = send(Method::POST, "/alerts", spans_dimension, None).await;
        assert_eq!(refused_at(&reply), ["/dimensions"]);
        // A spans rule takes no side.
        let mut spans_side = rule_body();
        spans_side["side"] = json!("client");
        let reply = send(Method::POST, "/alerts", spans_side, None).await;
        assert_eq!(refused_at(&reply), ["/side"]);

        // A patch of the capture source keeps the side.
        let patch = json!({ "capture_source": "proxy" });
        let changed = send(Method::PATCH, &one, patch, Some("\"1\"")).await;
        assert_eq!(changed.status.as_u16(), 200, "{:#}", changed.body);
        assert_eq!(
            (&changed.body["side"], &changed.body["capture_source"]),
            (&json!("server"), &json!("proxy"))
        );

        // On a spans rule a side is refused.
        let spans = send(Method::POST, "/alerts", rule_body(), None).await;
        let spans_one = format!("/alerts/{}", spans.body["id"].as_str().unwrap());
        assert!(spans.body.get("side").is_none(), "{:#}", spans.body);
        let reply = send(
            Method::PATCH,
            &spans_one,
            json!({ "side": "server" }),
            Some("\"1\""),
        )
        .await;
        assert_eq!(refused_at(&reply), ["/side"]);
    });
}
