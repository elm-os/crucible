//! Media (contracts 12.6 and 14): an upload is asked for, uploaded to the presigned URL, confirmed and read
//! back with a presigned download; the same bytes again need no upload; uses are listed by trace and span;
//! every refusal names what to change; an upload that differs from what was declared is refused and deleted;
//! and without an object store every media route answers `media_not_supported`.

use axum::http::Method;
use bytes::Bytes;
use crucible_env::Disposition;
use crucible_media::rules::object_key;
use crucible_records::MediaObjectId;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::identity::{PASSWORD, call, platform_with, session};
use crate::world::{World, project};
use crate::{api, get, post, run, send};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really an image";
const TRACE: &str = "0102030405060708090a0b0c0d0e0f10";
const SPAN: &str = "1112131415161718";

fn media_path(rest: &str) -> String {
    format!("/api/v1/projects/{}/media{rest}", project(1))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn create(bytes: &[u8], content_type: &str, trace: &str) -> Value {
    json!({
        "content_type": content_type,
        "len": bytes.len(),
        "sha256": hex(&Sha256::digest(bytes)),
        "trace_id": trace,
        "span_id": SPAN,
        "field": "input",
    })
}

#[test]
fn an_upload_is_asked_for_uploaded_confirmed_and_read_back() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;

        let created = call(
            &p.router,
            Method::POST,
            &media_path(""),
            &p.key,
            Some(create(PNG, "image/png", TRACE)),
            &[("idempotency-key", "first")],
        )
        .await;
        assert_eq!(created.status.as_u16(), 201, "{}", created.pretty());
        let id = created.body["media_id"].as_str().unwrap().to_owned();
        let upload = &created.body["upload"];
        let url = upload["url"].as_str().unwrap().to_owned();
        let headers: Vec<(String, String)> = upload["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["name"].as_str().unwrap().to_owned(),
                    h["value"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert!(headers.contains(&("content-type".to_owned(), "image/png".to_owned())));
        assert!(headers.contains(&("content-length".to_owned(), PNG.len().to_string())));
        assert!(upload["expires_at"].as_str().is_some());

        // A replay under the key answers what the first request did.
        let replay = call(
            &p.router,
            Method::POST,
            &media_path(""),
            &p.key,
            Some(create(PNG, "image/png", TRACE)),
            &[("idempotency-key", "first")],
        )
        .await;
        assert_eq!(replay.body, created.body);

        // Pending: no download, and nothing to confirm yet.
        let pending = call(
            &p.router,
            Method::GET,
            &media_path(&format!("/{id}")),
            &p.key,
            None,
            &[],
        )
        .await;
        assert_eq!(pending.body["state"], "pending", "{}", pending.pretty());
        assert!(pending.body.get("url").is_none());
        let early = call(
            &p.router,
            Method::PATCH,
            &media_path(&format!("/{id}")),
            &p.key,
            None,
            &[],
        )
        .await;
        early.assert_problem(409, "conflict");

        // The client uploads with the signed headers, then confirms.
        p.objects
            .upload(&url, "image/png", Bytes::from_static(PNG))
            .unwrap();
        let confirmed = call(
            &p.router,
            Method::PATCH,
            &media_path(&format!("/{id}")),
            &p.key,
            None,
            &[],
        )
        .await;
        assert_eq!(confirmed.status.as_u16(), 200, "{}", confirmed.pretty());
        assert_eq!(confirmed.body["state"], "uploaded");
        assert_eq!(confirmed.body["disposition"], "inline");
        assert_eq!(confirmed.body["sha256"], hex(&Sha256::digest(PNG)));

        // A reader gets a presigned download that serves the bytes inline.
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let read = call(
            &p.router,
            Method::GET,
            &media_path(&format!("/{id}")),
            &viewer,
            None,
            &[],
        )
        .await;
        assert_eq!(read.status.as_u16(), 200, "{}", read.pretty());
        let download = read.body["url"].as_str().unwrap();
        let (stored, disposition) = p.objects.download(download).unwrap();
        assert_eq!(stored.bytes.as_ref(), PNG);
        assert_eq!(disposition, Disposition::Inline);
        assert!(read.body["url_expires_at"].as_str().is_some());

        // The same bytes in another trace: the same media, nothing to upload, one more use.
        let other_trace = "2122232425262728292a2b2c2d2e2f30";
        let again = call(
            &p.router,
            Method::POST,
            &media_path(""),
            &p.key,
            Some(create(PNG, "image/png", other_trace)),
            &[],
        )
        .await;
        assert_eq!(again.status.as_u16(), 201);
        assert_eq!(again.body["media_id"], id.as_str());
        assert!(again.body.get("upload").is_none(), "{}", again.pretty());

        // Uses, by span and by trace.
        let of_span = call(
            &p.router,
            Method::GET,
            &media_path(&format!("?trace_id={TRACE}&span_id={SPAN}")),
            &viewer,
            None,
            &[],
        )
        .await;
        assert_eq!(of_span.status.as_u16(), 200, "{}", of_span.pretty());
        let uses = of_span.body["data"].as_array().unwrap();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0]["field"], "input");
        assert_eq!(uses[0]["span_id"], SPAN);
        assert_eq!(uses[0]["media"]["id"], id.as_str());
        assert!(uses[0]["media"].get("url").is_none());
        let of_trace = call(
            &p.router,
            Method::GET,
            &media_path(&format!("?trace_id={other_trace}")),
            &p.key,
            None,
            &[],
        )
        .await;
        assert_eq!(of_trace.body["data"].as_array().unwrap().len(), 1);
    });
}

#[test]
fn every_refusal_names_what_to_change() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let post = |body: Value| {
            let (router, key) = (p.router.clone(), &p.key);
            async move { call(&router, Method::POST, &media_path(""), key, Some(body), &[]).await }
        };

        let big = vec![0u8; (1 << 20) + 1];
        post(create(&big, "image/png", TRACE))
            .await
            .assert_problem(413, "media_too_large");
        let refused = post(create(PNG, "application/x-msdownload", TRACE)).await;
        refused.assert_problem(422, "validation_failed");
        assert_eq!(refused.field_errors(), [("/content_type", "not_allowed")]);
        let refused = post(json!({
            "content_type": "image/png",
            "len": 3,
            "sha256": "ABC",
            "trace_id": "0".repeat(32),
            "span_id": "xyz",
            "field": "body",
        }))
        .await;
        refused.assert_problem(422, "validation_failed");
        assert_eq!(
            refused.field_errors(),
            [
                ("/sha256", "invalid_hash"),
                ("/trace_id", "invalid_id"),
                ("/span_id", "invalid_id"),
                ("/field", "invalid")
            ]
        );
        let unknown = post(json!({ "content_type": "image/png", "colour": "red" })).await;
        unknown.assert_problem(422, "validation_failed");

        // A signed-in user reads media but never uploads: uploads are a key's `ingest` scope.
        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let as_viewer = call(
            &p.router,
            Method::POST,
            &media_path(""),
            &viewer,
            Some(create(PNG, "image/png", TRACE)),
            &[],
        )
        .await;
        as_viewer.assert_problem(403, "forbidden");

        // Ids that name nothing.
        let missing = MediaObjectId::from_bytes([7; 16]);
        call(
            &p.router,
            Method::GET,
            &media_path(&format!("/{missing}")),
            &p.key,
            None,
            &[],
        )
        .await
        .assert_problem(404, "not_found");
        call(
            &p.router,
            Method::GET,
            &media_path("/not-an-id"),
            &p.key,
            None,
            &[],
        )
        .await
        .assert_problem(404, "not_found");
        call(
            &p.router,
            Method::GET,
            &media_path("?trace_id=xyz"),
            &p.key,
            None,
            &[],
        )
        .await
        .assert_problem(422, "validation_failed");
    });
}

#[test]
fn an_upload_that_differs_from_its_declaration_is_refused_and_deleted() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let created = call(
            &p.router,
            Method::POST,
            &media_path(""),
            &p.key,
            Some(create(PNG, "image/png", TRACE)),
            &[],
        )
        .await;
        let id = created.body["media_id"].as_str().unwrap().to_owned();
        // An object of another length lands under the key, as a store that checks nothing would let it.
        let key = object_key("media", project(1), MediaObjectId::parse(&id).unwrap());
        p.objects
            .plant(&key, Bytes::from_static(b"short"), env.clock.wall_now());
        let refused = call(
            &p.router,
            Method::PATCH,
            &media_path(&format!("/{id}")),
            &p.key,
            None,
            &[],
        )
        .await;
        refused.assert_problem(422, "media_hash_mismatch");
        assert!(!p.objects.objects().contains_key(&key));
        let pending = call(
            &p.router,
            Method::GET,
            &media_path(&format!("/{id}")),
            &p.key,
            None,
            &[],
        )
        .await;
        assert_eq!(pending.body["state"], "pending");
    });
}

#[test]
fn without_an_object_store_every_media_route_answers_media_not_supported() {
    run(|env| async move {
        let world = World::new(10);
        let router = api(&env, &world);
        let id = MediaObjectId::from_bytes([7; 16]);
        let body = serde_json::to_string(&create(PNG, "image/png", TRACE)).unwrap();
        let created = post(&router, &media_path(""), &body, &[]).await;
        created.assert_problem(501, "media_not_supported");
        let detail = created.body["detail"].as_str().unwrap();
        assert!(detail.contains("object_store.endpoint"), "{detail}");
        get(&router, &media_path(&format!("/{id}")))
            .await
            .assert_problem(501, "media_not_supported");
        get(&router, &media_path(&format!("?trace_id={TRACE}")))
            .await
            .assert_problem(501, "media_not_supported");
        send(
            &router,
            Method::PATCH,
            &media_path(&format!("/{id}")),
            Some(&crate::key()),
        )
        .await
        .assert_problem(501, "media_not_supported");
    });
}

/// How the test platform's media read references out of a payload: `[[media <sha256 hex> <content type>]]`, a
/// stand-in for the compat SDKs' tokens, whose reader is tested in `crucible-compat`. Each reference's pointer
/// is its position in the body.
pub(crate) fn test_refs(body: &[u8]) -> Vec<crucible_media::PayloadRef> {
    let text = String::from_utf8_lossy(body);
    text.split("[[media ")
        .skip(1)
        .filter_map(|rest| rest.split("]]").next())
        .enumerate()
        .filter_map(|(at, inside)| {
            let (sha, content_type) = inside.split_once(' ')?;
            let bytes: Vec<u8> = (0..sha.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(sha.get(i..i + 2)?, 16).ok())
                .collect::<Option<_>>()?;
            let hash = <[u8; 32]>::try_from(bytes).ok()?;
            Some(crucible_media::PayloadRef {
                pointer: format!("/{at}"),
                content_type: content_type.to_owned(),
                hashes: (hash, hash),
            })
        })
        .collect()
}

fn reference(bytes: &[u8], content_type: &str) -> String {
    format!("[[media {} {content_type}]]", hex(&Sha256::digest(bytes)))
}

/// The media a span references: those its input and metadata name, resolved to the project's media (or to none
/// when the upload never arrived), then those uploaded for the span that its payload does not name. Without an
/// object store the route answers `media_not_supported`, and a request that names no span is refused.
#[test]
fn a_span_lists_the_media_its_payload_references() {
    use crucible_records::TableId;

    use crate::store::stored_row;
    use crate::world::{NOW, span};

    run(|env| async move {
        const WAV: &[u8] = b"RIFF0000WAVEfmt not really audio";
        let mut world = World::new(10);
        let entry = span(1, 10, 0, NOW.saturating_add_micros(-1_500));
        let lsn = world.ack(vec![entry.clone()]);
        let s = entry.summary.clone();
        let input = format!(
            "[{{\"role\":\"user\",\"content\":\"{} and {}\"}}]",
            reference(PNG, "image/png"),
            reference(b"never uploaded", "application/pdf")
        );
        let text = |t: &str| crucible_plan::Value::String(t.into());
        world.store.insert(
            TableId::SpanPayload,
            stored_row(
                TableId::SpanPayload,
                &[
                    ("project_id", crucible_plan::Value::Uuid(entry.project_id.uuid())),
                    ("start_minute", crucible_plan::Value::Time(s.start_time.floor_minute())),
                    ("trace_hash", crucible_plan::Value::UInt(u64::from(s.trace_id.trace_hash()))),
                    ("span_id", crucible_plan::Value::Bytes(s.span_id.as_bytes().to_vec())),
                    ("trace_id", crucible_plan::Value::Bytes(s.trace_id.as_bytes().to_vec())),
                    ("start_time", crucible_plan::Value::Time(s.start_time)),
                    ("input", text(&input)),
                    ("output", text("{\"answer\":\"a bird\"}")),
                    ("metadata_keys", crucible_plan::Value::Array(vec![text("chart")])),
                    (
                        "metadata_values",
                        crucible_plan::Value::Array(vec![text(&reference(PNG, "image/png"))]),
                    ),
                    ("lsn", crucible_plan::Value::UInt(lsn.get())),
                    ("received_at", crucible_plan::Value::Time(NOW)),
                ],
            ),
        );
        world.settle();
        let p = platform_with(&env, &world, false).await;
        let (trace, span) = (s.trace_id.to_string(), s.span_id.to_string());

        // The PNG is uploaded for the span's input; the WAV for its output, which does not name it.
        let mut ids = Vec::new();
        for (bytes, content_type, field) in [(PNG, "image/png", "input"), (WAV, "audio/wav", "output")] {
            let mut body = create(bytes, content_type, &trace);
            body["span_id"] = Value::from(span.as_str());
            body["field"] = Value::from(field);
            let created = call(&p.router, Method::POST, &media_path(""), &p.key, Some(body), &[]).await;
            let id = created.body["media_id"].as_str().unwrap().to_owned();
            let url = created.body["upload"]["url"].as_str().unwrap();
            p.objects
                .upload(url, content_type, Bytes::copy_from_slice(bytes))
                .unwrap();
            let confirmed = call(
                &p.router,
                Method::PATCH,
                &media_path(&format!("/{id}")),
                &p.key,
                None,
                &[],
            )
            .await;
            assert_eq!(confirmed.status.as_u16(), 200, "{}", confirmed.pretty());
            ids.push(id);
        }

        let route = format!(
            "/api/v1/projects/{}/spans/{trace}/{span}/media?start_time={}",
            project(1),
            s.start_time
        );
        let listed = call(&p.router, Method::GET, &route, &p.key, None, &[]).await;
        assert_eq!(listed.status.as_u16(), 200, "{}", listed.pretty());
        let data = listed.body["data"].as_array().unwrap();
        let seen: Vec<(&str, Option<&str>, &str, Option<&str>)> = data
            .iter()
            .map(|r| {
                (
                    r["field"].as_str().unwrap(),
                    r["pointer"].as_str(),
                    r["content_type"].as_str().unwrap(),
                    r["media"]["id"].as_str(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("input", Some("/0"), "image/png", Some(ids[0].as_str())),
                ("input", Some("/1"), "application/pdf", None),
                ("metadata", Some("/0"), "image/png", Some(ids[0].as_str())),
                ("output", None, "audio/wav", Some(ids[1].as_str())),
            ]
        );
        assert_eq!(data[0]["media"]["state"], "uploaded");
        assert!(data[0]["media"].get("url").is_none(), "links come from GET media/{{id}}");

        let viewer = session(&p.identity.login("vic@example.com", PASSWORD).await.unwrap());
        let as_viewer = call(&p.router, Method::GET, &route, &viewer, None, &[]).await;
        assert_eq!(as_viewer.body, listed.body);

        let unknown = route.replace(&span, "00000000000000ff");
        call(&p.router, Method::GET, &unknown, &p.key, None, &[])
            .await
            .assert_problem(404, "not_found");
        let bad = route.replace(&span, "xyz");
        call(&p.router, Method::GET, &bad, &p.key, None, &[])
            .await
            .assert_problem(422, "validation_failed");
        let no_time = route.split('?').next().unwrap().to_owned();
        call(&p.router, Method::GET, &no_time, &p.key, None, &[])
            .await
            .assert_problem(422, "validation_failed");

        // Without an object store there is no media to show, and the answer names the settings.
        let bare = get(&api(&env, &world), &route).await;
        bare.assert_problem(501, "media_not_supported");
        assert!(
            bare.body["detail"]
                .as_str()
                .unwrap()
                .contains("object_store.endpoint")
        );
    });
}
