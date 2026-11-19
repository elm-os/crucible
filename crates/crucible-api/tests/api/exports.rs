//! Exports (contracts 11.9, 12.5, 12.6 and 12.8): creating an export answers 202 with a queued export; an
//! `Idempotency-Key` replays it or refuses another request; the window and the trace ids are checked by JSON
//! pointer; a project gets 5 per hour; reading and listing; the download is refused until the export is ready,
//! streams the file with its checksum as the `ETag` when it is, and answers `export_expired` once a deletion
//! request expired it.

use axum::body::Body;
use axum::http::{Method, Request, header};
use crucible_control::{Queue, Repos as _};
use crucible_env::{DiskPath, OpenMode, Root};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt as _;

use crate::identity::{As, call, platform_with};
use crate::run;
use crate::world::{World, project};

fn exports_path() -> String {
    format!("/api/v1/projects/{}/exports", project(1))
}

fn request() -> Value {
    json!({
        "format": "crucible-jsonl",
        "from": "2026-11-17T09:00:00Z",
        "to": "2026-11-17T10:00:00Z",
    })
}

/// A snapshot-stable copy of an export: the parts that vary per run replaced.
fn stable(mut export: Value) -> Value {
    for (key, stand_in) in [("id", "<id>"), ("created_at", "<now>"), ("as_of", "<now>")] {
        if export.get(key).is_some() {
            export[key] = Value::from(stand_in);
        }
    }
    if export.get("download_path").is_some() {
        export["download_path"] = Value::from("<download path>");
    }
    export
}

#[test]
fn an_export_is_created_once_per_key_and_read_back() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let key = |k: &'static str| [("idempotency-key", k)];

        let created = call(&p.router, Method::POST, &exports_path(), &p.key, Some(request()), &key("first")).await;
        assert_eq!(created.status.as_u16(), 202, "{}", created.pretty());
        insta::assert_snapshot!("exports_created", serde_json::to_string_pretty(&stable(created.body.clone())).unwrap());
        let id = created.body["id"].as_str().unwrap().to_owned();

        // The same request under the key answers with the same export; another request under it is refused.
        let again = call(&p.router, Method::POST, &exports_path(), &p.key, Some(request()), &key("first")).await;
        assert_eq!(again.status.as_u16(), 202);
        assert_eq!(again.body["id"], id.as_str());
        let mut other = request();
        other["format"] = Value::from("otlp-json");
        let reused = call(&p.router, Method::POST, &exports_path(), &p.key, Some(other), &key("first")).await;
        reused.assert_problem(409, "idempotency_key_reused");

        let read = call(&p.router, Method::GET, &format!("{}/{id}", exports_path()), &p.key, None, &[]).await;
        assert_eq!(read.status.as_u16(), 200);
        assert_eq!(read.body["state"], "queued");
        let listed = call(&p.router, Method::GET, &exports_path(), &p.owner, None, &[]).await;
        assert_eq!(listed.body["data"].as_array().unwrap().len(), 1);
        assert_eq!(listed.body["data"][0]["id"], id.as_str());

        let unknown = format!("{}/{}", exports_path(), "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b");
        call(&p.router, Method::GET, &unknown, &p.key, None, &[]).await.assert_problem(404, "not_found");
        let early = call(&p.router, Method::GET, &format!("{}/{id}/download", exports_path()), &p.key, None, &[]).await;
        early.assert_problem(409, "conflict");
    });
}

#[test]
fn a_request_is_checked_field_by_field() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let bad = json!({
            "format": "crucible-jsonl",
            "from": "yesterday",
            "to": "2026-11-17T10:00:00Z",
            "trace_ids": ["c3a1e2f4b5d6478990a1b2c3d4e5f601", "not-a-trace"],
        });
        let reply = call(&p.router, Method::POST, &exports_path(), &p.key, Some(bad), &[]).await;
        reply.assert_problem(422, "validation_failed");
        assert_eq!(
            reply.field_errors(),
            [("/from", "invalid_timestamp"), ("/trace_ids/1", "invalid_trace_id")]
        );
        let backwards = json!({"format": "otlp-json", "from": "2026-11-17T10:00:00Z", "to": "2026-11-17T09:00:00Z"});
        let reply = call(&p.router, Method::POST, &exports_path(), &p.key, Some(backwards), &[]).await;
        assert_eq!(reply.field_errors(), [("/to", "invalid_window")]);
        let unknown = json!({"format": "csv", "from": "2026-11-17T09:00:00Z", "to": "2026-11-17T10:00:00Z"});
        let reply = call(&p.router, Method::POST, &exports_path(), &p.key, Some(unknown), &[]).await;
        reply.assert_problem(422, "validation_failed");
        let bad_key = call(&p.router, Method::POST, &exports_path(), &p.key, Some(request()), &[("idempotency-key", "no spaces")]).await;
        bad_key.assert_problem(422, "validation_failed");
    });
}

#[test]
fn a_project_creates_five_exports_per_hour() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        for _ in 0..5 {
            let created = call(&p.router, Method::POST, &exports_path(), &p.key, Some(request()), &[]).await;
            assert_eq!(created.status.as_u16(), 202);
        }
        let limited = call(&p.router, Method::POST, &exports_path(), &p.owner, Some(request()), &[]).await;
        limited.assert_problem(429, "rate_limited");
        assert_eq!(limited.body["limit"], json!({"name": "exports_per_hour", "value": 5}));
        assert!(limited.body["retry_after_s"].as_u64().unwrap() > 3_500);
        assert!(limited.header("retry-after").is_some());
    });
}

#[test]
fn a_ready_export_downloads_with_its_checksum_until_a_deletion_expires_it() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let created = call(&p.router, Method::POST, &exports_path(), &p.key, Some(request()), &[]).await;
        let id = created.body["id"].as_str().unwrap().to_owned();

        // What the export job does: write the file of its attempt and record it under the claim.
        let content = b"{\"schema\":\"crucible.span/1\",\"span\":{},\"facts\":[]}\n".repeat(3);
        let file = format!("{}/{id}.1.jsonl", project(1));
        let dir = DiskPath::new(Root::Exports, &project(1).to_string()).unwrap();
        env.disk.create_dir_all(&dir).await.unwrap();
        let handle = env.disk.open(&DiskPath::new(Root::Exports, &file).unwrap(), OpenMode::CreateNew).await.unwrap();
        handle.append(0, vec![bytes::Bytes::from(content.clone())]).await.unwrap();
        handle.sync_data().await.unwrap();
        let claim = p.model.claims().claim(Queue::ExportJobs, 1, std::time::Duration::from_secs(60)).await.unwrap();
        let checksum: String = Sha256::digest(&content).iter().map(|b| format!("{b:02x}")).collect();
        let committed = p
            .model
            .export_jobs()
            .commit(
                &claim[0],
                &crucible_control::ExportCommit {
                    file,
                    checksum: checksum.clone(),
                    spans: 3,
                    bytes: content.len() as u64,
                    skipped: 0,
                    expires_at: env.clock.wall_now().saturating_add_micros(3_600_000_000),
                },
            )
            .await
            .unwrap();
        assert!(committed);

        let read = call(&p.router, Method::GET, &format!("{}/{id}", exports_path()), &p.key, None, &[]).await;
        insta::assert_snapshot!("exports_ready", serde_json::to_string_pretty(&stable(read.body.clone())).unwrap());

        let download = Request::builder()
            .method(Method::GET)
            .uri(format!("{}/{id}/download", exports_path()))
            .header(header::HOST, "crucible.test")
            .header(header::AUTHORIZATION, match &p.key { As::Key(secret) => format!("Bearer {secret}"), _ => unreachable!() })
            .body(Body::empty())
            .unwrap();
        let response = p.router.clone().oneshot(download).await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let headers = response.headers().clone();
        assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
        assert_eq!(headers[header::ETAG], format!("\"{checksum}\"").as_str());
        assert_eq!(headers[header::CONTENT_DISPOSITION], format!("attachment; filename=\"crucible-export-{id}.jsonl\"").as_str());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), content.as_slice());

        // A deletion request accepted now expires every export created before it.
        let now = env.clock.wall_now();
        crucible_export::purge(&p.model, env.disk.as_ref(), project(1), now.saturating_add_micros(1), now).await.unwrap();
        let gone = call(&p.router, Method::GET, &format!("{}/{id}/download", exports_path()), &p.key, None, &[]).await;
        gone.assert_problem(410, "export_expired");
        let read = call(&p.router, Method::GET, &format!("{}/{id}", exports_path()), &p.key, None, &[]).await;
        assert_eq!(read.body["state"], "expired");
    });
}
