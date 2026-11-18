//! `GET projects/{project_id}/spans/{trace_id}/{span_id}/payload`: one payload field of one span as raw bytes,
//! whole or as a byte range, for the payload viewer (contract 8.7 and 12.6).

use axum::Router;
use axum::extract::State;
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_TYPE, ETAG, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use crucible_api_types::{PayloadParams, Problem};
use crucible_plan::PayloadField;
use crucible_query::PayloadQuery;
use crucible_records::SpanId;
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::params::Params;
use crate::problem::ApiError;

/// The length that reads a field from its offset to its end: no field is this long.
const REST: u64 = 1 << 40;

/// The bytes are the span's data as it was sent, so they are never served as a type a browser renders.
const TEXT: &str = "text/plain; charset=utf-8";

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route(
        "/projects/{project_id}/spans/{trace_id}/{span_id}/payload",
        get(span_payload),
    )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(span_payload))]
pub(super) struct Paths;

/// Read a span's payload field
///
/// The field's bytes as stored: JSON or text, cut at the size cap when the span named it in
/// `truncated_fields`, so possibly no longer valid JSON. A `Range: bytes=<first>-[<last>]` header asks for part
/// of it; other forms of the header are answered with the whole field. A span the store does not serve yet is
/// read from the write-ahead log.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/spans/{trace_id}/{span_id}/payload",
    operation_id = "get_span_payload",
    tag = "traces",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("trace_id" = String, Path, description = "The trace id (32 hex characters)."),
        ("span_id" = String, Path, description = "The span id (16 hex characters)."),
        ("Range" = Option<String>, Header, description = "`bytes=<first>-[<last>]`: one byte range of the field."),
        PayloadParams,
    ),
    responses(
        (status = 200, description = "The whole field.", body = String, content_type = "text/plain",
            headers(
                ("Content-Length" = u64, description = "The field's length in bytes."),
                ("ETag" = String, description = "The same for every range of the field."),
                ("Accept-Ranges" = String, description = "`bytes`."),
            )),
        (status = 206, description = "The range of the field that `Range` asked for.", body = String,
            content_type = "text/plain",
            headers(("Content-Range" = String, description = "`bytes <first>-<last>/<length of the field>`."))),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no such payload (it expired under the payload retention, or the span was never stored), or the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 416, description = "The range starts past the end of the field: `Content-Range: bytes */<length>`."),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`, also while the span may not be readable yet.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn span_payload(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let trace_id = params.trace_id(&project.path);
    let span_id = project
        .path
        .get("span_id")
        .and_then(|text| SpanId::parse(text).ok());
    if span_id.is_none() {
        params.error("span_id", "invalid_id", "expected 16 hex characters");
    }
    let field = params.required("field").and_then(|selector| {
        let field = PayloadField::parse(&selector);
        if field.is_none() {
            let selectors = PayloadField::ALL.map(PayloadField::as_str).join(", ");
            let message = format!("expected one of {selectors}");
            params.error("field", "invalid", &message);
        }
        field
    });
    let start_time = params.time("start_time");
    let download = match params.optional("download").as_deref() {
        None => false,
        Some("1") => true,
        Some(_) => {
            params.error(
                "download",
                "invalid",
                "expected `1`, or leave `download` out",
            );
            false
        }
    };
    params.finish()?;
    let (Some(trace_id), Some(span_id), Some(field), Some(start_time)) =
        (trace_id, span_id, field, start_time)
    else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let range = byte_range(&headers);
    let service = state.service(rid)?;
    let query = PayloadQuery {
        trace_id,
        span_id,
        start_time,
        field,
        range,
    };
    let found = service.payload(&project.ctx, query).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;

    let total = found.total;
    let etag = format!(
        "\"{trace_id}-{span_id}-{}-{}-{total}\"",
        start_time.micros(),
        field.as_str()
    );
    let partial = match range {
        // An empty field has no byte to range over: it is sent whole.
        Some((offset, _)) if total > 0 => {
            if offset >= total {
                let mut refused = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                insert(&mut refused, CONTENT_RANGE, &format!("bytes */{total}"));
                return Ok(refused);
            }
            let last = offset + found.bytes.len() as u64 - 1;
            Some(format!("bytes {offset}-{last}/{total}"))
        }
        _ => None,
    };
    let status = match partial {
        Some(_) => StatusCode::PARTIAL_CONTENT,
        None => StatusCode::OK,
    };
    let mut response = (status, found.bytes).into_response();
    insert(&mut response, CONTENT_TYPE, TEXT);
    insert(&mut response, ACCEPT_RANGES, "bytes");
    insert(&mut response, ETAG, &etag);
    insert(
        &mut response,
        CACHE_CONTROL,
        "private, max-age=31536000, immutable",
    );
    if let Some(partial) = partial {
        insert(&mut response, CONTENT_RANGE, &partial);
    }
    if download {
        let name = format!("attachment; filename=\"{span_id}-{}.txt\"", field.as_str());
        insert(&mut response, CONTENT_DISPOSITION, &name);
    }
    Ok(response)
}

fn insert(response: &mut Response, name: axum::http::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

/// The one range of a `Range: bytes=<first>-[<last>]` header, as the first byte and the number of bytes. No
/// header, or one this server answers with the whole field (several ranges, a suffix range, another unit or a
/// malformed value, which RFC 9110 lets a server ignore), is `None`.
fn byte_range(headers: &HeaderMap) -> Option<(u64, u64)> {
    let value = headers.get(RANGE)?.to_str().ok()?.trim();
    let (unit, spec) = value.split_once('=')?;
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last = last.trim();
    if last.is_empty() {
        return Some((first, REST));
    }
    let last: u64 = last.parse().ok()?;
    let len = last.checked_sub(first)?.checked_add(1)?;
    Some((first, len.min(REST)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(value: &str) -> Option<(u64, u64)> {
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_str(value).unwrap());
        byte_range(&headers)
    }

    #[test]
    fn one_byte_range_is_read_and_every_other_form_is_ignored() {
        assert_eq!(byte_range(&HeaderMap::new()), None);
        assert_eq!(range("bytes=0-1023"), Some((0, 1_024)));
        assert_eq!(range("Bytes = 10-10"), Some((10, 1)));
        assert_eq!(range("bytes=4096-"), Some((4_096, REST)));
        for ignored in [
            "bytes=-500",
            "bytes=0-1,5-6",
            "bytes=9-3",
            "items=0-1",
            "bytes=a-b",
            "bytes",
            "bytes=0-18446744073709551615",
        ] {
            assert_eq!(range(ignored), None, "{ignored}");
        }
    }
}
