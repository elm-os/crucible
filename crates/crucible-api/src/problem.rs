//! Error responses: problem documents (contract 12.3) that carry the request id.

use axum::http::header::{CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{Extensions, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use crucible_api_types::{FieldError, Problem, ProblemCode, ProblemLimit};
use crucible_env::Env;
use crucible_query::QueryServiceError;
use crucible_records::RequestId;

/// The media type of every error response.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// The request id the request-id layer stored for this request, or a new one when the layer did not run.
pub fn request_id(extensions: &Extensions, env: &Env) -> RequestId {
    extensions
        .get::<RequestId>()
        .copied()
        .unwrap_or_else(|| RequestId::from_uuid(env.new_id()))
}

/// The response for a problem without field errors or a limit.
pub fn problem(request_id: RequestId, code: ProblemCode, detail: &str) -> Response {
    ApiError::new(request_id, code, detail).into_response()
}

/// A failed request, answered as a problem document.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct ApiError {
    request_id: RequestId,
    code: ProblemCode,
    detail: String,
    errors: Vec<FieldError>,
    /// The rarer members, boxed so that every `Result` of a handler stays small.
    extra: Option<Box<Extra>>,
}

/// The members of a problem that only some codes carry.
#[derive(Clone, PartialEq, Debug, Default)]
struct Extra {
    limit: Option<ProblemLimit>,
    retry_after_s: Option<u32>,
    suggested_granularity: Option<String>,
}

impl ApiError {
    pub(crate) fn new(request_id: RequestId, code: ProblemCode, detail: impl Into<String>) -> Self {
        Self {
            request_id,
            code,
            detail: detail.into(),
            errors: Vec::new(),
            extra: None,
        }
    }

    fn extra(mut self, set: impl FnOnce(&mut Extra)) -> Self {
        set(self.extra.get_or_insert_with(Box::default));
        self
    }

    pub(crate) fn unauthenticated(request_id: RequestId) -> Self {
        Self::new(
            request_id,
            ProblemCode::Unauthenticated,
            "a valid secret key is required as `Authorization: Bearer <key>`",
        )
    }

    pub(crate) fn not_found(request_id: RequestId) -> Self {
        Self::new(
            request_id,
            ProblemCode::NotFound,
            "no resource exists at this path",
        )
    }

    /// `validation_failed` with the invalid parts of the request.
    pub(crate) fn validation(request_id: RequestId, errors: Vec<FieldError>) -> Self {
        Self {
            errors,
            ..Self::new(
                request_id,
                ProblemCode::ValidationFailed,
                "the request is invalid; see `errors`",
            )
        }
    }

    /// Sets `retry_after_s` and the `Retry-After` header.
    pub(crate) fn with_retry_after(self, seconds: u32) -> Self {
        self.extra(|extra| extra.retry_after_s = Some(seconds))
    }

    /// Sets `suggested_granularity`: the granularity under which a refused metrics query fits.
    pub(crate) fn with_suggested_granularity(self, granularity: Option<String>) -> Self {
        self.extra(|extra| extra.suggested_granularity = granularity)
    }

    #[cfg(test)]
    pub(crate) const fn code(&self) -> ProblemCode {
        self.code
    }

    #[cfg(test)]
    pub(crate) fn errors(&self) -> &[FieldError] {
        &self.errors
    }

    pub(crate) fn with_limit(self, name: &str, value: u32) -> Self {
        self.extra(|extra| {
            extra.limit = Some(ProblemLimit {
                name: name.to_owned(),
                value,
            });
        })
    }

    /// A failed read. Store failures keep their details in the log: the detail of a problem is public.
    pub(crate) fn query(request_id: RequestId, error: &QueryServiceError) -> Self {
        let code = ProblemCode::parse(error.problem_code()).unwrap_or(ProblemCode::InternalError);
        match error {
            QueryServiceError::Validation(name) => Self::validation(
                request_id,
                vec![field_error(name, "invalid", "the value is out of range")],
            ),
            QueryServiceError::NotFound => Self::not_found(request_id),
            QueryServiceError::CursorInvalid => Self::new(
                request_id,
                code,
                "the cursor does not belong to this request; restart from the first page",
            ),
            QueryServiceError::Store(_) | QueryServiceError::Internal(_) => {
                if code == ProblemCode::InternalError {
                    tracing::error!(request_id = %request_id, error = %error, "a read failed");
                } else {
                    tracing::warn!(request_id = %request_id, error = %error, "a read failed");
                }
                Self::new(request_id, code, store_detail(code))
            }
        }
    }
}

fn store_detail(code: ProblemCode) -> &'static str {
    match code {
        ProblemCode::QueryTimeout => "the query exceeded its time limit",
        ProblemCode::QueryMemoryExceeded => "the query exceeded its memory limit",
        ProblemCode::QueryTooBroad => "the query reads too much; narrow the window",
        ProblemCode::TelemetryStoreUnavailable => {
            "the telemetry store is unavailable and the recent index cannot answer"
        }
        _ => "the read failed",
    }
}

/// A field error for the query or path parameter `name`; an empty name points at the whole request.
pub(crate) fn field_error(name: &str, code: &str, message: &str) -> FieldError {
    FieldError {
        pointer: if name.is_empty() {
            String::new()
        } else {
            format!("/{name}")
        },
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// "not_found" reads "Not found": one title per problem type, as RFC 9457 asks.
fn title(code: ProblemCode) -> String {
    let words = code.as_str().replace('_', " ");
    let mut chars = words.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_uppercase().to_string() + chars.as_str()
    })
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let extra = self.extra.map(|extra| *extra).unwrap_or_default();
        let body = Problem {
            problem_type: Problem::type_uri(self.code),
            title: title(self.code),
            status: status.as_u16(),
            detail: self.detail,
            code: self.code,
            request_id: self.request_id.to_string(),
            errors: (!self.errors.is_empty()).then_some(self.errors),
            retry_after_s: extra.retry_after_s,
            limit: extra.limit,
            suggested_granularity: extra.suggested_granularity,
        };
        // Serializing these plain fields cannot fail; an empty body would still carry the status.
        let json = serde_json::to_vec(&body).unwrap_or_default();
        let mut response = (status, [(CONTENT_TYPE, PROBLEM_JSON)], json).into_response();
        if let Some(value) = extra
            .retry_after_s
            .and_then(|seconds| HeaderValue::from_str(&seconds.to_string()).ok())
        {
            response.headers_mut().insert(RETRY_AFTER, value);
        }
        if self.code == ProblemCode::Unauthenticated {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::QueryError;

    use super::*;

    #[test]
    fn read_failures_map_to_their_codes_without_store_details() {
        let id = RequestId::from_bytes([1; 16]);
        let cases = [
            (QueryServiceError::NotFound, ProblemCode::NotFound),
            (QueryServiceError::CursorInvalid, ProblemCode::CursorInvalid),
            (
                QueryServiceError::Validation("limit"),
                ProblemCode::ValidationFailed,
            ),
            (
                QueryServiceError::Store(QueryError::Timeout),
                ProblemCode::QueryTimeout,
            ),
            (
                QueryServiceError::Store(QueryError::MemoryExceeded),
                ProblemCode::QueryMemoryExceeded,
            ),
            (
                QueryServiceError::Store(QueryError::TooBroad),
                ProblemCode::QueryTooBroad,
            ),
            (
                QueryServiceError::Store(QueryError::Unavailable("ch-3:9000 refused".to_owned())),
                ProblemCode::TelemetryStoreUnavailable,
            ),
            (
                QueryServiceError::Store(QueryError::Internal("SELECT 1 failed".to_owned())),
                ProblemCode::InternalError,
            ),
            (
                QueryServiceError::Internal("a row of the wrong shape".to_owned()),
                ProblemCode::InternalError,
            ),
        ];
        for (error, code) in cases {
            let problem = ApiError::query(id, &error);
            assert_eq!(problem.code, code, "{error:?}");
            for secret in ["ch-3", "SELECT", "wrong shape"] {
                assert!(!problem.detail.contains(secret), "{}", problem.detail);
            }
        }
        let validation = ApiError::query(id, &QueryServiceError::Validation("limit"));
        assert_eq!(validation.errors[0].pointer, "/limit");
    }

    #[test]
    fn a_problem_response_has_its_status_media_type_and_challenge() {
        let id = RequestId::from_bytes([2; 16]);
        let response = ApiError::unauthenticated(id).into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[CONTENT_TYPE], PROBLEM_JSON);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        let response = problem(id, ProblemCode::QueryTimeout, "slow");
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(!response.headers().contains_key(WWW_AUTHENTICATE));
    }

    #[test]
    fn titles_follow_the_code() {
        assert_eq!(title(ProblemCode::NotFound), "Not found");
        assert_eq!(
            title(ProblemCode::TelemetryStoreUnavailable),
            "Telemetry store unavailable"
        );
        assert_eq!(title(ProblemCode::InternalError), "Internal error");
    }
}
