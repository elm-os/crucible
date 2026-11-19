//! The MCP operation of a span as the keys its projector reads (contract 9.1). The projector removes those keys
//! from the residual `attributes`, so the line carries them again, from the stored `mcp_ops` and `mcp_content`
//! rows: re-ingested, the span projects to the same operation. A field that a default rule would derive goes out
//! when it differs from that default, and the derived fields (era, result type, identity source, whether content
//! was captured) go out always, so that no default decides them a second time. The error class goes out as the
//! explicit class: the classifier keeps an explicit class that fits the status it derives from the same inputs,
//! and the stored class always does.
//!
//! Two things do not come back. The user subject is never stored, only its keyed hash, which cannot be sent as a
//! subject. A tool list the operation reported (contract 3.6) belongs to the tool inventory, not to the
//! operation.

use crucible_plan::Value;
use crucible_query::{ExportColumn, ExportMcp};
use crucible_records::{SpanCore, SpanKind, mcp_keys as keys};
use crucible_schema::Ty;

use super::Attrs;
use crate::json::{self, Json};

/// The columns of one stored row, by name. Every reader is total: a column that is missing or holds another
/// type reads as absent.
struct Row<'a>(&'a [ExportColumn]);

impl<'a> Row<'a> {
    fn value(&self, name: &str) -> Option<&'a Value> {
        self.0.iter().find(|c| c.name == name).map(|c| &c.value)
    }

    fn text(&self, name: &str) -> String {
        match self.value(name) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        }
    }

    /// The variant name of an `Enum8` column.
    fn variant(&self, name: &str) -> &'static str {
        match self.0.iter().find(|c| c.name == name) {
            Some(ExportColumn {
                ty: Ty::Enum8(kind),
                value: Value::Enum(v),
                ..
            }) => kind
                .variants()
                .into_iter()
                .find(|(_, n)| n == v)
                .map_or("", |(name, _)| name),
            _ => "",
        }
    }

    fn uint(&self, name: &str) -> Option<u64> {
        match self.value(name) {
            Some(Value::UInt(v)) => Some(*v),
            _ => None,
        }
    }

    fn int(&self, name: &str) -> Option<i64> {
        match self.value(name) {
            Some(Value::Int(v)) => Some(*v),
            Some(Value::UInt(v)) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    fn flag(&self, name: &str) -> bool {
        self.uint(name).is_some_and(|v| v != 0)
    }

    /// A `FixedString(16)` as lowercase hexadecimal; empty when all zero (the stored form of an absent value).
    fn hex(&self, name: &str) -> String {
        match self.value(name) {
            Some(Value::Bytes(b)) if b.iter().any(|x| *x != 0) => {
                let mut out = Vec::with_capacity(b.len() * 2);
                json::hex(&mut out, b);
                String::from_utf8_lossy(&out).into_owned()
            }
            _ => String::new(),
        }
    }
}

/// The attributes being written, and the keys of the residual that follows them.
pub(super) struct Line<'a> {
    pub(super) attrs: &'a mut Attrs,
    pub(super) residual: &'a [String],
}

impl Line<'_> {
    /// Appends one attribute unless the line already carries the key, so that no key appears twice: a key the
    /// projector shares with the span mapping (the tool name, `user.id`, the call's arguments and result) may
    /// be on the line already.
    fn put(&mut self, key: &str, value: Json) {
        if !self.attrs.has(key) && !self.residual.iter().any(|k| k == key) {
            self.attrs.put(key, &value);
        }
    }

    fn text(&mut self, key: &str, value: String) {
        if !value.is_empty() {
            self.put(key, Json::Str(value));
        }
    }

    /// A variant unless it is the default the projector applies when the key is absent.
    fn variant(&mut self, key: &str, value: &str, default: &str) {
        if value != default {
            self.text(key, value.to_owned());
        }
    }

    fn int(&mut self, key: &str, value: Option<i64>) {
        if let Some(value) = value {
            self.put(key, Json::Int(value));
        }
    }
}

/// A text column of the operation's `mcp_content` row.
pub(super) fn content(row: &[ExportColumn], column: &str) -> String {
    Row(row).text(column)
}

/// The operation's `user_id`.
pub(super) fn user_id(mcp: &ExportMcp) -> String {
    Row(&mcp.op).text("user_id")
}

/// Whether the operation's captured content was cut (contract 3.5), which makes the span one with a cut field.
pub(super) fn content_cut(mcp: &ExportMcp) -> bool {
    Row(&mcp.op).variant("content_truncation") == "cap_exceeded"
}

/// Appends the keys of the operation.
pub(super) fn attributes(line: &mut Line<'_>, core: &SpanCore, mcp: &ExportMcp) {
    let op = Row(&mcp.op);
    line.text(keys::METHOD, op.text("method"));
    if !matches!(core.kind, SpanKind::Server | SpanKind::Client) {
        line.variant(keys::SIDE, op.variant("side"), "none");
    }
    line.variant(keys::CAPTURE_SOURCE, op.variant("capture_source"), "otlp");
    line.text(keys::ERA, op.variant("era").to_owned());
    line.text(keys::PROTOCOL_VERSION, op.text("protocol_version"));
    line.variant(keys::TRANSPORT, op.variant("transport"), "unknown");
    let target = match op.variant("target_kind") {
        "tool" => Some(keys::TOOL_NAME),
        "prompt" => Some(keys::PROMPT_NAME),
        "resource" => Some(keys::RESOURCE_TEMPLATE),
        _ => None,
    };
    if let Some(key) = target {
        line.text(key, op.text("target_name"));
    }
    line.text(keys::JSONRPC_ID, op.text("jsonrpc_id"));
    line.variant(keys::DIRECTION, op.variant("direction"), "client_to_server");
    for (key, column) in [
        (keys::IS_NOTIFICATION, "is_notification"),
        (keys::IS_BATCH, "is_batch"),
        (keys::TIMED_OUT, "timed_out"),
    ] {
        if op.flag(column) {
            line.put(key, Json::Bool(true));
        }
    }

    // The classifier's inputs, then the class it gave (contract 9.2).
    line.text(keys::ERROR_TYPE, op.text("error_type"));
    line.int(keys::RPC_STATUS_CODE, op.int("jsonrpc_code"));
    line.int(keys::HTTP_STATUS, op.int("http_status"));
    line.variant(keys::CANCEL_SOURCE, op.variant("cancel_source"), "none");
    line.variant(keys::ERROR_CLASS, op.variant("error_class"), "none");
    line.variant(keys::ERROR_SUBCLASS, op.variant("error_subclass"), "none");
    line.text(keys::RESULT_TYPE, op.variant("result_type").to_owned());

    line.int(keys::MRTR_ROUND, op.int("mrtr_round").filter(|n| *n != 0));
    line.text(keys::MRTR_STATE_IN, op.hex("mrtr_state_in"));
    line.text(keys::MRTR_STATE_OUT, op.hex("mrtr_state_out"));
    line.text(keys::TASK_ID, op.text("task_id"));
    line.text(keys::SUBSCRIPTION_ID, op.text("subscription_id"));
    line.int(keys::PROGRESS_COUNT, op.int("progress_count"));

    // The client and the identity.
    line.text(keys::CLIENT_NAME, op.text("client_name"));
    line.text(keys::CLIENT_VERSION, op.text("client_version"));
    if let Some(hash) = op.uint("client_capabilities_hash").filter(|h| *h != 0) {
        // The projector reads an integer as its bits.
        line.put(
            keys::CLIENT_CAPABILITIES_HASH,
            Json::Int(i64::from_ne_bytes(hash.to_ne_bytes())),
        );
    }
    line.text(keys::OAUTH_CLIENT_ID, op.text("oauth_client_id"));
    line.text(keys::TENANT, op.text("tenant"));
    line.text(keys::USER_ID, op.text("user_id"));
    line.text(keys::HANDLE_ID, op.text("handle_id"));
    line.text(keys::USER_ISSUER, op.text("user_issuer"));
    line.text(
        keys::IDENTITY_SOURCE,
        op.variant("identity_source").to_owned(),
    );
    // The stored session key: the 32 lowercase hexadecimal characters the projector takes as already hashed.
    line.text(keys::SESSION_ID, op.text("session_key"));

    // Sizes and content.
    line.int(
        keys::REQUEST_BYTES,
        op.int("request_bytes").filter(|n| *n != 0),
    );
    line.int(
        keys::RESPONSE_BYTES,
        op.int("response_bytes").filter(|n| *n != 0),
    );
    if let (Some(Value::Array(types)), Some(Value::Array(counts))) = (
        op.value("content_block_types"),
        op.value("content_block_counts"),
    ) {
        for (kind, count) in types.iter().zip(counts) {
            let kind = match kind {
                Value::String(s) => s.clone(),
                Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                _ => continue,
            };
            if let (false, Value::UInt(count)) = (kind.is_empty(), count) {
                let count = i64::try_from(*count).unwrap_or(i64::MAX);
                line.put(
                    &format!("{}{kind}", keys::CONTENT_BLOCKS_PREFIX),
                    Json::Int(count),
                );
            }
        }
    }
    let captured = op.flag("content_captured");
    line.put(keys::CONTENT_CAPTURED, Json::Bool(captured));
    if let Some(Value::Float(rate)) = op.value("content_sample_rate") {
        // Without the key the rate is 1 when content was captured, else 0.
        let default = if captured { 1.0 } else { 0.0 };
        if *rate != default {
            line.put(keys::CONTENT_SAMPLE_RATE, Json::Float(*rate));
        }
    }
    line.variant(
        keys::CONTENT_TRUNCATION,
        op.variant("content_truncation"),
        "none",
    );
    if let Some(content) = &mcp.content {
        let content = Row(content);
        line.text(keys::TOOL_ARGUMENTS, content.text("args"));
        line.text(keys::TOOL_RESULT, content.text("result"));
    }

    // The server and the client's connection.
    line.text(keys::SERVER_NAME, op.text("server_name"));
    line.text(keys::SERVER_VERSION, op.text("server_version"));
    line.text(keys::CLIENT_IP_PREFIX, op.text("ip_prefix"));
    line.text(keys::USER_AGENT, op.text("user_agent"));
}
