//! Tool errors (contract 12.3): a failure the caller can act on is a tool result with `isError`, whose text is a
//! problem document with the stable code, what to change, and the request id. The text is the whole error: a
//! client that checks `structuredContent` against the tool's output schema finds none on an error.

use std::time::Duration;

use crucible_api_types::{FieldError, Problem, ProblemCode, ProblemLimit};
use crucible_cursor::CursorError;
use crucible_query::QueryServiceError;
use crucible_records::RequestId;
use rmcp::model::{CallToolResult, ContentBlock};

/// A failed tool call.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct ToolError {
    code: ProblemCode,
    detail: String,
    errors: Vec<FieldError>,
    limit: Option<ProblemLimit>,
    retry_after_s: Option<u32>,
}

/// A field error for the argument `name`; an empty name points at the whole argument object.
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

impl ToolError {
    pub(crate) fn new(code: ProblemCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            errors: Vec::new(),
            limit: None,
            retry_after_s: None,
        }
    }

    /// `validation_failed` with the invalid arguments.
    pub(crate) fn validation(errors: Vec<FieldError>) -> Self {
        Self {
            errors,
            ..Self::new(
                ProblemCode::ValidationFailed,
                "the arguments are invalid; see `errors`, whose pointers name the arguments",
            )
        }
    }

    /// Arguments that do not deserialize: a missing or unknown argument, or a value of the wrong type.
    pub(crate) fn arguments(error: &serde_json::Error) -> Self {
        Self::validation(vec![field_error(
            "",
            "invalid_arguments",
            &error.to_string(),
        )])
    }

    pub(crate) fn with_limit(self, name: &str, value: u32) -> Self {
        Self {
            limit: Some(ProblemLimit {
                name: name.to_owned(),
                value,
            }),
            ..self
        }
    }

    /// Too many calls for the key: wait `wait` and call again.
    pub(crate) fn rate_limited(wait: Duration) -> Self {
        let seconds = u32::try_from(wait.as_secs()).unwrap_or(u32::MAX);
        let seconds = if wait.subsec_nanos() > 0 {
            seconds.saturating_add(1)
        } else {
            seconds.max(1)
        };
        Self {
            retry_after_s: Some(seconds),
            ..Self::new(
                ProblemCode::RateLimited,
                format!(
                    "this key made more tool calls than the server allows; call again in {seconds} s, or ask \
                     the operator to raise the MCP rate limit"
                ),
            )
        }
    }

    /// A cursor that does not open for this call.
    pub(crate) fn cursor(error: CursorError) -> Self {
        Self::new(
            ProblemCode::CursorInvalid,
            format!(
                "{error}: a cursor continues only the call that returned it, with the same arguments, \
                 for 24 hours; call again without `cursor` to start over"
            ),
        )
    }

    /// A failed read. Store failures keep their details in the log: a problem's detail is public.
    pub(crate) fn query(error: &QueryServiceError, request_id: RequestId) -> Self {
        let code = ProblemCode::parse(error.problem_code()).unwrap_or(ProblemCode::InternalError);
        match error {
            QueryServiceError::Validation(name) => Self::validation(vec![field_error(
                name,
                "out_of_range",
                "the value is out of range",
            )]),
            QueryServiceError::NotFound => Self::new(
                code,
                "the project has no trace or span with this id: check the id, or it may have expired or \
                 not arrived yet",
            ),
            QueryServiceError::CursorInvalid => Self::cursor(CursorError::FiltersChanged),
            QueryServiceError::Store(_) | QueryServiceError::Internal(_) => {
                if code == ProblemCode::InternalError {
                    tracing::error!(request_id = %request_id, error = %error, "an MCP read failed");
                } else {
                    tracing::warn!(request_id = %request_id, error = %error, "an MCP read failed");
                }
                Self::new(code, store_detail(code))
            }
        }
    }

    /// The problem document of the error, as the text of a tool result with `isError`.
    pub(crate) fn into_result(self, request_id: RequestId) -> CallToolResult {
        let problem = Problem {
            problem_type: Problem::type_uri(self.code),
            title: title(self.code),
            status: self.code.status(),
            detail: self.detail,
            code: self.code,
            request_id: request_id.to_string(),
            errors: (!self.errors.is_empty()).then_some(self.errors),
            retry_after_s: self.retry_after_s,
            limit: self.limit,
            suggested_granularity: None,
        };
        // Serializing these plain fields cannot fail; the code would still be in the empty text's place.
        let text = serde_json::to_string(&problem).unwrap_or_default();
        CallToolResult::error(vec![ContentBlock::text(text)])
    }
}

fn store_detail(code: ProblemCode) -> &'static str {
    match code {
        ProblemCode::QueryTimeout => {
            "the query exceeded the time limit of agent reads; narrow the window or lower `limit`"
        }
        ProblemCode::QueryMemoryExceeded => {
            "the query exceeded the memory limit of agent reads; narrow the window or lower `limit`"
        }
        ProblemCode::QueryTooBroad => {
            "the query reads more rows than agent reads may; narrow the window"
        }
        ProblemCode::TelemetryStoreUnavailable => {
            "the telemetry store is unavailable and the recent index cannot answer; retry shortly"
        }
        _ => "the read failed; the request id finds it in the server log",
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

#[cfg(test)]
mod tests {
    use crucible_env::QueryError;

    use super::*;

    fn problem(error: ToolError) -> serde_json::Value {
        let result = error.into_result(RequestId::from_bytes([3; 16]));
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
        let text = &result.content[0].as_text().unwrap().text;
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn read_failures_keep_their_code_and_leave_store_details_out() {
        let id = RequestId::from_bytes([1; 16]);
        for (error, code) in [
            (QueryServiceError::NotFound, "not_found"),
            (QueryServiceError::CursorInvalid, "cursor_invalid"),
            (QueryServiceError::Validation("limit"), "validation_failed"),
            (
                QueryServiceError::Store(QueryError::Timeout),
                "query_timeout",
            ),
            (
                QueryServiceError::Store(QueryError::TooBroad),
                "query_too_broad",
            ),
            (
                QueryServiceError::Store(QueryError::MemoryExceeded),
                "query_memory_exceeded",
            ),
            (
                QueryServiceError::Store(QueryError::Unavailable("ch-3:9000 refused".to_owned())),
                "telemetry_store_unavailable",
            ),
            (
                QueryServiceError::Internal("SELECT 1 failed".to_owned()),
                "internal_error",
            ),
        ] {
            let body = problem(ToolError::query(&error, id));
            assert_eq!(body["code"], code, "{body}");
            assert_eq!(body["type"], format!("urn:crucible:problem:{code}"));
            let text = body.to_string();
            assert!(!text.contains("ch-3") && !text.contains("SELECT"), "{text}");
        }
    }

    #[test]
    fn a_rate_limit_says_when_to_call_again() {
        for (wait, seconds) in [
            (Duration::from_millis(1), 1),
            (Duration::from_secs(2), 2),
            (Duration::from_millis(2_500), 3),
            (Duration::ZERO, 1),
        ] {
            let body = problem(ToolError::rate_limited(wait));
            assert_eq!(body["code"], "rate_limited");
            assert_eq!(body["status"], 429);
            assert_eq!(body["retry_after_s"], seconds, "{wait:?}");
        }
    }

    #[test]
    fn titles_follow_the_code() {
        assert_eq!(title(ProblemCode::NotFound), "Not found");
        assert_eq!(title(ProblemCode::RateLimited), "Rate limited");
    }
}
