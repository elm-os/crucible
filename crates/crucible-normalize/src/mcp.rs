//! The MCP projector (contracts 3.5, 3.6 and 9.1 to 9.8): from a mapped span to its [`McpOpRecord`] and the tool
//! inventory entries it reports. Pure, like the rest of stage A.
//!
//! A span is an MCP operation if and only if it carries `mcp.method.name`. The keys are those of
//! [`crucible_records::mcp_keys`]; everything else about the operation comes from the span itself (ids, parent,
//! times, the status message, the first link) and from its resource.
//!
//! Readings of the contract, where it leaves a choice:
//!
//! - **Classifier inputs** (9.2). The JSON-RPC code is `rpc.response.status_code`, else an `error.type` that is
//!   a number other than the HTTP status (a proxy reports an HTTP failure with the status as its `error.type`).
//!   `error.type = tool_error` is `isError`. Any other `error.type` that is not a number is an unhandled
//!   exception. No key says that the transport failed or that a deadline passed without `crucible.mcp.timed_out`:
//!   only the capture source can observe either, so an explicit `crucible.mcp.error.class` of `transport` or
//!   `timeout` is that observation. The `mark_validation` and `mark_not_found` annotations reach the classifier
//!   as the explicit class.
//! - **Target** (9.1). The kind comes from the method (`tools/call`, `prompts/get`, the `resources/` methods that
//!   name one resource) and the name from the key of that kind.
//! - **Content** (9.10). Without `crucible.mcp.content.captured`, content counts as captured when arguments or a
//!   result are there, so that a plain semantic-convention span keeps them. A sender that says `false` is taken
//!   at its word.
//! - **Tool inventory** (3.6). The contract names no key. `crucible.mcp.tools` holds the tools of a
//!   `tools/list` or `server/discover` result, or of an SDK's snapshot, as the JSON array the protocol sends (or
//!   as an attribute array of key-value lists). The schema hash is computed here, so that one implementation
//!   decides what canonical JSON is: xxh3-64 of the input schema, one zero byte and the output schema, each
//!   rendered compactly with object members in ascending byte order of their names, an absent schema as `null`.
//! - **Session key** (9.4). A session id is read in the legacy era only. A value of 32 lowercase hexadecimal
//!   characters is the key; anything else is hashed, so a raw session id is never stored.

use core::fmt;

use bytes::Bytes;
use crucible_otlp::{AnyValueView, SpanView};
use crucible_records::{
    CancelSource, CaptureSource, Direction, Era, ErrorClass, ErrorSubclass, GroupKeyKind,
    IdentitySource, McpErrorAnnotations, McpErrorInput, McpOpRecord, McpSide, McpTargetKind,
    McpToolInventoryRecord, ResultType, SpanCore, SpanId, SpanKind, TraceId, Transport, Truncation,
    caps, classify_mcp_error, mcp_keys as keys, truncate_str,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use xxhash_rust::xxh3::xxh3_64;

use crate::json;
use crate::node::Node;

/// Bytes kept of a content block type, the suffix of `crucible.mcp.content.blocks.<type>`. The contract caps the
/// number of types, not their length (default; revisit).
const CONTENT_BLOCK_TYPE_BYTES: usize = 64;

/// A seeded defect of the normalizer. The simulation suite switches each one on and must catch it.
///
/// Without the cargo feature `test-mutants` this type has no value, so no defect can be switched on. Only
/// `crucible-sim` enables the feature, through `crucible-shipper`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// Report an invalid-params error on a server-side row as the server's fault (contract 9.2).
    #[cfg(feature = "test-mutants")]
    MisclassifyCallerFault,
    /// Flag the usage aggregates of a frame after its exact repeats were dropped (contract 5.3).
    #[cfg(feature = "test-mutants")]
    MarkAfterDuplicateDrop,
}

/// The salt of a project (contract 9.3): the key of the keyed hash of a user subject. It is random, immutable for
/// the life of the project and never logged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProjectSalt([u8; 32]);

impl ProjectSalt {
    /// The salt with these bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The bytes of the salt.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// `user_subject_hash` (contract 9.3): the first 16 bytes of HMAC-SHA256 keyed with the salt over the
    /// issuer, one zero byte and the subject.
    pub fn subject_hash(&self, issuer: &str, subject: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        // HMAC takes keys of any length, so this never fails.
        if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.0) {
            mac.update(issuer.as_bytes());
            mac.update(&[0]);
            mac.update(subject.as_bytes());
            out.copy_from_slice(&mac.finalize().into_bytes()[..16]);
        }
        out
    }
}

impl fmt::Debug for ProjectSalt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProjectSalt(..)")
    }
}

/// What stage B knows about the identity of a frame's project (contracts 3.2 and 9.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdentityCtx<'a> {
    /// No control store holds project salts (ingest-only mode). Stage B drops the identity keys of every MCP
    /// operation, as `identity: none` in the sender would have: the user id, the subject and its issuer, the
    /// tenant, the OAuth client id and the identity source.
    Suppressed,
    /// The salt of the frame's project. `None` when the frame carries no user subject, so that none was
    /// fetched; an operation that carries one all the same loses the subject and its issuer.
    Salted(Option<&'a ProjectSalt>),
}

/// The draft of an MCP operation (contract 10.2): the record, and the raw user subject that stage B hashes and
/// discards. The subject is not part of the record, cannot be read outside this crate and is not printed, so it
/// is never stored or logged.
#[derive(Clone, PartialEq)]
pub struct McpOpDraft {
    /// The record. `user_subject_hash` is `None` until stage B.
    pub record: McpOpRecord,
    pub(crate) subject: Box<str>,
}

impl fmt::Debug for McpOpDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpOpDraft")
            .field("record", &self.record)
            .field("has_subject", &self.has_subject())
            .finish()
    }
}

impl McpOpDraft {
    /// Whether the operation carries a user subject, which stage B needs the project's salt for.
    pub fn has_subject(&self) -> bool {
        !self.subject.is_empty()
    }
}

/// The draft of a tool inventory entry (contract 10.2).
pub type InventoryDraft = McpToolInventoryRecord;

/// One MCP operation of a frame. Before stage B it also holds the raw user subject, which cannot be read outside
/// this crate and is not printed.
#[derive(Clone, PartialEq)]
pub struct McpOp {
    /// The position of the operation's span in [`crate::NormalizedFrame::spans`].
    pub span: u32,
    /// The record.
    pub record: McpOpRecord,
    pub(crate) subject: Box<str>,
}

impl fmt::Debug for McpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpOp")
            .field("span", &self.span)
            .field("record", &self.record)
            .field("has_subject", &self.has_subject())
            .finish()
    }
}

impl McpOp {
    /// Whether the operation still carries a user subject for stage B to hash.
    pub fn has_subject(&self) -> bool {
        !self.subject.is_empty()
    }

    /// Stage B's part of the record (contract 9.3): hashes the subject with the project's salt and discards
    /// it, or drops the identity keys when there is no control store.
    pub(crate) fn seal_identity(&mut self, identity: &IdentityCtx<'_>) {
        let subject = core::mem::take(&mut self.subject);
        let r = &mut self.record;
        match identity {
            IdentityCtx::Suppressed => {
                r.user_subject_hash = None;
                r.user_issuer = "".into();
                r.user_id = "".into();
                r.tenant = "".into();
                r.oauth_client_id = "".into();
                r.identity_source = IdentitySource::None;
            }
            IdentityCtx::Salted(Some(salt)) if !subject.is_empty() => {
                r.user_subject_hash = Some(salt.subject_hash(&r.user_issuer, &subject));
            }
            IdentityCtx::Salted(_) => {
                if !subject.is_empty() {
                    // No salt was fetched for a subject: the issuer alone says nothing.
                    r.user_issuer = "".into();
                }
                r.user_subject_hash = None;
            }
        }
    }
}

/// One tool inventory entry of a frame.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpInventory {
    /// The position of the reporting span in [`crate::NormalizedFrame::spans`].
    pub span: u32,
    /// The record.
    pub record: McpToolInventoryRecord,
}

fn text(s: &str, cap: usize) -> Box<str> {
    truncate_str(s, cap).into()
}

fn string<'a>(value: &AnyValueView<'a>) -> Option<&'a str> {
    match value {
        AnyValueView::String(s) => Some(s),
        _ => None,
    }
}

/// A string, or an integer as its decimal text.
fn string_or_int(value: &AnyValueView<'_>, cap: usize) -> Box<str> {
    match value {
        AnyValueView::String(s) => text(s, cap),
        AnyValueView::Int(i) => i.to_string().into(),
        _ => "".into(),
    }
}

fn int(value: &AnyValueView<'_>) -> Option<i64> {
    match value {
        AnyValueView::Int(i) => Some(*i),
        AnyValueView::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn unsigned(value: &AnyValueView<'_>) -> Option<u64> {
    int(value).map(|i| u64::try_from(i).unwrap_or(0))
}

fn flag(value: &AnyValueView<'_>) -> Option<bool> {
    match value {
        AnyValueView::Bool(b) => Some(*b),
        AnyValueView::String("true") => Some(true),
        AnyValueView::String("false") => Some(false),
        _ => None,
    }
}

/// 32 hexadecimal characters as 16 bytes.
fn hex16(s: &str) -> Option<[u8; 16]> {
    let bytes = s.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 16];
    for (byte, [high, low]) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *byte = nibble(*high)? << 4 | nibble(*low)?;
    }
    Some(out)
}

/// `session_key` (contract 9.4): the value when it is already the 32 lowercase hexadecimal characters of a
/// hashed session id, else the first 16 bytes of SHA-256 of the raw value.
fn session_key(raw: &str) -> Option<[u8; 16]> {
    if raw.is_empty() {
        return None;
    }
    let hashed = raw.len() == 32 && raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if hashed {
        return hex16(raw);
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&Sha256::digest(raw.as_bytes())[..16]);
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    json::push_hex(&mut out, bytes);
    String::from_utf8(out).unwrap_or_default()
}

/// The client capabilities hash: an integer (its bits), or its decimal or 16-digit hexadecimal text.
fn capabilities_hash(value: &AnyValueView<'_>) -> u64 {
    match value {
        AnyValueView::Int(i) => *i as u64,
        AnyValueView::String(s) => s
            .parse::<u64>()
            .ok()
            .or_else(|| {
                (s.len() == 16)
                    .then(|| u64::from_str_radix(s, 16).ok())
                    .flatten()
            })
            .unwrap_or(0),
        _ => 0,
    }
}

/// A content body: a string as it is, any other value as its JSON text.
fn body(value: AnyValueView<'_>) -> Bytes {
    match value {
        AnyValueView::String(s) => Bytes::copy_from_slice(s.as_bytes()),
        AnyValueView::Empty => Bytes::new(),
        other => json::value(other),
    }
}

/// The target kind of a method (contract 9.1).
fn target_kind(method: &str) -> McpTargetKind {
    match method {
        "tools/call" => McpTargetKind::Tool,
        "prompts/get" => McpTargetKind::Prompt,
        "resources/read" | "resources/subscribe" | "resources/unsubscribe" => {
            McpTargetKind::Resource
        }
        _ => McpTargetKind::None,
    }
}

/// Who served the operation: the server's name, version, deploy and instance (contracts 9.1 and 9.7), and the
/// capture source.
struct Server {
    name: Box<str>,
    version: Box<str>,
    deploy_id: Box<str>,
    instance_id: Box<str>,
    capture_source: CaptureSource,
}

fn server(span: &SpanView<'_>) -> Server {
    let (mut name, mut version) = (None, None);
    let mut capture_source = CaptureSource::Otlp;
    for (key, value) in span.attributes() {
        match key {
            keys::SERVER_NAME => name = string(&value),
            keys::SERVER_VERSION => version = string(&value),
            keys::CAPTURE_SOURCE => {
                capture_source = string(&value)
                    .and_then(CaptureSource::parse)
                    .unwrap_or(CaptureSource::Otlp);
            }
            _ => {}
        }
    }
    let (mut service_name, mut service_version, mut deploy, mut instance) = ("", "", "", "");
    for (key, value) in span.resource().attributes() {
        let Some(value) = string(&value) else {
            continue;
        };
        match key {
            keys::RESOURCE_SERVICE_NAME => service_name = value,
            keys::RESOURCE_SERVICE_VERSION => service_version = value,
            keys::RESOURCE_DEPLOY_ID => deploy = value,
            keys::RESOURCE_INSTANCE_ID => instance = value,
            _ => {}
        }
    }
    let pick = |own: Option<&str>, resource: &str, cap: usize| {
        text(own.filter(|s| !s.is_empty()).unwrap_or(resource), cap)
    };
    let version = pick(version, service_version, caps::MCP_SERVER_VERSION_BYTES);
    Server {
        name: pick(name, service_name, caps::MCP_SERVER_NAME_BYTES),
        // Contract 9.7: the deploy id defaults to the server version.
        deploy_id: if deploy.is_empty() {
            text(&version, caps::MCP_DEPLOY_ID_BYTES)
        } else {
            text(deploy, caps::MCP_DEPLOY_ID_BYTES)
        },
        version,
        instance_id: text(instance, caps::MCP_INSTANCE_ID_BYTES),
        capture_source,
    }
}

/// Whether a span carries `mcp.method.name`.
pub(crate) fn is_operation(span: &SpanView<'_>) -> bool {
    span.attributes().any(|(key, _)| key == keys::METHOD)
}

/// Whether a span carries a tool list.
pub(crate) fn has_tools(span: &SpanView<'_>) -> bool {
    span.attributes().any(|(key, _)| key == keys::TOOLS)
}

/// The MCP operation of a span (contract 9.1), or `None` for a span without `mcp.method.name`. `core` is the
/// span's core tier after the mapping; `mutant` is a seeded defect.
pub(crate) fn project(
    core: &SpanCore,
    span: &SpanView<'_>,
    mutant: Option<Mutant>,
) -> Option<McpOpDraft> {
    let mut method: Option<Box<str>> = None;
    let (mut side, mut era, mut transport, mut network) = (None, None, None, None);
    let (mut tool, mut prompt, mut template) = ("", "", "");
    let mut protocol_version = "";
    let mut jsonrpc_id: Box<str> = "".into();
    let mut direction = Direction::ClientToServer;
    let (mut is_notification, mut is_batch, mut timed_out) = (false, false, false);
    let (mut error_type, mut rpc_code, mut http_status) = ("", None, None);
    let (mut explicit_class, mut subclass) = (None, ErrorSubclass::None);
    let mut result_type = None;
    let mut mrtr_round = 0u16;
    let (mut state_in, mut state_out) = (None, None);
    let (mut task_id, mut subscription_id) = ("", "");
    let mut progress_count = None;
    let mut cancel_source = CancelSource::None;
    let (mut client_name, mut client_version) = ("", "");
    let mut client_capabilities_hash = 0u64;
    let (mut oauth_client_id, mut tenant, mut user_id, mut handle_id) = ("", "", "", "");
    let (mut issuer, mut subject) = ("", "");
    let mut identity_source = None;
    let mut session = "";
    let (mut request_bytes, mut response_bytes) = (0u64, 0u64);
    let mut content_blocks: Vec<(Box<str>, u32)> = Vec::new();
    let (mut args, mut result) = (None, None);
    let (mut captured, mut sample_rate) = (None, None);
    let mut content_truncation = Truncation::None;
    let (mut ip_prefix, mut user_agent) = ("", "");

    for (key, value) in span.attributes() {
        let str_value = string(&value).unwrap_or_default();
        match key {
            keys::METHOD => method = Some(text(str_value, caps::MCP_METHOD_BYTES)),
            keys::SIDE => side = McpSide::parse(str_value).filter(|s| *s != McpSide::None),
            keys::ERA => era = Era::parse(str_value),
            keys::TRANSPORT => transport = Transport::parse(str_value),
            keys::NETWORK_TRANSPORT => {
                network = match str_value {
                    "pipe" => Some(Transport::Stdio),
                    "tcp" => Some(Transport::StreamableHttp),
                    _ => None,
                };
            }
            keys::TOOL_NAME => tool = str_value,
            keys::PROMPT_NAME => prompt = str_value,
            keys::RESOURCE_TEMPLATE => template = str_value,
            keys::PROTOCOL_VERSION => protocol_version = str_value,
            keys::JSONRPC_ID => jsonrpc_id = string_or_int(&value, caps::MCP_JSONRPC_ID_BYTES),
            keys::DIRECTION => {
                direction = Direction::parse(str_value).unwrap_or(Direction::ClientToServer);
            }
            keys::IS_NOTIFICATION => is_notification = flag(&value).unwrap_or(false),
            keys::IS_BATCH => is_batch = flag(&value).unwrap_or(false),
            keys::TIMED_OUT => timed_out = flag(&value).unwrap_or(false),
            keys::ERROR_TYPE => error_type = str_value,
            keys::RPC_STATUS_CODE => {
                rpc_code = int(&value).and_then(|code| i32::try_from(code).ok());
            }
            keys::HTTP_STATUS => {
                http_status = int(&value).and_then(|status| u16::try_from(status).ok());
            }
            keys::ERROR_CLASS => explicit_class = ErrorClass::parse(str_value),
            keys::ERROR_SUBCLASS => {
                subclass = ErrorSubclass::parse(str_value).unwrap_or(ErrorSubclass::None);
            }
            keys::RESULT_TYPE => result_type = ResultType::parse(str_value),
            keys::MRTR_ROUND => {
                mrtr_round = unsigned(&value).map_or(0, |n| u16::try_from(n).unwrap_or(u16::MAX));
            }
            keys::MRTR_STATE_IN => state_in = hex16(str_value),
            keys::MRTR_STATE_OUT => state_out = hex16(str_value),
            keys::TASK_ID => task_id = str_value,
            keys::SUBSCRIPTION_ID => subscription_id = str_value,
            keys::PROGRESS_COUNT => {
                progress_count = unsigned(&value).map(|n| u32::try_from(n).unwrap_or(u32::MAX));
            }
            keys::CANCEL_SOURCE => {
                cancel_source = CancelSource::parse(str_value).unwrap_or(CancelSource::None);
            }
            keys::CLIENT_NAME => client_name = str_value,
            keys::CLIENT_VERSION => client_version = str_value,
            keys::CLIENT_CAPABILITIES_HASH => client_capabilities_hash = capabilities_hash(&value),
            keys::OAUTH_CLIENT_ID => oauth_client_id = str_value,
            keys::TENANT => tenant = str_value,
            keys::USER_ID => user_id = str_value,
            keys::HANDLE_ID => handle_id = str_value,
            keys::USER_ISSUER => issuer = str_value,
            keys::USER_SUBJECT => subject = str_value,
            keys::IDENTITY_SOURCE => identity_source = IdentitySource::parse(str_value),
            keys::SESSION_ID => session = str_value,
            keys::REQUEST_BYTES => request_bytes = unsigned(&value).unwrap_or(0),
            keys::RESPONSE_BYTES => response_bytes = unsigned(&value).unwrap_or(0),
            keys::TOOL_ARGUMENTS => args = Some(body(value)),
            keys::TOOL_RESULT => result = Some(body(value)),
            keys::CONTENT_CAPTURED => captured = flag(&value),
            keys::CONTENT_SAMPLE_RATE => {
                sample_rate = match value {
                    AnyValueView::Double(d) => Some(d as f32),
                    AnyValueView::Int(i) => Some(i as f32),
                    _ => None,
                };
            }
            keys::CONTENT_TRUNCATION => {
                content_truncation = Truncation::parse(str_value).unwrap_or(Truncation::None);
            }
            keys::CLIENT_IP_PREFIX => ip_prefix = str_value,
            keys::USER_AGENT => user_agent = str_value,
            _ => {
                if let Some(kind) = key.strip_prefix(keys::CONTENT_BLOCKS_PREFIX)
                    && !kind.is_empty()
                    && let Some(count) = unsigned(&value)
                {
                    content_blocks.push((
                        text(kind, CONTENT_BLOCK_TYPE_BYTES),
                        u32::try_from(count).unwrap_or(u32::MAX),
                    ));
                }
            }
        }
    }
    let method = method?;

    let side = match span.kind() {
        SpanKind::Server => McpSide::Server,
        SpanKind::Client => McpSide::Client,
        _ => side.unwrap_or(McpSide::Server),
    };
    let era = era.unwrap_or(if protocol_version >= keys::MODERN_PROTOCOL_VERSION {
        Era::Modern
    } else {
        Era::Legacy
    });
    let target_kind = target_kind(&method);
    let target_name = match target_kind {
        McpTargetKind::Tool => tool,
        McpTargetKind::Prompt => prompt,
        McpTargetKind::Resource => template,
        McpTargetKind::None => "",
    };

    // The classifier's inputs (contract 9.2), as the module documentation reads them.
    let numeric_error = error_type.parse::<i64>().ok();
    let jsonrpc_code = rpc_code.or_else(|| {
        numeric_error
            .filter(|code| http_status.is_none_or(|status| i64::from(status) != *code))
            .and_then(|code| i32::try_from(code).ok())
    });
    let is_error = error_type == "tool_error";
    let classification = classify_mcp_error(&McpErrorInput {
        era,
        side,
        jsonrpc_code,
        is_error,
        http_status,
        cancel_source,
        timed_out: timed_out || explicit_class == Some(ErrorClass::Timeout),
        transport_failure: explicit_class == Some(ErrorClass::Transport),
        unhandled_exception: !error_type.is_empty() && !is_error && numeric_error.is_none(),
        annotations: McpErrorAnnotations {
            explicit_class,
            mark_validation: false,
            mark_not_found: false,
            subclass,
        },
    });
    let caller_fault = classification.caller_fault;
    #[cfg(feature = "test-mutants")]
    let caller_fault = caller_fault
        && !(mutant == Some(Mutant::MisclassifyCallerFault) && jsonrpc_code == Some(-32602));
    let _ = mutant;

    let server = server(span);
    // Contract 9.4, the first rule that matches.
    let session_key = if era == Era::Legacy {
        session_key(session)
    } else {
        None
    };
    let handle_id = text(handle_id, caps::MCP_HANDLE_ID_BYTES);
    let (group_key_kind, group_key): (GroupKeyKind, Box<str>) = if let Some(key) = session_key {
        (GroupKeyKind::LegacySession, hex(&key).into())
    } else if !handle_id.is_empty() {
        (GroupKeyKind::Handle, handle_id.clone())
    } else if core.parent_span_id != SpanId::ZERO {
        (GroupKeyKind::Trace, hex(core.trace_id.as_bytes()).into())
    } else {
        (GroupKeyKind::Process, server.instance_id.clone())
    };

    content_blocks.sort_by(|a, b| a.0.cmp(&b.0));
    content_blocks.dedup_by(|later, earlier| later.0 == earlier.0);
    content_blocks.truncate(caps::MCP_CONTENT_BLOCKS_MAX_KEYS);

    let content_captured = captured.unwrap_or(args.is_some() || result.is_some());
    let link = span.links().next();
    let user_id = text(user_id, caps::MCP_USER_ID_BYTES);
    let mut record = McpOpRecord {
        project_id: core.project_id,
        trace_id: core.trace_id,
        span_id: core.span_id,
        parent_span_id: core.parent_span_id,
        transport_link_trace_id: link.map_or(TraceId::ZERO, |l| l.trace_id()),
        transport_link_span_id: link.map_or(SpanId::ZERO, |l| l.span_id()),
        server_name: server.name,
        server_version: server.version,
        deploy_id: server.deploy_id,
        instance_id: server.instance_id,
        side,
        capture_source: server.capture_source,
        era,
        protocol_version: text(protocol_version, caps::MCP_PROTOCOL_VERSION_BYTES),
        transport: transport.or(network).unwrap_or(Transport::Unknown),
        method,
        target_kind,
        target_name: text(target_name, caps::MCP_TARGET_NAME_BYTES),
        jsonrpc_id,
        direction,
        is_notification,
        is_batch,
        // Contract 9.1: `complete` for a successful request, else `none`.
        result_type: result_type.unwrap_or(
            if classification.error_class == ErrorClass::None && !is_notification {
                ResultType::Complete
            } else {
                ResultType::None
            },
        ),
        mrtr_round,
        mrtr_state_in: state_in,
        mrtr_state_out: state_out,
        task_id: text(task_id, caps::MCP_TASK_ID_BYTES),
        subscription_id: text(subscription_id, caps::MCP_TASK_ID_BYTES),
        start_time: core.start_time,
        duration_us: core.duration_us(),
        progress_count,
        cancel_source,
        timed_out,
        status: classification.status,
        error_class: classification.error_class,
        error_subclass: classification.error_subclass,
        jsonrpc_code,
        http_status,
        caller_fault,
        error_type: text(error_type, caps::MCP_ERROR_TYPE_BYTES),
        error_message: text(&core.status_message, caps::MCP_ERROR_MESSAGE_BYTES),
        client_name: text(client_name, caps::MCP_CLIENT_NAME_BYTES),
        client_version: text(client_version, caps::MCP_CLIENT_VERSION_BYTES),
        client_capabilities_hash,
        oauth_client_id: text(oauth_client_id, caps::MCP_OAUTH_CLIENT_ID_BYTES),
        user_subject_hash: None,
        user_issuer: text(issuer, caps::MCP_USER_ISSUER_BYTES),
        identity_source: identity_source.unwrap_or(if user_id.is_empty() {
            IdentitySource::None
        } else {
            IdentitySource::IdentifyHook
        }),
        user_id,
        tenant: text(tenant, caps::MCP_TENANT_BYTES),
        session_key,
        handle_id,
        group_key: text(&group_key, caps::MCP_GROUP_KEY_BYTES),
        group_key_kind,
        ip_prefix: text(ip_prefix, caps::MCP_IP_PREFIX_BYTES),
        user_agent: text(user_agent, caps::MCP_USER_AGENT_BYTES),
        request_bytes,
        response_bytes,
        content_blocks,
        content_captured,
        content_sample_rate: match sample_rate {
            Some(rate) if rate.is_finite() => rate.clamp(0.0, 1.0),
            Some(_) => 0.0,
            None => f32::from(u8::from(content_captured)),
        },
        content_truncation,
        args: Bytes::new(),
        result: Bytes::new(),
        args_bytes: 0,
        result_bytes: 0,
        ingestion: core.ingestion,
        normalizer_version: core.normalizer_version,
    };
    if content_captured {
        record.args = args.unwrap_or_default();
        record.result = result.unwrap_or_default();
        record.enforce_content_cap(caps::MCP_CONTENT_MAX_BYTES);
    }
    Some(McpOpDraft {
        record,
        subject: subject.into(),
    })
}

/// A schema in canonical form: object members in ascending byte order of their names, at every depth. Members
/// with one name keep their wire order.
fn canonical(node: &Node) -> Node {
    match node {
        Node::Arr(items) => Node::Arr(items.iter().map(canonical).collect()),
        Node::Obj(members) => {
            let mut members: Vec<(String, Node)> = members
                .iter()
                .map(|(name, value)| (name.clone(), canonical(value)))
                .collect();
            members.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            Node::Obj(members)
        }
        other => other.clone(),
    }
}

/// `schema_hash` (contract 3.6): xxh3-64 of the canonical JSON of the input schema, one zero byte and the
/// canonical JSON of the output schema; an absent schema is `null`.
fn schema_hash(tool: &Node) -> u64 {
    let mut bytes = Vec::new();
    for (at, member) in ["inputSchema", "outputSchema"].into_iter().enumerate() {
        if at > 0 {
            bytes.push(0);
        }
        match tool.member(member) {
            Some(schema) => canonical(schema).push(&mut bytes),
            None => bytes.extend_from_slice(b"null"),
        }
    }
    xxh3_64(&bytes)
}

/// The tool inventory entries a span reports (contract 3.6): one per tool of its `crucible.mcp.tools` list that
/// has a name, in list order.
pub(crate) fn inventory(core: &SpanCore, span: &SpanView<'_>) -> Vec<InventoryDraft> {
    let Some(tools) = span
        .attributes()
        .find(|(key, _)| *key == keys::TOOLS)
        .map(|(_, value)| value)
    else {
        return Vec::new();
    };
    let tools = match tools {
        AnyValueView::String(s) => Node::parse_container(s),
        other @ AnyValueView::Array(_) => Some(Node::from_view(other)),
        _ => None,
    };
    let Some(Node::Arr(tools)) = tools else {
        return Vec::new();
    };
    let server = server(span);
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.member("name")?.as_str().filter(|n| !n.is_empty())?;
            Some(McpToolInventoryRecord {
                project_id: core.project_id,
                server_name: server.name.clone(),
                deploy_id: server.deploy_id.clone(),
                tool_name: text(name, caps::MCP_INVENTORY_TOOL_NAME_BYTES),
                schema_hash: schema_hash(tool),
                capture_source: server.capture_source,
                first_seen: core.start_time,
                last_seen: core.start_time,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_that_is_not_a_hash_is_hashed() {
        // The vectors of `conformance/registry.json`.
        let raw = "3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5";
        let key = session_key(raw).map(|k| hex(&k));
        assert_eq!(key.as_deref(), Some("84757436d5e0791bd4a8eaa6f1d05986"));
        // The hashed form is taken as it is; uppercase is not that form.
        assert_eq!(
            session_key("84757436d5e0791bd4a8eaa6f1d05986").map(|k| hex(&k)),
            key
        );
        assert_ne!(
            session_key("84757436D5E0791BD4A8EAA6F1D05986").map(|k| hex(&k)),
            key
        );
        assert_eq!(session_key(""), None);
    }

    #[test]
    fn the_subject_hash_is_the_keyed_hash_of_issuer_and_subject() {
        let salt = ProjectSalt::new([7; 32]);
        let mut mac = Hmac::<Sha256>::new_from_slice(&[7; 32]).unwrap();
        mac.update(b"https://issuer.example\0user-1");
        let expected = mac.finalize().into_bytes();
        assert_eq!(
            salt.subject_hash("https://issuer.example", "user-1")[..],
            expected[..16]
        );
        // The separator keeps (issuer, subject) pairs apart.
        assert_ne!(salt.subject_hash("ab", "c"), salt.subject_hash("a", "bc"));
        assert_ne!(
            salt.subject_hash("i", "s"),
            ProjectSalt::new([8; 32]).subject_hash("i", "s")
        );
        assert_eq!(format!("{salt:?}"), "ProjectSalt(..)");
    }

    #[test]
    fn the_schema_hash_ignores_member_order_and_whitespace() {
        let parse = |s: &str| Node::parse_container(s).unwrap();
        let a = parse(
            r#"{"name":"t","inputSchema":{"type":"object","properties":{"b":{"type":"string"},"a":{}}}}"#,
        );
        let b = parse(
            r#"{ "inputSchema": {"properties": {"a": {}, "b": {"type": "string"}}, "type": "object"}, "name": "t" }"#,
        );
        assert_eq!(schema_hash(&a), schema_hash(&b));
        let other = parse(r#"{"name":"t","inputSchema":{"type":"object"}}"#);
        assert_ne!(schema_hash(&a), schema_hash(&other));
        // An output schema is part of the hash, and is not the input schema.
        let out = parse(r#"{"name":"t","outputSchema":{"type":"object"}}"#);
        assert_ne!(schema_hash(&other), schema_hash(&out));
        assert_eq!(
            schema_hash(&parse(r#"{"name":"t"}"#)),
            xxh3_64(b"null\0null")
        );
    }
}
