//! What the server wrapper and the client wrapper share: trace context in `_meta`, the protocol era, the error
//! taxonomy of contract 9.2 and the hashes of contracts 9.4 and 9.6.

use std::collections::BTreeMap;

use hmac::{Hmac, KeyInit as _, Mac as _};
#[cfg(feature = "mcp")]
use opentelemetry::propagation::Extractor;
#[cfg(feature = "mcp-client")]
use opentelemetry::propagation::Injector;
use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use rmcp::model::{ContentBlock, JsonObject};
#[cfg(feature = "mcp")]
use sha2::Digest as _;
use sha2::Sha256;

use crate::keys::mcp as k;

/// The first protocol version of the modern era (contract 9.1).
pub(crate) const MODERN: &str = "2026-07-28";

/// How requests reach a server (`crucible.mcp.transport`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Transport {
    /// Not known.
    Unknown,
    /// Standard input and output.
    #[default]
    Stdio,
    /// Streamable HTTP.
    StreamableHttp,
    /// The HTTP with server-sent events transport of early protocol versions.
    Sse,
}

impl Transport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable_http",
            Self::Sse => "sse",
        }
    }
}

/// `mcp.protocol.version` and the era it implies (`crucible.mcp.era`).
pub(crate) fn era(version: &str) -> [KeyValue; 2] {
    let era = if is_modern(version) {
        "modern"
    } else {
        "legacy"
    };
    [
        KeyValue::new(k::PROTOCOL_VERSION, version.to_owned()),
        KeyValue::new(k::ERA, era),
    ]
}

/// Versions are ISO dates, which compare as strings.
pub(crate) fn is_modern(version: &str) -> bool {
    version >= MODERN
}

#[cfg(feature = "mcp")]
/// The trace context (`traceparent`, `tracestate`) a message's `_meta` carries, as the parent of an operation.
/// Baggage is never read.
pub(crate) fn extract(meta: &JsonObject) -> Context {
    TraceContextPropagator::new().extract_with_context(&Context::new(), &Meta(meta))
}

/// Writes the trace context of `cx` into a message's `_meta`.
#[cfg(feature = "mcp-client")]
pub(crate) fn inject(cx: &Context, meta: &mut JsonObject) {
    TraceContextPropagator::new().inject_context(cx, &mut MetaMut(meta));
}

#[cfg(feature = "mcp")]
struct Meta<'a>(&'a JsonObject);

#[cfg(feature = "mcp")]
impl Extractor for Meta<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(serde_json::Value::as_str)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

#[cfg(feature = "mcp-client")]
struct MetaMut<'a>(&'a mut JsonObject);

#[cfg(feature = "mcp-client")]
impl Injector for MetaMut<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0
            .insert(key.to_owned(), serde_json::Value::String(value));
    }
}

#[cfg(feature = "mcp")]
/// Contract 9.4: the first 16 bytes of SHA-256 of a raw session id, as 32 lowercase hex characters.
pub(crate) fn session_hash(raw: &str) -> String {
    hex16(&Sha256::digest(raw.as_bytes()))
}

/// Contract 9.6: the first 16 bytes of HMAC-SHA256 keyed with the sender's secret key over `crucible.mrtr.v1`,
/// one zero byte and the `requestState`, as 32 lowercase hex characters. The state itself is never sent.
pub(crate) fn state_hash(secret_key: &str, state: &str) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes()) else {
        // HMAC takes keys of any length.
        return String::new();
    };
    mac.update(b"crucible.mrtr.v1");
    mac.update(&[0]);
    mac.update(state.as_bytes());
    hex16(&mac.finalize().into_bytes())
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// What a handler annotated on its operation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Annotations {
    pub(crate) subclass: Option<&'static str>,
    pub(crate) validation: bool,
    pub(crate) not_found: bool,
}

/// How one operation ended, as far as a wrapper can observe it.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// `notification` or `stream_close` when the operation was cancelled.
    pub(crate) cancel: Option<&'static str>,
    /// The transport went away with the operation pending.
    pub(crate) transport: bool,
    /// The JSON-RPC error code of an error response.
    pub(crate) code: Option<i32>,
    /// The error message: the JSON-RPC error message, or the first text block of an `isError` result.
    pub(crate) message: String,
    /// A result with `isError: true`.
    pub(crate) is_error: bool,
    /// `complete`, `input_required` or `task` for a request that returned a result.
    pub(crate) result_type: Option<&'static str>,
    /// The `requestState` of an `input_required` result.
    pub(crate) state_out: Option<String>,
    /// The id of a task a request created.
    pub(crate) task_id: Option<String>,
    /// Content blocks of the result, by type.
    pub(crate) blocks: BTreeMap<&'static str, i64>,
}

impl Outcome {
    pub(crate) fn count_blocks(&mut self, content: &[ContentBlock]) {
        for block in content {
            let kind = match block {
                ContentBlock::Text(_) => "text",
                ContentBlock::Image(_) => "image",
                ContentBlock::Audio(_) => "audio",
                ContentBlock::Resource(_) => "resource",
                ContentBlock::ResourceLink(_) => "resource_link",
                _ => "other",
            };
            *self.blocks.entry(kind).or_default() += 1;
        }
    }

    /// Records a tool result's error flag, content and error text.
    pub(crate) fn tool_result(&mut self, result: &rmcp::model::CallToolResult) {
        self.result_type = Some("complete");
        self.count_blocks(&result.content);
        if result.is_error == Some(true) {
            self.is_error = true;
            self.message = result
                .content
                .iter()
                .find_map(|c| c.as_text().map(|t| t.text.clone()))
                .unwrap_or_default();
        }
    }

    /// The error class: the first matching row of the classifier table (contract 9.2) among those a wrapper
    /// can observe. HTTP statuses, timeouts and transport failures happen outside the wrapper.
    pub(crate) fn class(&self, annotations: Annotations) -> &'static str {
        if self.cancel.is_some() {
            return "cancelled";
        }
        if self.transport {
            return "transport";
        }
        if let Some(code) = self.code {
            return match code {
                -32700 | -32600 | -32601 | -32020 | -32021 | -32022 => "protocol",
                -32002 => "not_found",
                -32602 if annotations.not_found => "not_found",
                -32602 => "validation",
                _ => "internal",
            };
        }
        match (self.is_error, annotations.validation, annotations.not_found) {
            (false, _, _) => "none",
            (true, true, _) => "validation",
            (true, false, true) => "not_found",
            (true, false, false) => "tool_execution",
        }
    }

    /// The outcome keys of contract 9.1 and the error class.
    pub(crate) fn attributes(
        &self,
        annotations: Annotations,
        secret_key: &str,
    ) -> (Vec<KeyValue>, &'static str) {
        let class = self.class(annotations);
        let mut kv = Vec::new();
        if class != "none" {
            kv.push(KeyValue::new(k::ERROR_CLASS, class));
        }
        if let Some(source) = self.cancel {
            kv.push(KeyValue::new(k::CANCEL_SOURCE, source));
            return (kv, class);
        }
        if class == "tool_execution"
            && let Some(subclass) = annotations.subclass
        {
            kv.push(KeyValue::new(k::ERROR_SUBCLASS, subclass));
        }
        if let Some(code) = self.code {
            kv.push(KeyValue::new(k::ERROR_TYPE, code.to_string()));
            kv.push(KeyValue::new(k::RPC_STATUS_CODE, code.to_string()));
        } else if self.is_error {
            kv.push(KeyValue::new(k::ERROR_TYPE, "tool_error"));
        }
        if let Some(result_type) = self.result_type {
            kv.push(KeyValue::new(k::RESULT_TYPE, result_type));
        }
        if let Some(state) = &self.state_out {
            kv.push(KeyValue::new(k::STATE_OUT, state_hash(secret_key, state)));
        }
        if let Some(task) = &self.task_id {
            kv.push(KeyValue::new(k::TASK_ID, task.clone()));
        }
        for (kind, count) in &self.blocks {
            kv.push(KeyValue::new(
                format!("{}{kind}", k::CONTENT_BLOCKS_PREFIX),
                *count,
            ));
        }
        (kv, class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "mcp")]
    #[test]
    fn session_hashes_match_the_registry_vectors() {
        assert_eq!(
            session_hash("3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5"),
            "84757436d5e0791bd4a8eaa6f1d05986"
        );
        assert_eq!(
            session_hash("conformance-session"),
            "b1427bda7462603423a9e4a7b73fbdd0"
        );
    }

    #[test]
    fn state_hashes_match_the_registry_vectors() {
        let key = "csk_conformance00000000000000000000000000000000";
        assert_eq!(
            state_hash(key, "crucible-conformance-request-state-1"),
            "59c50f1c773bc1354d420d974c06c5af"
        );
        assert_eq!(
            state_hash(key, "eyJzdGVwIjoiY29uZmlybWVkIn0.c2ln"),
            "bec039d76aab792bd526d369321fb0c0"
        );
    }

    #[test]
    fn classes_follow_the_taxonomy_table() {
        let none = Annotations::default();
        let class = |out: Outcome, a: Annotations| out.class(a);
        let code = |c: i32| Outcome {
            code: Some(c),
            ..Outcome::default()
        };
        for c in [-32700, -32600, -32601, -32020, -32021, -32022] {
            assert_eq!(class(code(c), none), "protocol");
        }
        assert_eq!(class(code(-32002), none), "not_found");
        assert_eq!(class(code(-32602), none), "validation");
        let not_found = Annotations {
            not_found: true,
            ..none
        };
        assert_eq!(class(code(-32602), not_found), "not_found");
        assert_eq!(class(code(-32603), none), "internal");
        assert_eq!(class(code(-1), none), "internal");
        let tool_error = || Outcome {
            is_error: true,
            ..Outcome::default()
        };
        assert_eq!(class(tool_error(), none), "tool_execution");
        assert_eq!(class(tool_error(), not_found), "not_found");
        let validation = Annotations {
            validation: true,
            ..none
        };
        assert_eq!(class(tool_error(), validation), "validation");
        let cancelled = Outcome {
            cancel: Some("notification"),
            code: Some(-32603),
            ..Outcome::default()
        };
        assert_eq!(class(cancelled, none), "cancelled");
        assert_eq!(class(Outcome::default(), none), "none");
    }

    #[test]
    fn eras_split_at_the_modern_version() {
        assert!(!is_modern("2025-11-25"));
        assert!(is_modern("2026-07-28"));
        assert!(is_modern("2027-01-01"));
    }
}
