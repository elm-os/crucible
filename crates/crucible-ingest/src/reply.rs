//! OTLP responses. Over HTTP they are encoded as protobuf or JSON to match the request:
//! `ExportTraceServiceResponse` with optional partial success on 200, and a `google.rpc.Status` with a
//! developer-facing message on every refusal; 429 and 503 carry `Retry-After` (contract 4.8) and 401 carries
//! the challenges of both key forms. Over gRPC the same refusal is a status with the code of contract 4.8, and
//! `RESOURCE_EXHAUSTED` and `UNAVAILABLE` carry a `RetryInfo` in the status details.

use axum::http::header::{CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use crucible_auth::Refusal;
use crucible_otlp::PartialSuccess;
use tonic::{Code, Status};

use crate::body::Format;

/// The challenges of a 401: a secret key as Bearer, or a key pair as Basic (contract 12.2).
const CHALLENGES: &str = "Bearer, Basic realm=\"crucible\"";

/// The type URL of `google.rpc.RetryInfo` in a status detail.
const RETRY_INFO: &str = "type.googleapis.com/google.rpc.RetryInfo";

/// Why a request is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reject {
    /// No valid credential in `Authorization`, and why (401).
    Unauthenticated(Refusal),
    /// A media type or content coding other than the supported ones (415).
    UnsupportedMediaType(String),
    /// Over a cap: the body, the inflated body, the span count or the WAL frame (413).
    TooLarge(String),
    /// Not a well-formed request (400).
    Malformed(String),
    /// The in-flight byte budget or the project's byte rate is exhausted (429).
    TooManyRequests {
        /// Seconds for `Retry-After`.
        retry_after_s: u32,
        /// The developer-facing reason: the setting that refused the request.
        detail: &'static str,
    },
    /// The WAL cannot take the request, or failed to make it durable (503).
    Unavailable {
        /// Seconds for `Retry-After`.
        retry_after_s: u32,
        /// The developer-facing reason.
        detail: &'static str,
    },
    /// A failure that the request did not cause (500).
    Internal(&'static str),
}

impl Reject {
    /// The `outcome` label values of `crucible_ingest_requests_total` for refusals, indexed by
    /// [`Reject::outcome`].
    pub(crate) const OUTCOMES: [&'static str; 7] = [
        "unauthenticated",
        "unsupported_media_type",
        "payload_too_large",
        "malformed",
        "rate_limited",
        "unavailable",
        "internal",
    ];

    /// The index of this refusal's label in [`Reject::OUTCOMES`].
    pub(crate) const fn outcome(&self) -> usize {
        match self {
            Self::Unauthenticated(_) => 0,
            Self::UnsupportedMediaType(_) => 1,
            Self::TooLarge(_) => 2,
            Self::Malformed(_) => 3,
            Self::TooManyRequests { .. } => 4,
            Self::Unavailable { .. } => 5,
            Self::Internal(_) => 6,
        }
    }

    const fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            Self::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Malformed(_) => StatusCode::BAD_REQUEST,
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The gRPC code that gRPC ingest answers for the same refusal (contract 4.8).
    const fn grpc_code(&self) -> Code {
        match self {
            Self::UnsupportedMediaType(_) | Self::TooLarge(_) | Self::Malformed(_) => {
                Code::InvalidArgument
            }
            Self::TooManyRequests { .. } => Code::ResourceExhausted,
            Self::Internal(_) => Code::Internal,
            Self::Unavailable { .. } => Code::Unavailable,
            Self::Unauthenticated(_) => Code::Unauthenticated,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Unauthenticated(refusal) => refusal.to_string(),
            Self::UnsupportedMediaType(detail)
            | Self::TooLarge(detail)
            | Self::Malformed(detail) => detail.clone(),
            Self::TooManyRequests { detail, .. }
            | Self::Unavailable { detail, .. }
            | Self::Internal(detail) => (*detail).to_owned(),
        }
    }

    const fn retry_after_s(&self) -> Option<u32> {
        match self {
            Self::TooManyRequests { retry_after_s, .. }
            | Self::Unavailable { retry_after_s, .. } => Some(*retry_after_s),
            _ => None,
        }
    }

    /// The refusal as a gRPC status.
    pub(crate) fn grpc_status(&self) -> Status {
        grpc_status(self.grpc_code(), self.message(), self.retry_after_s())
    }
}

/// A gRPC status. One that asks for a retry carries `google.rpc.Status` details with a `RetryInfo` of
/// `retry_after_s` seconds, which OTLP exporters wait for before they retry.
pub(crate) fn grpc_status(code: Code, message: String, retry_after_s: Option<u32>) -> Status {
    let Some(seconds) = retry_after_s else {
        return Status::new(code, message);
    };
    // google.protobuf.Duration { seconds }, in google.rpc.RetryInfo { retry_delay }, in google.protobuf.Any.
    let mut delay = Vec::new();
    put_int(&mut delay, 1, u64::from(seconds));
    let mut retry_info = Vec::new();
    put_bytes(&mut retry_info, 1, &delay);
    let mut any = Vec::new();
    put_bytes(&mut any, 1, RETRY_INFO.as_bytes());
    put_bytes(&mut any, 2, &retry_info);
    // google.rpc.Status { code, message, details }.
    let mut details = Vec::new();
    put_int(&mut details, 1, u64::from((code as i32).unsigned_abs()));
    put_bytes(&mut details, 2, message.as_bytes());
    put_bytes(&mut details, 3, &any);
    Status::with_details(code, message, Bytes::from(details))
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A varint field.
fn put_int(out: &mut Vec<u8>, field: u8, value: u64) {
    out.push(field << 3);
    put_varint(out, value);
}

/// A length-delimited field.
fn put_bytes(out: &mut Vec<u8>, field: u8, bytes: &[u8]) {
    out.push((field << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn respond(status: StatusCode, format: Format, body: Vec<u8>) -> Response {
    let content_type = HeaderValue::from_static(format.content_type());
    (status, [(CONTENT_TYPE, content_type)], body).into_response()
}

/// The protobuf `ExportTraceServiceResponse`: empty for a full success, else with `partial_success`.
pub(crate) fn export_response(partial: Option<&PartialSuccess>) -> Vec<u8> {
    let Some(partial) = partial else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    put_int(&mut fields, 1, u64::from(partial.rejected_spans()));
    put_bytes(&mut fields, 2, partial.to_string().as_bytes());
    let mut out = Vec::new();
    put_bytes(&mut out, 1, &fields);
    out
}

/// 200 with an `ExportTraceServiceResponse`; `partial_success` is set only when spans were rejected, and int64
/// fields are decimal strings in JSON (the protobuf JSON mapping that OTLP/JSON follows).
pub(crate) fn accepted(format: Format, partial: Option<&PartialSuccess>) -> Response {
    let body = match (format, partial) {
        (Format::Protobuf, _) => export_response(partial),
        (Format::Json, None) => b"{}".to_vec(),
        (Format::Json, Some(partial)) => serde_json::json!({
            "partialSuccess": {
                "rejectedSpans": partial.rejected_spans().to_string(),
                "errorMessage": partial.to_string(),
            }
        })
        .to_string()
        .into_bytes(),
    };
    respond(StatusCode::OK, format, body)
}

/// The refusal as a `google.rpc.Status` with the gRPC code of the same refusal, plus `Retry-After` or the
/// challenges where they apply.
pub(crate) fn rejected(format: Format, reject: &Reject) -> Response {
    let (code, message) = (reject.grpc_code() as i32, reject.message());
    let body = match format {
        Format::Protobuf => {
            let mut out = Vec::new();
            put_int(&mut out, 1, u64::from(code.unsigned_abs()));
            put_bytes(&mut out, 2, message.as_bytes());
            out
        }
        Format::Json => serde_json::json!({ "code": code, "message": message })
            .to_string()
            .into_bytes(),
    };
    let mut response = respond(reject.status(), format, body);
    let headers = response.headers_mut();
    if let Some(seconds) = reject.retry_after_s() {
        headers.insert(RETRY_AFTER, HeaderValue::from(seconds));
    }
    if matches!(reject, Reject::Unauthenticated(_)) {
        headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static(CHALLENGES));
    }
    response
}
