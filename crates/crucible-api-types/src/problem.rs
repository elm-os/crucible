//! The problem document of every error response (contract 12.3, RFC 9457).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

/// Declares the problem codes with their HTTP status, in the order of the contract's table.
macro_rules! problem_codes {
    ($($(#[$meta:meta])* $variant:ident = $status:literal => $text:literal,)+) => {
        /// The stable code of a problem. Clients branch on it; `title` and `detail` are for logs
        /// and developers.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
        #[serde(rename_all = "snake_case")]
        pub enum ProblemCode {
            $($(#[$meta])* $variant,)+
        }

        impl ProblemCode {
            /// Every code, in the order of the contract's table.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The wire name.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// The HTTP status that carries the problem.
            pub const fn status(self) -> u16 {
                match self {
                    $(Self::$variant => $status,)+
                }
            }

            /// The code with this wire name.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

problem_codes! {
    /// No or invalid credential.
    Unauthenticated = 401 => "unauthenticated",
    /// Wrong email or password.
    InvalidCredentials = 401 => "invalid_credentials",
    /// Role or scope too low.
    Forbidden = 403 => "forbidden",
    /// Missing or wrong CSRF header.
    CsrfInvalid = 403 => "csrf_invalid",
    /// The `Origin` or `Sec-Fetch-Site` check failed.
    OriginInvalid = 403 => "origin_invalid",
    /// A valid API key lacks the scope the request needs, on the MCP server and the compat media endpoint;
    /// `detail` names the scope, the scopes the key has and how to make a key with it.
    KeyScopeMissing = 403 => "key_scope_missing",
    /// Unknown route or entity, an entity the caller cannot see, or one that was deleted or expired.
    NotFound = 404 => "not_found",
    /// The route exists but not for this method.
    MethodNotAllowed = 405 => "method_not_allowed",
    /// The server-side request deadline passed.
    RequestTimeout = 408 => "request_timeout",
    /// Unique or state conflict.
    Conflict = 409 => "conflict",
    /// An `Idempotency-Key` was reused with a different request.
    IdempotencyKeyReused = 409 => "idempotency_key_reused",
    /// `If-Match` did not match.
    PreconditionFailed = 412 => "precondition_failed",
    /// The request body is too large.
    PayloadTooLarge = 413 => "payload_too_large",
    /// The request body's media type is not accepted.
    UnsupportedMediaType = 415 => "unsupported_media_type",
    /// Body, query or path invalid; `errors` has JSON pointers.
    ValidationFailed = 422 => "validation_failed",
    /// A reset, invitation or setup token is invalid, expired or used.
    TokenInvalid = 422 => "token_invalid",
    /// A mutable resource was changed without `If-Match`.
    PreconditionRequired = 428 => "precondition_required",
    /// Too many requests; see `Retry-After` and `retry_after_s`.
    RateLimited = 429 => "rate_limited",
    /// The window is above the project maximum, or a row, byte, id or granularity cap was exceeded.
    QueryTooBroad = 422 => "query_too_broad",
    /// The query class time limit passed.
    QueryTimeout = 504 => "query_timeout",
    /// The query class memory limit was exceeded.
    QueryMemoryExceeded = 422 => "query_memory_exceeded",
    /// The cursor is malformed, expired, from another project, or bound to other filters, sort or window.
    CursorInvalid = 400 => "cursor_invalid",
    /// The control-plane database is unreachable.
    ControlPlaneUnavailable = 503 => "control_plane_unavailable",
    /// The telemetry store is unreachable and the recent index cannot answer.
    TelemetryStoreUnavailable = 503 => "telemetry_store_unavailable",
    /// The WAL cannot accept a fact or tombstone write.
    IngestUnavailable = 503 => "ingest_unavailable",
    /// A module is off for the deployment or project.
    FeatureDisabled = 404 => "feature_disabled",
    /// A webhook target was rejected by the egress policy.
    SsrfBlocked = 422 => "ssrf_blocked",
    /// No object store is configured.
    MediaNotSupported = 501 => "media_not_supported",
    /// The media object is above the cap.
    MediaTooLarge = 413 => "media_too_large",
    /// An uploaded object does not match its declared hash or length.
    MediaHashMismatch = 422 => "media_hash_mismatch",
    /// SQL syntax or lexical rejection.
    SqlInvalid = 422 => "sql_invalid",
    /// SQL that is not read-only, or that the store denied.
    SqlForbidden = 403 => "sql_forbidden",
    /// SQL that does not use `{from}` and `{to}`.
    SqlTimeWindowRequired = 422 => "sql_time_window_required",
    /// A SQL quota or concurrency limit was reached.
    SqlQuotaExceeded = 429 => "sql_quota_exceeded",
    /// The SQL workspace is disabled.
    SqlWorkspaceUnavailable = 503 => "sql_workspace_unavailable",
    /// The export file expired or predates a deletion request.
    ExportExpired = 410 => "export_expired",
    /// Anything else, including a caught panic.
    InternalError = 500 => "internal_error",
}

/// One invalid part of a request.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FieldError {
    /// A JSON pointer to the invalid part: `/<name>` for a query or path parameter.
    pub pointer: String,
    /// A stable code for the failure, such as `required` or `invalid_timestamp`.
    pub code: String,
    /// A developer-facing message; clients show their own copy.
    pub message: String,
}

/// The limit a request exceeded.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ProblemLimit {
    /// The limit, for example `max_window_days`.
    pub name: String,
    /// Its value.
    pub value: u32,
}

/// An error response, sent as `application/problem+json` (RFC 9457).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Problem {
    /// `urn:crucible:problem:<code>`.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// A short summary of the problem type.
    pub title: String,
    /// The HTTP status.
    pub status: u16,
    /// What went wrong with this request, for logs and developers.
    pub detail: String,
    /// The stable code.
    pub code: ProblemCode,
    /// The request id, also sent as `X-Request-Id`.
    pub request_id: String,
    /// The invalid parts of the request, for `validation_failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub errors: Option<Vec<FieldError>>,
    /// Seconds to wait before retrying, for `rate_limited` and `sql_quota_exceeded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub retry_after_s: Option<u32>,
    /// The limit the request exceeded, for `query_too_broad`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub limit: Option<ProblemLimit>,
    /// A coarser granularity that fits the caps, for `query_too_broad` on metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub suggested_granularity: Option<String>,
    /// The line of the error in a query text, one-based, for `sql_invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub line: Option<u32>,
    /// The column of the error in a query text, one-based, for `sql_invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub column: Option<u32>,
    /// Why a module is unavailable, for `sql_workspace_unavailable`: a stable word such as
    /// `isolation_probe_failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub reason: Option<String>,
}

impl Problem {
    /// The `type` URI of a code.
    pub fn type_uri(code: ProblemCode) -> String {
        format!("urn:crucible:problem:{}", code.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_contract_table() {
        assert_eq!(ProblemCode::ALL.len(), 37);
        for &code in ProblemCode::ALL {
            let wire = serde_json::to_value(code).unwrap();
            assert_eq!(wire.as_str(), Some(code.as_str()));
            assert_eq!(ProblemCode::parse(code.as_str()), Some(code));
            assert!((400..600).contains(&code.status()), "{code:?}");
        }
        let status = |text| ProblemCode::parse(text).map(ProblemCode::status);
        assert_eq!(status("unauthenticated"), Some(401));
        assert_eq!(status("key_scope_missing"), Some(403));
        assert_eq!(status("not_found"), Some(404));
        assert_eq!(status("validation_failed"), Some(422));
        assert_eq!(status("query_too_broad"), Some(422));
        assert_eq!(status("query_timeout"), Some(504));
        assert_eq!(status("cursor_invalid"), Some(400));
        assert_eq!(status("telemetry_store_unavailable"), Some(503));
        assert_eq!(status("internal_error"), Some(500));
        assert_eq!(ProblemCode::parse("teapot"), None);
    }

    #[test]
    fn optional_members_are_absent_when_unset() {
        let problem = Problem {
            problem_type: Problem::type_uri(ProblemCode::NotFound),
            title: "Not found".to_owned(),
            status: 404,
            detail: "nothing here".to_owned(),
            code: ProblemCode::NotFound,
            request_id: "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b".to_owned(),
            errors: None,
            retry_after_s: None,
            limit: None,
            suggested_granularity: None,
            line: None,
            column: None,
            reason: None,
        };
        let json = serde_json::to_value(&problem).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["code", "detail", "request_id", "status", "title", "type"]
        );
        assert_eq!(json["type"], "urn:crucible:problem:not_found");
        assert_eq!(serde_json::from_value::<Problem>(json).unwrap(), problem);
    }
}
