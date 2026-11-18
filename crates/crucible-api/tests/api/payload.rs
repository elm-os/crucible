//! `GET projects/{project_id}/spans/{trace_id}/{span_id}/payload`: raw bytes with their length, tag and caching,
//! byte ranges, downloads, and the problems of a request that names no payload (contract 1.3, 8.7 and 12.6).

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, RANGE};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use crucible_recent::SpanEntry;
use crucible_records::TableId;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use crate::world::{NOW, World, span, span_id, span_payload_row, trace};
use crate::{api, api_without_store, get, key, path};

const INPUT: &str = "[{\"role\":\"user\",\"content\":\"h\u{e9}llo <script>\"}]";

/// One span of trace 1 whose stored input is [`INPUT`].
fn stored() -> (World, SpanEntry) {
    let mut world = World::new(100);
    let entry = span(1, 10, 0, NOW.saturating_add_micros(-1_500));
    let lsn = world.ack(vec![entry.clone()]);
    let row = span_payload_row(&entry, lsn, NOW, INPUT, &[]);
    world.store.insert(TableId::SpanPayload, row);
    world.settle();
    (world, entry)
}

fn uri(entry: &SpanEntry, query: &str) -> String {
    let s = &entry.summary;
    let rest = format!("/spans/{}/{}/payload", s.trace_id, s.span_id);
    format!("{}?start_time={}&{query}", path(1, &rest), s.start_time)
}

/// A request with project 1's key and an optional `Range`, answered with its raw body.
async fn fetch(
    router: &Router,
    method: Method,
    uri: &str,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {}", key()));
    if let Some(range) = range {
        request = request.header(RANGE, range);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status, parts.headers, bytes)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).map(|value| value.to_str().unwrap())
}

#[test]
fn a_field_comes_whole_or_by_range_as_text_that_is_never_rendered() {
    crate::run(|env| async move {
        let (world, entry) = stored();
        let api = api(&env, &world);
        let total = INPUT.len();
        let (status, headers, body) =
            fetch(&api, Method::GET, &uri(&entry, "field=input"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, INPUT.as_bytes());
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(
            header(&headers, "content-length"),
            Some(total.to_string().as_str())
        );
        assert_eq!(header(&headers, "accept-ranges"), Some("bytes"));
        assert_eq!(header(&headers, "x-content-type-options"), Some("nosniff"));
        assert_eq!(
            header(&headers, "content-security-policy"),
            Some("default-src 'none'; frame-ancestors 'none'")
        );
        assert_eq!(
            header(&headers, "cache-control"),
            Some("private, max-age=31536000, immutable")
        );
        assert_eq!(header(&headers, "content-disposition"), None);
        let etag = header(&headers, "etag").unwrap().to_owned();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

        // A range, which may cut a character, with the same tag.
        let at = INPUT.find('\u{e9}').unwrap();
        let range = format!("bytes={}-{}", at - 2, at);
        let (status, headers, body) =
            fetch(&api, Method::GET, &uri(&entry, "field=input"), Some(&range)).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, &INPUT.as_bytes()[at - 2..=at]);
        let expected = format!("bytes {}-{}/{total}", at - 2, at);
        assert_eq!(header(&headers, "content-range"), Some(expected.as_str()));
        assert_eq!(header(&headers, "etag"), Some(etag.as_str()));
        // To the end, and past it.
        let tail = format!("bytes={}-", total - 3);
        let (status, _, body) =
            fetch(&api, Method::GET, &uri(&entry, "field=input"), Some(&tail)).await;
        assert_eq!(
            (status, body.as_slice()),
            (StatusCode::PARTIAL_CONTENT, &INPUT.as_bytes()[total - 3..])
        );
        let past = format!("bytes={total}-");
        let (status, headers, _) =
            fetch(&api, Method::GET, &uri(&entry, "field=input"), Some(&past)).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        let expected = format!("bytes */{total}");
        assert_eq!(header(&headers, "content-range"), Some(expected.as_str()));
        // A form the server does not serve partly is answered whole.
        let (status, _, body) = fetch(
            &api,
            Method::GET,
            &uri(&entry, "field=input"),
            Some("bytes=-5"),
        )
        .await;
        assert_eq!((status, body.len()), (StatusCode::OK, total));
        // An empty field is sent whole, ranged or not.
        let (status, headers, body) = fetch(
            &api,
            Method::GET,
            &uri(&entry, "field=output"),
            Some("bytes=0-1023"),
        )
        .await;
        assert_eq!((status, body.len()), (StatusCode::OK, 0));
        assert_eq!(header(&headers, "content-length"), Some("0"));
        // A download, and a HEAD without a body.
        let (_, headers, _) = fetch(
            &api,
            Method::GET,
            &uri(&entry, "field=input&download=1"),
            None,
        )
        .await;
        let expected = format!(
            "attachment; filename=\"{}-input.txt\"",
            entry.summary.span_id
        );
        assert_eq!(
            header(&headers, "content-disposition"),
            Some(expected.as_str())
        );
        let (status, headers, body) =
            fetch(&api, Method::HEAD, &uri(&entry, "field=input"), None).await;
        assert_eq!((status, body.len()), (StatusCode::OK, 0));
        assert_eq!(
            header(&headers, "content-length"),
            Some(total.to_string().as_str())
        );
    });
}

#[test]
fn requests_that_name_no_payload_are_refused_with_what_to_change() {
    crate::run(|env| async move {
        let (world, entry) = stored();
        let api = api(&env, &world);
        let s = &entry.summary;
        let base = path(1, &format!("/spans/{}/{}/payload", s.trace_id, s.span_id));
        let pointers = |reply: crate::Reply| -> Vec<(String, String)> {
            reply.assert_problem(422, "validation_failed");
            reply
                .field_errors()
                .into_iter()
                .map(|(pointer, code)| (pointer.to_owned(), code.to_owned()))
                .collect()
        };
        let pair = |pointer: &str, code: &str| (pointer.to_owned(), code.to_owned());
        assert_eq!(
            pointers(get(&api, &base).await),
            [pair("/field", "required"), pair("/start_time", "required")]
        );
        assert_eq!(
            pointers(get(&api, &uri(&entry, "field=metadata&download=yes")).await),
            [pair("/field", "invalid"), pair("/download", "invalid")]
        );
        let bad_ids = path(
            1,
            &format!(
                "/spans/{}/xyz/payload?field=input&start_time={}",
                trace(1),
                NOW
            ),
        );
        assert_eq!(
            pointers(get(&api, &bad_ids).await),
            [pair("/span_id", "invalid_id")]
        );
        assert_eq!(
            pointers(get(&api, &uri(&entry, "field=input&extra=1")).await),
            [pair("/extra", "unknown_parameter")]
        );
        // No such span, and a start time that names no payload row.
        let other = path(
            1,
            &format!(
                "/spans/{}/{}/payload?field=input&start_time={}",
                s.trace_id,
                span_id(99),
                s.start_time
            ),
        );
        get(&api, &other).await.assert_problem(404, "not_found");
        let moved = path(
            1,
            &format!(
                "/spans/{}/{}/payload?field=input&start_time={}",
                s.trace_id,
                s.span_id,
                s.start_time.saturating_add_micros(1)
            ),
        );
        get(&api, &moved).await.assert_problem(404, "not_found");
        // Checked first, then refused without a store.
        let bare = api_without_store(&env);
        get(&bare, &base)
            .await
            .assert_problem(422, "validation_failed");
        get(&bare, &uri(&entry, "field=input"))
            .await
            .assert_problem(503, "telemetry_store_unavailable");
    });
}
