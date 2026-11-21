//! The compat media endpoint (contracts 12.1, 12.6 and 14.3): the media upload API of dialect 1's SDKs, mapped
//! onto Crucible's flow. The binary serves it on both HTTP ingest listeners; this module holds its paths and
//! its wire shapes and stays pure.
//!
//! - `POST <collection>` with `traceId`, `observationId`, `contentType`, `contentLength`, `sha256Hash` (base64)
//!   and `field` asks for an upload, and answers `201` with `uploadUrl` (absent when the bytes are already
//!   stored) and `mediaId`.
//! - `PATCH <collection>/<mediaId>` reports the upload: `uploadedAt`, `uploadHttpStatus` and optionally
//!   `uploadHttpError` and `uploadTimeMs`. A successful status confirms it.
//! - `GET <collection>/<mediaId>` answers `mediaId`, `contentType`, `contentLength`, `uploadedAt`, `url` and
//!   `urlExpiry`.
//!
//! The SDKs write the media id into their payloads before the upload request answers, deriving it from the
//! SHA-256: the first 22 characters of its URL-safe base64 form. So this endpoint answers with that id, and
//! finds the object again by the hash prefix it encodes ([`hash_range`]); Crucible's own media id stays
//! internal to it.
//!
//! The client half of the wire shapes ([`upload_request`], [`report_request`], [`UploadReply`], [`MediaReply`]) is
//! what tests play the SDK with, so that no other crate spells the dialect's fields.
//!
//! [`payload_tokens`] reads the references the SDKs leave in a payload in place of their media, so that the span
//! panel can show the media a span references; [`payload_token`] writes one, for tests.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use crucible_records::{SpanId, TraceId};
use serde::{Deserialize, Serialize};

use crate::MediaToken;

/// The compat media endpoint of one dialect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MediaRoute {
    /// The dialect whose SDKs call it.
    pub dialect: u8,
    /// The collection path: `POST` here, `GET` and `PATCH` on `<collection>/<mediaId>`.
    pub collection: &'static str,
}

/// The compat media endpoints. Each lies under its dialect's ingest route prefix (contract 12.1).
pub const MEDIA_ROUTES: [MediaRoute; 1] = [MediaRoute {
    dialect: 1,
    collection: "/api/public/media",
}];

/// The length of a compat media id.
pub const MEDIA_ID_LEN: usize = 22;

/// Why the compat media endpoint refuses every call on a server without object storage, and how to keep the
/// media (contracts 1.7 and 14). The SDKs put the reference in the payload before they ask for the upload, so a
/// refused upload loses the media unless the SDK keeps them inline.
pub const NOT_SUPPORTED: &str = "this server has no object storage, so it stores no media, and the SDK has already \
    replaced each media in the payload with a reference: with uploads on, those media are lost. Set \
    LANGFUSE_MEDIA_UPLOAD_ENABLED=false where the SDK runs to keep media inline in the payload, under the 8 MiB \
    field cap, or turn media on in the server with object_store.endpoint, object_store.bucket, \
    object_store.access_key and object_store.secret_key (or their CRUCIBLE_OBJECT_STORE_* variables) and \
    postgres.url";

/// What a server without object storage says at startup: media stay inline, and the SDKs that upload media
/// separately keep them only when told to.
pub const NO_OBJECT_STORAGE: &str = "media stay inline in payloads: no object storage is configured \
    (object_store.endpoint). An SDK that uploads media separately loses them on this server unless it keeps them \
    inline: set LANGFUSE_MEDIA_UPLOAD_ENABLED=false where the SDK runs";

/// A field of a request that is wrong, as a JSON pointer and what to send instead.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldError {
    /// The JSON pointer of the member, such as `/traceId`; empty for the body as a whole.
    pub pointer: String,
    /// What is wrong and how to fix it.
    pub message: String,
}

fn field(pointer: &str, message: &str) -> FieldError {
    FieldError {
        pointer: pointer.to_owned(),
        message: message.to_owned(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadBody {
    trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dataset_id: Option<String>,
    content_type: Option<String>,
    content_length: Option<i64>,
    sha256_hash: Option<String>,
    field: Option<String>,
}

/// An upload request, checked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UploadRequest {
    /// The trace that uses the bytes.
    pub trace_id: TraceId,
    /// The observation, which is the span.
    pub span_id: Option<SpanId>,
    /// The declared content type.
    pub content_type: String,
    /// The declared length.
    pub len: u64,
    /// The SHA-256 of the bytes.
    pub sha256: [u8; 32],
    /// The payload field: `input`, `output` or `metadata`.
    pub field: String,
}

/// The payload fields an upload may name.
const FIELDS: [&str; 3] = ["input", "output", "metadata"];

/// Reads an upload request. Every wrong field is reported at once.
pub fn parse_upload(body: &[u8]) -> Result<UploadRequest, Vec<FieldError>> {
    let body: UploadBody = serde_json::from_slice(body).map_err(|error| {
        vec![field(
            "",
            &format!("the body must be a JSON object with the upload's fields: {error}"),
        )]
    })?;
    let mut errors = Vec::new();
    if body.dataset_id.is_some() {
        errors.push(field(
            "/datasetId",
            "media of dataset items is not stored; send media of traces, with traceId",
        ));
    }
    let trace_id = match body.trace_id.as_deref().map(TraceId::parse) {
        Some(Ok(id)) if !id.is_zero() => Some(id),
        _ => {
            errors.push(field(
                "/traceId",
                "send the trace id as 32 hexadecimal characters, as the SDK's OTLP spans carry it",
            ));
            None
        }
    };
    let span_id = match body.observation_id.as_deref() {
        None => None,
        Some(id) => match SpanId::parse(id) {
            Ok(id) if !id.is_zero() => Some(id),
            _ => {
                errors.push(field(
                    "/observationId",
                    "send the observation id as 16 hexadecimal characters, or leave it out",
                ));
                None
            }
        },
    };
    let len = match body.content_length.map(u64::try_from) {
        Some(Ok(len)) if len > 0 => Some(len),
        _ => {
            errors.push(field("/contentLength", "send the length in bytes, above 0"));
            None
        }
    };
    let sha256 = match body
        .sha256_hash
        .as_deref()
        .and_then(|hash| STANDARD.decode(hash).ok())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
    {
        Some(sha256) => Some(sha256),
        None => {
            errors.push(field(
                "/sha256Hash",
                "send the SHA-256 of the bytes as 44 characters of standard base64",
            ));
            None
        }
    };
    let field_name = match body.field.as_deref() {
        Some(name) if FIELDS.contains(&name) => Some(name.to_owned()),
        _ => {
            errors.push(field(
                "/field",
                "send the field the media is in: input, output or metadata",
            ));
            None
        }
    };
    if body.content_type.is_none() {
        errors.push(field("/contentType", "send the media's content type"));
    }
    match (trace_id, len, sha256, field_name, body.content_type) {
        (Some(trace_id), Some(len), Some(sha256), Some(field_name), Some(content_type))
            if errors.is_empty() =>
        {
            Ok(UploadRequest {
                trace_id,
                span_id,
                content_type,
                len,
                sha256,
                field: field_name,
            })
        }
        _ => Err(errors),
    }
}

/// The body a client sends to ask for an upload: what [`parse_upload`] reads back.
pub fn upload_request(request: &UploadRequest) -> Vec<u8> {
    serde_json::to_vec(&UploadBody {
        trace_id: Some(request.trace_id.to_string()),
        observation_id: request.span_id.map(|id| id.to_string()),
        dataset_id: None,
        content_type: Some(request.content_type.clone()),
        content_length: i64::try_from(request.len).ok(),
        sha256_hash: Some(STANDARD.encode(request.sha256)),
        field: Some(request.field.clone()),
    })
    .unwrap_or_default()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadAnswer<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_url: Option<&'a str>,
    media_id: &'a str,
}

/// The answer to an upload request: the media id, and the presigned upload URL when the bytes are not
/// stored yet.
pub fn upload_answer(media_id: &str, upload_url: Option<&str>) -> Vec<u8> {
    serde_json::to_vec(&UploadAnswer {
        upload_url,
        media_id,
    })
    .unwrap_or_default()
}

/// An upload answer as a client reads it.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadReply {
    /// The compat media id.
    pub media_id: String,
    /// The presigned upload URL; absent when the bytes are already stored.
    pub upload_url: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportBody {
    upload_http_status: Option<i64>,
}

/// The body a client sends to report the HTTP status its upload answered: what [`parse_report`] reads back.
pub fn report_request(upload_http_status: u16) -> Vec<u8> {
    serde_json::to_vec(&ReportBody {
        upload_http_status: Some(i64::from(upload_http_status)),
    })
    .unwrap_or_default()
}

/// Reads an upload report: whether the upload succeeded, by the HTTP status the SDK saw.
pub fn parse_report(body: &[u8]) -> Result<bool, Vec<FieldError>> {
    let body: ReportBody = serde_json::from_slice(body).map_err(|error| {
        vec![field(
            "",
            &format!("the body must be a JSON object with the upload's outcome: {error}"),
        )]
    })?;
    match body.upload_http_status {
        Some(status) if (100..600).contains(&status) => Ok((200..300).contains(&status)),
        _ => Err(vec![field(
            "/uploadHttpStatus",
            "send the HTTP status the upload answered, such as 200",
        )]),
    }
}

/// A stored object, as the endpoint's `GET` describes it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MediaAnswer<'a> {
    /// The compat media id.
    pub media_id: &'a str,
    /// The content type.
    pub content_type: &'a str,
    /// The length in bytes.
    pub len: u64,
    /// When the upload was confirmed, RFC 3339.
    pub uploaded_at: Option<String>,
    /// The presigned download URL.
    pub url: &'a str,
    /// When the URL expires, RFC 3339.
    pub url_expiry: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaBody<'a> {
    media_id: &'a str,
    content_type: &'a str,
    content_length: u64,
    uploaded_at: Option<&'a str>,
    url: &'a str,
    url_expiry: &'a str,
}

/// A `GET` answer as a client reads it.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaReply {
    /// The compat media id.
    pub media_id: String,
    /// The content type.
    pub content_type: String,
    /// The length in bytes.
    #[serde(rename = "contentLength")]
    pub len: u64,
    /// When the upload was confirmed, RFC 3339.
    pub uploaded_at: Option<String>,
    /// The presigned download URL.
    pub url: String,
    /// When the URL expires, RFC 3339.
    pub url_expiry: String,
}

/// The answer to a `GET`.
pub fn media_answer(media: &MediaAnswer<'_>) -> Vec<u8> {
    serde_json::to_vec(&MediaBody {
        media_id: media.media_id,
        content_type: media.content_type,
        content_length: media.len,
        uploaded_at: media.uploaded_at.as_deref(),
        url: media.url,
        url_expiry: &media.url_expiry,
    })
    .unwrap_or_default()
}

/// A media reference that a payload carries, in the token format that the compat media endpoint's SDKs write in
/// place of the media they upload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PayloadToken {
    /// The JSON pointer (RFC 6901) of the string that holds it: empty for a body that is the string itself, or that
    /// is not JSON.
    pub pointer: String,
    /// The content type the reference names.
    pub content_type: String,
    /// The SHA-256 values its media id can stand for, the least and the greatest ([`hash_range`]).
    pub hashes: ([u8; 32], [u8; 32]),
}

/// The most references [`payload_tokens`] reads from one body: a payload of up to 8 MiB could hold far more than
/// anyone looks at, and each one is looked up.
pub const MAX_PAYLOAD_TOKENS: usize = 64;

/// The delimiters of the tokens of the dialect whose SDKs call the compat media endpoint.
fn delimiters() -> Option<&'static MediaToken> {
    let dialect = MEDIA_ROUTES[0].dialect;
    crate::TABLES
        .iter()
        .find(|table| table.dialect == dialect)
        .and_then(|table| table.media.first())
}

/// The media references in one payload field's bytes, in the order they appear, at most [`MAX_PAYLOAD_TOKENS`].
/// A JSON body is read string by string, so that each reference has the pointer of the string that holds it;
/// any other body is read as text. A token without a content type, or whose id is not a compat media id, is not
/// a reference.
pub fn payload_tokens(body: &[u8]) -> Vec<PayloadToken> {
    let Some(token) = delimiters() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => walk(&value, &mut String::new(), token, &mut found),
        Err(_) => {
            if let Ok(text) = std::str::from_utf8(body) {
                scan(text, "", token, &mut found);
            }
        }
    }
    found
}

fn walk(
    value: &serde_json::Value,
    pointer: &mut String,
    token: &MediaToken,
    found: &mut Vec<PayloadToken>,
) {
    use serde_json::Value;
    let mut enter = |name: &str, item: &Value, found: &mut Vec<PayloadToken>| {
        let len = pointer.len();
        pointer.push('/');
        pointer.push_str(&name.replace('~', "~0").replace('/', "~1"));
        walk(item, pointer, token, found);
        pointer.truncate(len);
    };
    match value {
        Value::String(text) => scan(text, pointer, token, found),
        Value::Array(items) => {
            for (at, item) in items.iter().enumerate() {
                if found.len() >= MAX_PAYLOAD_TOKENS {
                    return;
                }
                enter(&at.to_string(), item, found);
            }
        }
        Value::Object(members) => {
            for (name, item) in members {
                if found.len() >= MAX_PAYLOAD_TOKENS {
                    return;
                }
                enter(name, item, found);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn scan(text: &str, pointer: &str, token: &MediaToken, found: &mut Vec<PayloadToken>) {
    let mut rest = text;
    while found.len() < MAX_PAYLOAD_TOKENS {
        let Some(at) = rest.find(token.prefix) else {
            return;
        };
        let inside = rest.get(at + token.prefix.len()..).unwrap_or_default();
        let Some(end) = inside.find(token.suffix).filter(|_| !token.suffix.is_empty()) else {
            return;
        };
        if let Some((content_type, hashes)) = token_fields(inside.get(..end).unwrap_or_default()) {
            found.push(PayloadToken {
                pointer: pointer.to_owned(),
                content_type,
                hashes,
            });
        }
        rest = inside.get(end + token.suffix.len()..).unwrap_or_default();
    }
}

/// The content type and the hash range of a token's fields, `type=<content type>|id=<media id>|...`.
fn token_fields(fields: &str) -> Option<(String, ([u8; 32], [u8; 32]))> {
    let (mut content_type, mut id) = (None, None);
    for field in fields.split('|') {
        match field.split_once('=') {
            Some(("type", value)) => content_type = Some(value.trim()),
            Some(("id", value)) => id = Some(value.trim()),
            _ => {}
        }
    }
    let content_type = content_type.filter(|value| value.contains('/'))?;
    Some((content_type.to_owned(), hash_range(id?)?))
}

/// The reference the SDKs write into a payload in place of bytes of `content_type` with this SHA-256: what
/// [`payload_tokens`] reads, and what tests play the SDK with.
pub fn payload_token(content_type: &str, sha256: &[u8; 32]) -> String {
    let (prefix, suffix) = delimiters().map_or(("", ""), |token| (token.prefix, token.suffix));
    format!(
        "{prefix}type={content_type}|id={}|source=bytes{suffix}",
        media_id(sha256)
    )
}

/// The compat media id of bytes with this SHA-256: the first 22 characters of its URL-safe base64 form.
pub fn media_id(sha256: &[u8; 32]) -> String {
    let mut id = URL_SAFE_NO_PAD.encode(sha256);
    id.truncate(MEDIA_ID_LEN);
    id
}

/// The SHA-256 values a compat media id can stand for, as the least and the greatest: its 22 characters fix
/// the first 132 bits. `None` for anything that is not such an id.
pub fn hash_range(id: &str) -> Option<([u8; 32], [u8; 32])> {
    if id.len() != MEDIA_ID_LEN {
        return None;
    }
    // Two more zero characters make 24, which decode to 18 bytes: 16 whole bytes and the high half of one.
    let decoded = URL_SAFE_NO_PAD.decode(format!("{id}AA")).ok()?;
    let mut low = [0u8; 32];
    low[..17].copy_from_slice(decoded.get(..17)?);
    let mut high = [0xffu8; 32];
    high[..16].copy_from_slice(&low[..16]);
    high[16] = low[16] | 0x0f;
    Some((low, high))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::routes::{INGEST_ROUTES, under_route_prefix};

    #[test]
    fn every_media_route_lies_under_its_dialects_ingest_prefix() {
        for route in MEDIA_ROUTES {
            assert!(under_route_prefix(route.collection), "{route:?}");
            assert!(
                INGEST_ROUTES
                    .iter()
                    .any(|ingest| ingest.dialect == route.dialect)
            );
            assert!(route.collection.starts_with('/') && !route.collection.ends_with('/'));
        }
    }

    /// The refusal names both ways to keep the media: the SDK's setting and the server's (contract 1.7). The
    /// startup notice names both settings too.
    #[test]
    fn the_refusal_without_object_storage_says_how_to_keep_the_media() {
        for setting in [
            "LANGFUSE_MEDIA_UPLOAD_ENABLED=false",
            "object_store.endpoint",
            "object_store.bucket",
            "CRUCIBLE_OBJECT_STORE_",
            "postgres.url",
        ] {
            assert!(NOT_SUPPORTED.contains(setting), "{setting}");
        }
        for setting in [
            "LANGFUSE_MEDIA_UPLOAD_ENABLED=false",
            "object_store.endpoint",
        ] {
            assert!(NO_OBJECT_STORAGE.contains(setting), "{setting}");
        }
    }

    #[test]
    fn an_upload_request_is_read_with_every_field() {
        let body = br#"{"traceId":"0123456789abcdef0123456789abcdef","observationId":"0123456789abcdef",
            "contentType":"image/png","contentLength":12,"sha256Hash":"47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "field":"input","datasetId":null,"datasetItemId":null}"#;
        let request = parse_upload(body).unwrap();
        assert_eq!(
            request.trace_id.to_string(),
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            request.span_id.map(|id| id.to_string()).as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!((request.len, request.field.as_str()), (12, "input"));
        assert_eq!(media_id(&request.sha256), "47DEQpj8HBSa-_TImW-5JC");
    }

    #[test]
    fn a_wrong_upload_request_names_each_wrong_field() {
        let body =
            br#"{"traceId":"trace-1","observationId":"x","contentLength":0,"sha256Hash":"abc",
            "field":"body","datasetId":"d"}"#;
        let pointers: Vec<String> = parse_upload(body)
            .unwrap_err()
            .into_iter()
            .map(|error| error.pointer)
            .collect();
        assert_eq!(
            pointers,
            [
                "/datasetId",
                "/traceId",
                "/observationId",
                "/contentLength",
                "/sha256Hash",
                "/field",
                "/contentType"
            ]
        );
        assert_eq!(parse_upload(b"[]").unwrap_err()[0].pointer, "");
    }

    #[test]
    fn answers_carry_the_endpoints_field_names() {
        let upload = String::from_utf8(upload_answer("id", Some("http://s/u"))).unwrap();
        assert_eq!(upload, r#"{"uploadUrl":"http://s/u","mediaId":"id"}"#);
        let stored = String::from_utf8(upload_answer("id", None)).unwrap();
        assert_eq!(stored, r#"{"mediaId":"id"}"#);
        let media = String::from_utf8(media_answer(&MediaAnswer {
            media_id: "id",
            content_type: "image/png",
            len: 3,
            uploaded_at: Some("2026-11-19T00:00:00.000000Z".to_owned()),
            url: "http://s/g",
            url_expiry: "2026-11-19T00:05:00.000000Z".to_owned(),
        }))
        .unwrap();
        assert_eq!(
            media,
            r#"{"mediaId":"id","contentType":"image/png","contentLength":3,"uploadedAt":"2026-11-19T00:00:00.000000Z","url":"http://s/g","urlExpiry":"2026-11-19T00:05:00.000000Z"}"#
        );
    }

    #[test]
    fn reports_succeed_on_a_2xx_status() {
        let report = |status: &str| {
            parse_report(
                format!(r#"{{"uploadedAt":"2026-11-19T00:00:00Z","uploadHttpStatus":{status}}}"#)
                    .as_bytes(),
            )
        };
        assert_eq!(report("200"), Ok(true));
        assert_eq!(report("204"), Ok(true));
        assert_eq!(report("403"), Ok(false));
        assert!(report("0").is_err());
        assert!(parse_report(b"{}").is_err());
        assert_eq!(parse_report(&report_request(200)), Ok(true));
        assert_eq!(parse_report(&report_request(403)), Ok(false));
    }

    #[test]
    fn a_client_reads_the_answers_back() {
        let reply = |body: Vec<u8>| serde_json::from_slice::<UploadReply>(&body).unwrap();
        assert_eq!(
            reply(upload_answer("id", Some("http://s/u"))),
            UploadReply {
                media_id: "id".to_owned(),
                upload_url: Some("http://s/u".to_owned()),
            }
        );
        assert_eq!(reply(upload_answer("id", None)).upload_url, None);
        let answer = MediaAnswer {
            media_id: "id",
            content_type: "image/png",
            len: 3,
            uploaded_at: None,
            url: "http://s/g",
            url_expiry: "2026-11-19T00:05:00.000000Z".to_owned(),
        };
        let read: MediaReply = serde_json::from_slice(&media_answer(&answer)).unwrap();
        assert_eq!(
            read,
            MediaReply {
                media_id: "id".to_owned(),
                content_type: "image/png".to_owned(),
                len: 3,
                uploaded_at: None,
                url: "http://s/g".to_owned(),
                url_expiry: answer.url_expiry.clone(),
            }
        );
    }

    /// The references in a payload are found in every string of a JSON body, each with the pointer of its string,
    /// and in a body that is not JSON, without one.
    #[test]
    fn payload_tokens_are_found_with_where_they_are() {
        let (png, pdf) = ([1u8; 32], [2u8; 32]);
        let image = payload_token("image/png", &png);
        assert!(image.starts_with("@@@langfuseMedia:type=image/png|id="));
        let document = payload_token("application/pdf", &pdf);
        let body = serde_json::json!([
            {"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": image}},
            ]},
            {"a/b~c": format!("see {document} and {image}")},
        ]);
        let found = payload_tokens(body.to_string().as_bytes());
        let seen: Vec<(&str, &str)> = found
            .iter()
            .map(|t| (t.pointer.as_str(), t.content_type.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("/0/content/1/image_url/url", "image/png"),
                ("/1/a~1b~0c", "application/pdf"),
                ("/1/a~1b~0c", "image/png"),
            ]
        );
        assert_eq!(found[0].hashes, hash_range(&media_id(&png)).unwrap());
        assert_eq!(found[1].hashes, hash_range(&media_id(&pdf)).unwrap());
        let text = format!("an image, {image}, in plain text");
        let found = payload_tokens(text.as_bytes());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pointer, "");
        assert_eq!(
            payload_tokens(serde_json::json!(image).to_string().as_bytes())[0].pointer,
            ""
        );
    }

    #[test]
    fn what_is_not_a_whole_token_with_an_id_is_left_alone() {
        let id = media_id(&[3; 32]);
        for body in [
            format!("@@@langfuseMedia:type=image/png|id={id}"),
            "@@@langfuseMedia:type=image/png|id=short|source=bytes@@@".to_owned(),
            format!("@@@langfuseMedia:id={id}|source=bytes@@@"),
            format!("@@@langfuseMedia:type=|id={id}@@@"),
            format!("@@@otherMedia:type=image/png|id={id}@@@"),
            String::new(),
        ] {
            assert!(payload_tokens(body.as_bytes()).is_empty(), "{body}");
        }
    }

    #[test]
    fn a_body_gives_at_most_the_cap_of_references() {
        let token = payload_token("image/png", &[4; 32]);
        let body = serde_json::json!(vec![token; MAX_PAYLOAD_TOKENS + 10]);
        assert_eq!(
            payload_tokens(body.to_string().as_bytes()).len(),
            MAX_PAYLOAD_TOKENS
        );
    }

    #[test]
    fn texts_that_are_not_ids_are_refused() {
        for id in [
            "",
            "short",
            "47DEQpj8HBSa-_TImW-5JC=",
            "47DEQpj8HBSa+/TImW+5JC",
        ] {
            assert_eq!(hash_range(id), None, "{id}");
        }
    }

    proptest! {
        /// A hash lies in the range its own id names, and the range holds only hashes with that id.
        #[test]
        fn a_hash_is_found_by_its_id(sha256: [u8; 32], other: [u8; 32]) {
            let id = media_id(&sha256);
            prop_assert_eq!(id.len(), MEDIA_ID_LEN);
            let (low, high) = hash_range(&id).unwrap();
            prop_assert!(low <= sha256 && sha256 <= high);
            let inside = low <= other && other <= high;
            prop_assert_eq!(inside, media_id(&other) == id);
        }

        /// The reference the SDKs write for bytes is read back with their content type and the range their hash is
        /// in, wherever the payload holds it.
        #[test]
        fn a_payload_token_reads_back(
            content_type in "[a-z]{1,12}/[a-z0-9.+-]{1,24}",
            sha256: [u8; 32],
            before in "[ -~]{0,20}",
            key in "[a-z/~]{1,8}",
        ) {
            let token = payload_token(&content_type, &sha256);
            let body = serde_json::json!({ key.clone(): format!("{before}{token}") });
            let found = payload_tokens(body.to_string().as_bytes());
            prop_assert_eq!(found.len(), 1);
            let (low, high) = found[0].hashes;
            prop_assert!(low <= sha256 && sha256 <= high);
            prop_assert_eq!(&found[0].content_type, &content_type);
            let pointer = format!("/{}", key.replace('~', "~0").replace('/', "~1"));
            prop_assert_eq!(&found[0].pointer, &pointer);
        }

        /// What a client sends is read back as it was meant.
        #[test]
        fn an_upload_request_round_trips(
            trace: [u8; 16],
            span: Option<[u8; 8]>,
            content_type in "[a-z]{1,12}/[a-z0-9.+-]{1,24}",
            len in 1..=i64::MAX.unsigned_abs(),
            sha256: [u8; 32],
            at in 0..FIELDS.len(),
        ) {
            let trace_id = TraceId::from_bytes(trace);
            prop_assume!(!trace_id.is_zero());
            let request = UploadRequest {
                trace_id,
                span_id: span.map(SpanId::from_bytes).and_then(SpanId::non_zero),
                content_type,
                len,
                sha256,
                field: FIELDS[at].to_owned(),
            };
            prop_assert_eq!(parse_upload(&upload_request(&request)), Ok(request));
        }
    }
}
