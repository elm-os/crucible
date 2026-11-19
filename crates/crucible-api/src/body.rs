//! JSON request bodies (contract 1.3 and 12.3): a body must be JSON, unknown fields are refused, and every
//! problem names its part of the body with a JSON pointer.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use crucible_api_types::{FieldError, ProblemCode};
use crucible_records::RequestId;
use serde::de::DeserializeOwned;
use serde_path_to_error::Segment;

use crate::problem::ApiError;

/// The JSON pointer of a path inside the body.
fn pointer(path: &serde_path_to_error::Path) -> String {
    path.iter()
        .filter_map(|segment| match segment {
            Segment::Seq { index } => Some(index.to_string()),
            Segment::Map { key } => Some(key.replace('~', "~0").replace('/', "~1")),
            Segment::Enum { variant } => Some(variant.clone()),
            Segment::Unknown => None,
        })
        .map(|segment| format!("/{segment}"))
        .collect()
}

fn field_error(pointer: &str, code: &str, message: &str) -> FieldError {
    FieldError {
        pointer: pointer.to_owned(),
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// The field named between the first pair of backticks of a serde message.
fn quoted(message: &str) -> Option<&str> {
    let start = message.find('`')? + 1;
    let len = message[start..].find('`')?;
    Some(&message[start..start + len])
}

/// The body as `T`. A body that is not JSON is `unsupported_media_type`; a body that does not parse, or that
/// has a field `T` does not know, is `validation_failed` with the pointer of the part to change.
pub(crate) fn json<T: DeserializeOwned>(
    request_id: RequestId,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<T, ApiError> {
    let json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(ApiError::new(
            request_id,
            ProblemCode::UnsupportedMediaType,
            "send the body as JSON with `Content-Type: application/json`",
        ));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let trailing = || {
        let error = field_error("", "invalid_json", "the body is not valid JSON");
        ApiError::validation(request_id, vec![error])
    };
    let parsed = serde_path_to_error::deserialize(&mut deserializer);
    let value = parsed.map_err(|error| {
        let at = pointer(error.path());
        let inner = error.inner();
        let message = inner.to_string();
        let field = |code: &str| {
            let at = match quoted(&message) {
                Some(name) => format!("{at}/{name}"),
                None => at.clone(),
            };
            field_error(&at, code, &message)
        };
        let error = if inner.is_syntax() || inner.is_eof() {
            field_error("", "invalid_json", "the body is not valid JSON")
        } else if message.starts_with("unknown field") {
            // The path already ends at the unknown key.
            field_error(&at, "unknown_field", &message)
        } else if message.starts_with("missing field") {
            field("required")
        } else {
            field_error(&at, "invalid", &message)
        };
        ApiError::validation(request_id, vec![error])
    })?;
    deserializer.end().map_err(|_| trailing())?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Body {
        name: String,
        items: Vec<Item>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Item {
        count: u32,
    }

    fn parse(text: &str) -> Result<Body, ApiError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        json(
            RequestId::from_bytes([1; 16]),
            &headers,
            &Bytes::from(text.to_owned()),
        )
    }

    fn first(error: ApiError) -> (String, String) {
        let e = &error.errors()[0];
        (e.pointer.clone(), e.code.clone())
    }

    #[test]
    fn problems_point_at_the_part_to_change() {
        assert!(parse(r#"{"name":"a","items":[{"count":1}]}"#).is_ok());
        let cases = [
            (
                r#"{"name":"a","items":[{"count":-1}]}"#,
                "/items/0/count",
                "invalid",
            ),
            (
                r#"{"name":"a","items":[],"extra":1}"#,
                "/extra",
                "unknown_field",
            ),
            (
                r#"{"name":"a","items":[{"count":1,"x":2}]}"#,
                "/items/0/x",
                "unknown_field",
            ),
            (r#"{"items":[]}"#, "/name", "required"),
            (r#"{"name":"a","items":[]"#, "", "invalid_json"),
            (r#"{"name":"a","items":[]} trailing"#, "", "invalid_json"),
        ];
        for (text, pointer, code) in cases {
            let error = parse(text).unwrap_err();
            assert_eq!(
                first(error),
                (pointer.to_owned(), code.to_owned()),
                "{text}"
            );
        }
        let error = json::<Body>(
            RequestId::from_bytes([1; 16]),
            &HeaderMap::new(),
            &Bytes::new(),
        );
        assert_eq!(error.unwrap_err().code(), ProblemCode::UnsupportedMediaType);
    }
}
