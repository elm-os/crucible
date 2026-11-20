//! The fact routes (contract 3.8, 12.5 and 12.6): writes that are read back, `Idempotency-Key`, the problems
//! of a write with their pointers, and the refusals of the writer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{Method, StatusCode};
use crucible_api::{AcceptedFact, FactAppend, FactAuthor, FactRefusal, FactWriter, NewFact};
use crucible_plan::Value;
use crucible_records::{FactBody, FactId, ProjectScope, TableId, Uuid};
use serde_json::json;

use crate::identity::{As, call, platform_with};
use crate::store::stored_row;
use crate::world::{NOW, World, span, trace};
use crate::{api, api_writing, get, path, post, run};

/// A writer that lands every fact in the world's store at once, as a shipped `FactBatch` would.
#[derive(Debug)]
pub(crate) struct FakeWriter {
    store: Arc<crate::store::FakeStore>,
    next: AtomicU64,
    /// The refusal of the next write, when set.
    pub(crate) refuse: Mutex<Option<FactRefusal>>,
    /// Every fact written, in order.
    pub(crate) written: Mutex<Vec<NewFact>>,
}

impl FakeWriter {
    pub(crate) fn new(world: &World) -> Arc<Self> {
        Arc::new(Self {
            store: Arc::clone(&world.store),
            next: AtomicU64::new(1),
            refuse: Mutex::new(None),
            written: Mutex::new(Vec::new()),
        })
    }
}

fn row(
    scope: ProjectScope,
    author: FactAuthor,
    fact: &NewFact,
    accepted: AcceptedFact,
) -> (TableId, Vec<Value>) {
    let table = fact.body.table();
    let text = |v: &str| Value::String(v.to_owned());
    let mut set = vec![
        ("project_id", Value::Uuid(scope.project_id().uuid())),
        ("key", text(&fact.key)),
        ("target_day", Value::Date(fact.target_time.date())),
        ("target_time", Value::Time(fact.target_time)),
        ("trace_id", Value::Bytes(fact.trace_id.to_bytes().to_vec())),
        ("span_id", Value::Bytes(fact.span_id.to_bytes().to_vec())),
        ("session_id", text(&fact.session_id)),
        ("version", Value::UInt(accepted.version)),
        ("fact_id", Value::Uuid(accepted.fact_id.uuid())),
        ("target_kind", Value::Enum(fact.target_kind.value())),
        ("retracted", Value::UInt(u64::from(fact.retracted))),
        ("author_kind", Value::Enum(author.kind.value())),
        ("author_id", Value::Uuid(author.id)),
        ("created_at", Value::Time(accepted.created_at)),
    ];
    match &fact.body {
        FactBody::Score(s) => set.extend([
            ("data_type", Value::Enum(s.data_type.value())),
            ("value_num", Value::Float(s.value_num)),
            ("value_str", text(&s.value_str)),
            ("comment", text(&s.comment)),
            ("source", Value::Enum(s.source.value())),
            ("metadata", text(std::str::from_utf8(&s.metadata).unwrap())),
        ]),
        FactBody::Tag(t) => set.push(("op", Value::Enum(t.op.value()))),
        FactBody::Flag(f) => set.push(("value", Value::UInt(u64::from(f.value)))),
        FactBody::Correction(c) => set.extend([
            (
                "corrected",
                text(std::str::from_utf8(&c.corrected).unwrap()),
            ),
            ("note", text(&c.note)),
        ]),
    }
    (table, stored_row(table, &set))
}

impl FactWriter for FakeWriter {
    fn append(
        &self,
        scope: ProjectScope,
        author: FactAuthor,
        facts: Vec<NewFact>,
    ) -> FactAppend<'_> {
        Box::pin(async move {
            if let Some(refusal) = self.refuse.lock().unwrap().take() {
                return Err(refusal);
            }
            let mut accepted = Vec::new();
            for fact in facts {
                let n = self.next.fetch_add(1, Ordering::Relaxed);
                let assigned = AcceptedFact {
                    fact_id: FactId::from_uuid(Uuid::from_v7_parts(
                        1_794_907_800_000 + n,
                        u128::from(n),
                    )),
                    version: 1_794_907_800_000_000 + n,
                    created_at: NOW,
                };
                let (table, stored) = row(scope, author, &fact, assigned);
                self.store.insert(table, stored);
                self.written.lock().unwrap().push(fact);
                accepted.push(assigned);
            }
            Ok(accepted)
        })
    }
}

fn ago(seconds: i64) -> crucible_records::Timestamp {
    NOW.saturating_add_micros(-seconds * 1_000_000)
}

/// Trace 1 with root 10 and child 11, in the store.
fn one_trace() -> World {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, ago(60)), span(1, 11, 10, ago(59))]);
    world.settle();
    world
}

#[test]
fn facts_are_written_and_read_back() {
    run(|env| async move {
        let world = one_trace();
        let api = api(&env, &world);
        let t = trace(1).to_string();
        let tag = post(
            &api,
            &path(1, "/tags"),
            &json!({"trace_id": t, "tag": "urgent", "op": "add"}).to_string(),
            &[],
        )
        .await;
        assert_eq!(tag.status, StatusCode::CREATED, "{:#}", tag.body);
        assert_eq!(tag.body["key"], "urgent");
        assert_eq!(tag.body["target_time"], ago(60).to_string());
        let flag = json!({"trace_id": t, "flag": "bookmarked", "value": true}).to_string();
        assert_eq!(
            post(&api, &path(1, "/flags"), &flag, &[]).await.status,
            StatusCode::CREATED
        );
        let feedback = json!({
            "trace_id": t, "name": "helpful", "score_id": "u-7", "value": true,
            "comment": "thanks", "metadata": {"page": "chat"}
        });
        let written = post(&api, &path(1, "/feedback"), &feedback.to_string(), &[]).await;
        assert_eq!(written.status, StatusCode::CREATED, "{:#}", written.body);
        assert_eq!(written.body["key"], "helpful#u-7");
        let span_score =
            json!({"trace_id": t, "span_id": "000000000000000b", "name": "accuracy", "value": 0.5});
        assert_eq!(
            post(&api, &path(1, "/feedback"), &span_score.to_string(), &[])
                .await
                .status,
            StatusCode::CREATED
        );
        let correction = json!({"trace_id": t, "span_id": "000000000000000b", "corrected": {"answer": 42}, "note": "off by one"});
        assert_eq!(
            post(&api, &path(1, "/corrections"), &correction.to_string(), &[])
                .await
                .status,
            StatusCode::CREATED
        );

        let facts = get(&api, &path(1, &format!("/facts?trace_id={t}"))).await;
        assert_eq!(facts.status, StatusCode::OK, "{:#}", facts.body);
        insta::assert_snapshot!("trace_facts", facts.pretty());
        let span_facts = get(
            &api,
            &path(1, &format!("/facts?trace_id={t}&span_id=000000000000000b")),
        )
        .await;
        assert_eq!(span_facts.body["scores"][0]["value"], 0.5);
        assert_eq!(span_facts.body["correction"]["corrected"]["answer"], 42);
        assert_eq!(span_facts.body["correction"]["note"], "off by one");

        // Trace detail and the list carry the trace's facts.
        let detail = get(&api, &path(1, &format!("/traces/{t}"))).await;
        assert_eq!(detail.body["facts"]["tags"], json!(["urgent"]));
        assert_eq!(detail.body["facts"]["bookmarked"], true);
        assert_eq!(detail.body["facts"]["scores"][0]["comment"], "thanks");
        let list_uri = format!(
            "{}?from={}&to={}",
            path(1, "/traces"),
            ago(3_600),
            NOW.saturating_add_micros(1)
        );
        let list = get(&api, &list_uri).await;
        assert_eq!(list.body["data"][0]["tags"], json!(["urgent"]));
        assert_eq!(list.body["data"][0]["facts"]["scores"][0]["comment"], "");
        assert_eq!(list.body["data"][0]["facts"]["scores"][0]["value"], true);

        // Removing the tag wins over adding it.
        let remove = json!({"trace_id": t, "tag": "urgent", "op": "remove"}).to_string();
        assert_eq!(
            post(&api, &path(1, "/tags"), &remove, &[]).await.status,
            StatusCode::CREATED
        );
        let detail = get(&api, &path(1, &format!("/traces/{t}"))).await;
        assert_eq!(detail.body["facts"]["tags"], json!([]));
    });
}

/// A project's public key as Bearer writes feedback and nothing else (contracts 11.3 and 12.2). The feedback route
/// answers cross-origin requests without credentials and takes at most 16 KiB from a public key; every other route
/// refuses the public key with 401 and says which key to send, and another project's feedback route does not see
/// it.
#[test]
fn a_public_key_writes_feedback_and_nothing_else() {
    run(|env| async move {
        let world = one_trace();
        let p = platform_with(&env, &world, false).await;
        let t = trace(1).to_string();
        let feedback = json!({"trace_id": t, "name": "helpful", "value": true});
        let site = [("origin", "https://shop.example")];
        let written = call(
            &p.router,
            Method::POST,
            &path(1, "/feedback"),
            &p.public,
            Some(feedback.clone()),
            &site,
        )
        .await;
        assert_eq!(written.status, StatusCode::CREATED, "{:#}", written.body);
        assert_eq!(written.body["key"], "helpful");
        assert_eq!(written.header("access-control-allow-origin"), Some("*"));
        assert_eq!(written.header("access-control-allow-credentials"), None);

        // The browser's preflight for that request.
        let preflight = call(
            &p.router,
            Method::OPTIONS,
            &path(1, "/feedback"),
            &As::Nobody,
            None,
            &[
                ("origin", "https://shop.example"),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "authorization, content-type",
                ),
            ],
        )
        .await;
        assert_eq!(preflight.status, StatusCode::NO_CONTENT);
        assert_eq!(preflight.header("access-control-allow-origin"), Some("*"));
        assert_eq!(
            preflight.header("access-control-allow-methods"),
            Some("POST")
        );
        let allowed = preflight.header("access-control-allow-headers").unwrap();
        assert!(
            allowed.contains("authorization") && allowed.contains("content-type"),
            "{allowed}"
        );

        // A public-key body above 16 KiB is refused before it is read as JSON.
        let large =
            json!({"trace_id": t, "name": "helpful", "value": true, "comment": "x".repeat(16_400)});
        call(
            &p.router,
            Method::POST,
            &path(1, "/feedback"),
            &p.public,
            Some(large),
            &site,
        )
        .await
        .assert_problem(413, "payload_too_large");

        // Another project's route does not see the key; every other route refuses it.
        call(
            &p.router,
            Method::POST,
            &path(2, "/feedback"),
            &p.public,
            Some(feedback),
            &[],
        )
        .await
        .assert_problem(404, "not_found");
        let window = format!("?from={}&to={}", ago(3_600), NOW.saturating_add_micros(1));
        let tag = json!({"trace_id": t, "tag": "urgent", "op": "add"});
        for (method, rest, body) in [
            (Method::POST, "/tags".to_owned(), Some(tag)),
            (Method::GET, format!("/traces{window}"), None),
            (Method::GET, format!("/facts?trace_id={t}"), None),
        ] {
            let refused = call(&p.router, method, &path(1, &rest), &p.public, body, &[]).await;
            refused.assert_problem(401, "unauthenticated");
            assert!(
                refused.body["detail"]
                    .as_str()
                    .unwrap()
                    .contains("secret key"),
                "{rest}: {:#}",
                refused.body
            );
        }
    });
}

#[test]
fn an_idempotency_key_replays_the_first_answer() {
    run(|env| async move {
        let world = one_trace();
        let writer = FakeWriter::new(&world);
        let api = api_writing(&env, &world, Arc::clone(&writer));
        let body = json!({"trace_id": trace(1).to_string(), "tag": "a", "op": "add"}).to_string();
        let key = [("idempotency-key", "retry-1")];
        let first = post(&api, &path(1, "/tags"), &body, &key).await;
        let again = post(&api, &path(1, "/tags"), &body, &key).await;
        assert_eq!(first.status, StatusCode::CREATED);
        assert_eq!(
            (again.status, &again.body),
            (StatusCode::CREATED, &first.body)
        );
        assert_eq!(writer.written.lock().unwrap().len(), 1);
        let other = json!({"trace_id": trace(1).to_string(), "tag": "b", "op": "add"}).to_string();
        post(&api, &path(1, "/tags"), &other, &key)
            .await
            .assert_problem(409, "idempotency_key_reused");
        // Another route keeps its own keys; a malformed key is refused.
        let flag =
            json!({"trace_id": trace(1).to_string(), "flag": "public", "value": false}).to_string();
        assert_eq!(
            post(&api, &path(1, "/flags"), &flag, &key).await.status,
            StatusCode::CREATED
        );
        let bad = post(
            &api,
            &path(1, "/flags"),
            &flag,
            &[("idempotency-key", "a b")],
        )
        .await;
        bad.assert_problem(422, "validation_failed");
        // A refused write frees its key, so the retry runs.
        *writer.refuse.lock().unwrap() = Some(FactRefusal::Unavailable { retry_after_s: 7 });
        let fresh = [("idempotency-key", "retry-2")];
        let refused = post(&api, &path(1, "/tags"), &other, &fresh).await;
        refused.assert_problem(503, "ingest_unavailable");
        assert_eq!(refused.header("retry-after"), Some("7"));
        assert_eq!(refused.body["retry_after_s"], 7);
        assert_eq!(
            post(&api, &path(1, "/tags"), &other, &fresh).await.status,
            StatusCode::CREATED
        );
        *writer.refuse.lock().unwrap() = Some(FactRefusal::Throttled { retry_after_s: 2 });
        let throttled = post(&api, &path(1, "/tags"), &body, &[]).await;
        throttled.assert_problem(429, "rate_limited");
        assert_eq!(throttled.header("retry-after"), Some("2"));
    });
}

#[test]
fn malformed_writes_are_problems_with_pointers() {
    run(|env| async move {
        let world = one_trace();
        let api = api(&env, &world);
        let t = trace(1).to_string();
        let cases = [
            (
                "/tags",
                json!({"tag": "a", "op": "add"}),
                "/trace_id",
                "required",
            ),
            (
                "/tags",
                json!({"trace_id": t, "session_id": "s", "tag": "a", "op": "add"}),
                "/session_id",
                "conflict",
            ),
            (
                "/tags",
                json!({"trace_id": "xyz", "tag": "a", "op": "add"}),
                "/trace_id",
                "invalid_id",
            ),
            (
                "/tags",
                json!({"trace_id": t, "tag": "", "op": "add"}),
                "/tag",
                "invalid",
            ),
            (
                "/tags",
                json!({"trace_id": t, "tag": "a", "op": "toggle"}),
                "/op",
                "invalid",
            ),
            (
                "/tags",
                json!({"trace_id": t, "tag": "a", "op": "add", "extra": 1}),
                "/extra",
                "unknown_field",
            ),
            (
                "/flags",
                json!({"trace_id": t, "value": true}),
                "/flag",
                "required",
            ),
            (
                "/feedback",
                json!({"trace_id": t, "name": "a#b", "value": 1}),
                "/name",
                "invalid",
            ),
            (
                "/feedback",
                json!({"trace_id": t, "name": "a", "value": "x", "data_type": "numeric"}),
                "/data_type",
                "mismatch",
            ),
            (
                "/feedback",
                json!({"trace_id": t, "name": "a", "value": 1, "metadata": [1]}),
                "/metadata",
                "invalid",
            ),
        ];
        for (route, body, pointer, code) in cases {
            let reply = post(&api, &path(1, route), &body.to_string(), &[]).await;
            reply.assert_problem(422, "validation_failed");
            assert_eq!(reply.field_errors()[0], (pointer, code), "{route} {body}");
        }
        let invalid_json = post(&api, &path(1, "/tags"), "{", &[]).await;
        assert_eq!(invalid_json.field_errors()[0], ("", "invalid_json"));
        // A body that is not JSON, and a target the project has not sent.
        let request = axum::http::Request::post(path(1, "/tags"))
            .header("authorization", format!("Bearer {}", crate::key()))
            .body(axum::body::Body::from("tag=a"))
            .unwrap();
        let response = tower::ServiceExt::oneshot(api.clone(), request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let unknown =
            json!({"trace_id": trace(9).to_string(), "tag": "a", "op": "add"}).to_string();
        let reply = post(&api, &path(1, "/tags"), &unknown, &[]).await;
        reply.assert_problem(404, "not_found");
        let facts = get(&api, &path(1, &format!("/facts?trace_id={}", trace(9)))).await;
        facts.assert_problem(404, "not_found");
        let facts = get(&api, &path(1, "/facts")).await;
        assert_eq!(facts.field_errors(), [("/trace_id", "required")]);
    });
}
