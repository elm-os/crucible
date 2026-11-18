//! MCP records (contract 3.5 to 3.7): one row per MCP operation, the tool inventory record and the deploy record.
//!
//! Representation follows the span records (see the `span` module): `Box<str>` strings written once at their final
//! size, [`Bytes`] for the opt-in content bodies, vectors of pairs for small maps. Optional OTel ids are
//! all-zero when absent, as stored; other optional values are `Option`.

use bytes::Bytes;

use crate::enums::{
    CancelSource, CaptureSource, Direction, Era, ErrorClass, ErrorSubclass, GroupKeyKind,
    IdentitySource, McpSide, McpStatus, McpTargetKind, ResultType, Transport, Truncation,
};
use crate::id::{ProjectId, SpanId, TraceId};
use crate::ingestion::Ingestion;
use crate::time::Timestamp;
use crate::truncate::utf8_floor;

/// One MCP operation (`mcp_ops`, and `mcp_content` when content was captured), keyed by its span's
/// `(project_id, trace_id, span_id)`. Caps are the constants in [`caps`].
///
/// Never stored here: Authorization values, `requestState`, cookies, `Mcp-Param-*` values, baggage, raw IP
/// addresses and raw user subjects.
#[derive(Clone, PartialEq, Debug)]
pub struct McpOpRecord {
    /// From the credential only.
    pub project_id: ProjectId,
    /// The span's trace id.
    pub trace_id: TraceId,
    /// The span's id.
    pub span_id: SpanId,
    /// The span's parent; [`SpanId::ZERO`] when absent.
    pub parent_span_id: SpanId,
    /// Trace id of the first span link; [`TraceId::ZERO`] when absent.
    pub transport_link_trace_id: TraceId,
    /// Span id of the first span link; [`SpanId::ZERO`] when absent.
    pub transport_link_span_id: SpanId,
    /// [`crate::caps::MCP_SERVER_NAME_BYTES`].
    pub server_name: Box<str>,
    /// [`crate::caps::MCP_SERVER_VERSION_BYTES`].
    pub server_version: Box<str>,
    /// Contract 9.7; [`crate::caps::MCP_DEPLOY_ID_BYTES`].
    pub deploy_id: Box<str>,
    /// [`crate::caps::MCP_INSTANCE_ID_BYTES`].
    pub instance_id: Box<str>,
    /// Client or server.
    pub side: McpSide,
    /// Which instrumentation captured the operation.
    pub capture_source: CaptureSource,
    /// Protocol era.
    pub era: Era,
    /// [`crate::caps::MCP_PROTOCOL_VERSION_BYTES`].
    pub protocol_version: Box<str>,
    /// Transport.
    pub transport: Transport,
    /// JSON-RPC method; [`crate::caps::MCP_METHOD_BYTES`].
    pub method: Box<str>,
    /// From the method.
    pub target_kind: McpTargetKind,
    /// Tool or prompt name, or a resource URI template; [`crate::caps::MCP_TARGET_NAME_BYTES`].
    pub target_name: Box<str>,
    /// JSON-RPC id in string form; [`crate::caps::MCP_JSONRPC_ID_BYTES`].
    pub jsonrpc_id: Box<str>,
    /// Message direction.
    pub direction: Direction,
    /// A notification has no response.
    pub is_notification: bool,
    /// Sent in a JSON-RPC batch.
    pub is_batch: bool,
    /// Kind of result.
    pub result_type: ResultType,
    /// Round of a multi-round exchange; 0 when not part of one.
    pub mrtr_round: u16,
    /// Keyed hash of the incoming `requestState` (contract 9.6).
    pub mrtr_state_in: Option<[u8; 16]>,
    /// Keyed hash of the returned `requestState` (contract 9.6).
    pub mrtr_state_out: Option<[u8; 16]>,
    /// [`crate::caps::MCP_TASK_ID_BYTES`].
    pub task_id: Box<str>,
    /// [`crate::caps::MCP_TASK_ID_BYTES`].
    pub subscription_id: Box<str>,
    /// Start of the operation.
    pub start_time: Timestamp,
    /// Duration of the operation.
    pub duration_us: u64,
    /// Progress notifications seen; `None` when the capture source could not observe progress.
    pub progress_count: Option<u32>,
    /// How the operation was cancelled.
    pub cancel_source: CancelSource,
    /// Server deadline, or no response by the deadline.
    pub timed_out: bool,
    /// Outcome (contract 9.2).
    pub status: McpStatus,
    /// Error class (contract 9.2).
    pub error_class: ErrorClass,
    /// Set only by SDK annotation.
    pub error_subclass: ErrorSubclass,
    /// JSON-RPC error code.
    pub jsonrpc_code: Option<i32>,
    /// HTTP response status.
    pub http_status: Option<u16>,
    /// Contract 9.2 caller-fault rule.
    pub caller_fault: bool,
    /// The semantic-convention `error.type` string; [`crate::caps::MCP_ERROR_TYPE_BYTES`].
    pub error_type: Box<str>,
    /// Scrubbed in the SDK or proxy; [`crate::caps::MCP_ERROR_MESSAGE_BYTES`].
    pub error_message: Box<str>,
    /// Self-reported; [`crate::caps::MCP_CLIENT_NAME_BYTES`].
    pub client_name: Box<str>,
    /// Self-reported; [`crate::caps::MCP_CLIENT_VERSION_BYTES`].
    pub client_version: Box<str>,
    /// Hash of the client capabilities.
    pub client_capabilities_hash: u64,
    /// [`crate::caps::MCP_OAUTH_CLIENT_ID_BYTES`].
    pub oauth_client_id: Box<str>,
    /// Keyed hash of issuer and subject (contract 9.3).
    pub user_subject_hash: Option<[u8; 16]>,
    /// The issuer the hash was computed with; [`crate::caps::MCP_USER_ISSUER_BYTES`].
    pub user_issuer: Box<str>,
    /// Only from the `identify` hook; [`crate::caps::MCP_USER_ID_BYTES`].
    pub user_id: Box<str>,
    /// [`crate::caps::MCP_TENANT_BYTES`].
    pub tenant: Box<str>,
    /// Where the identity came from.
    pub identity_source: IdentitySource,
    /// Legacy era only (contract 9.4): 16 bytes, stored as 32 lowercase hex characters.
    pub session_key: Option<[u8; 16]>,
    /// Contract 9.8; [`crate::caps::MCP_HANDLE_ID_BYTES`].
    pub handle_id: Box<str>,
    /// Contract 9.4; [`crate::caps::MCP_GROUP_KEY_BYTES`].
    pub group_key: Box<str>,
    /// Which rule produced `group_key`.
    pub group_key_kind: GroupKeyKind,
    /// A /24 or /48 prefix, truncated by the sender.
    pub ip_prefix: Box<str>,
    /// [`crate::caps::MCP_USER_AGENT_BYTES`].
    pub user_agent: Box<str>,
    /// Uncompressed JSON-RPC request size; 0 when unknown.
    pub request_bytes: u64,
    /// Uncompressed JSON-RPC response size; 0 when unknown.
    pub response_bytes: u64,
    /// Count per content type; [`crate::caps::MCP_CONTENT_BLOCKS_MAX_KEYS`] keys in ascending byte order.
    pub content_blocks: Vec<(Box<str>, u32)>,
    /// Arguments and result were captured (opt-in); an `mcp_content` row exists only then.
    pub content_captured: bool,
    /// Sampling rate of content capture.
    pub content_sample_rate: f32,
    /// Whether `args` or `result` was cut, by the sender or by [`McpOpRecord::enforce_content_cap`].
    pub content_truncation: Truncation,
    /// Arguments, JSON (`mcp_content`).
    pub args: Bytes,
    /// Result, JSON (`mcp_content`).
    pub result: Bytes,
    /// Size of `args` as received, before the normalizer's cap (`mcp_content.args_bytes`).
    pub args_bytes: u64,
    /// Size of `result` as received, before the normalizer's cap (`mcp_content.result_bytes`).
    pub result_bytes: u64,
    /// Where and when the WAL accepted the span.
    pub ingestion: Ingestion,
    /// Version of the normalizer that built the record.
    pub normalizer_version: u32,
}

impl McpOpRecord {
    /// `start_minute` (contract 6.1).
    pub const fn start_minute(&self) -> u32 {
        self.start_time.start_minute()
    }

    /// `trace_hash` (contract 6.1).
    pub fn trace_hash(&self) -> u32 {
        self.trace_id.trace_hash()
    }

    /// Applies the normalizer's hard cap to `args` and `result` (contract 3.5): records their received sizes,
    /// cuts each to the longest prefix of at most `max_bytes` that ends on a character boundary, and sets
    /// `content_truncation` to `cap_exceeded` when either was cut. A truncation reported by the sender is kept.
    /// Call it once, on the values as received.
    pub fn enforce_content_cap(&mut self, max_bytes: usize) {
        self.args_bytes = self.args.len() as u64;
        self.result_bytes = self.result.len() as u64;
        let mut cut = false;
        for body in [&mut self.args, &mut self.result] {
            if body.len() > max_bytes {
                body.truncate(utf8_floor(body, max_bytes));
                cut = true;
            }
        }
        if cut {
            self.content_truncation = Truncation::CapExceeded;
        }
    }
}

/// A tool seen in a `tools/list` or `server/discover` result, or in an SDK's snapshot of its registered tools
/// (contract 3.6). The shipper folds these into [`crate::McpToolInventoryRow`]s.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct McpToolInventoryRecord {
    /// From the credential only.
    pub project_id: ProjectId,
    /// [`crate::caps::MCP_SERVER_NAME_BYTES`].
    pub server_name: Box<str>,
    /// [`crate::caps::MCP_DEPLOY_ID_BYTES`].
    pub deploy_id: Box<str>,
    /// [`crate::caps::MCP_INVENTORY_TOOL_NAME_BYTES`].
    pub tool_name: Box<str>,
    /// xxh3-64 of the canonical JSON of the tool's input and output schemas.
    pub schema_hash: u64,
    /// Which instrumentation observed the tool.
    pub capture_source: CaptureSource,
    /// The observing span's `start_time`.
    pub first_seen: Timestamp,
    /// The observing span's `start_time`.
    pub last_seen: Timestamp,
}

/// A deploy seen on server-side MCP rows (contract 3.7), emitted by stage B once per distinct
/// `(server_name, deploy_id, server_version, capture_source)` per batch. It is also the `mcp_deploys` row: those
/// four values and `project_id` are its key, and the seen times merge by min and max.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct McpDeployRecord {
    /// From the credential only.
    pub project_id: ProjectId,
    /// [`crate::caps::MCP_SERVER_NAME_BYTES`].
    pub server_name: Box<str>,
    /// [`crate::caps::MCP_DEPLOY_ID_BYTES`].
    pub deploy_id: Box<str>,
    /// [`crate::caps::MCP_SERVER_VERSION_BYTES`].
    pub server_version: Box<str>,
    /// Which instrumentation captured the rows.
    pub capture_source: CaptureSource,
    /// Earliest `start_time` of the rows.
    pub first_seen: Timestamp,
    /// Latest `start_time` of the rows.
    pub last_seen: Timestamp,
}

impl McpDeployRecord {
    /// Merges the measures of a row with the same key (`min`, `max`).
    pub fn merge(&mut self, other: &Self) {
        self.first_seen = self.first_seen.min(other.first_seen);
        self.last_seen = self.last_seen.max(other.last_seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::WalEpoch;
    use crate::lsn::Lsn;

    fn op() -> McpOpRecord {
        McpOpRecord {
            project_id: ProjectId::from_bytes([1; 16]),
            trace_id: TraceId::from_bytes([2; 16]),
            span_id: SpanId::from_bytes([3; 8]),
            parent_span_id: SpanId::ZERO,
            transport_link_trace_id: TraceId::ZERO,
            transport_link_span_id: SpanId::ZERO,
            server_name: "files".into(),
            server_version: "1.2.0".into(),
            deploy_id: "1.2.0".into(),
            instance_id: "".into(),
            side: McpSide::Server,
            capture_source: CaptureSource::Sdk,
            era: Era::Modern,
            protocol_version: "2026-07-28".into(),
            transport: Transport::Stdio,
            method: "tools/call".into(),
            target_kind: McpTargetKind::Tool,
            target_name: "read_file".into(),
            jsonrpc_id: "7".into(),
            direction: Direction::ClientToServer,
            is_notification: false,
            is_batch: false,
            result_type: ResultType::Complete,
            mrtr_round: 0,
            mrtr_state_in: None,
            mrtr_state_out: None,
            task_id: "".into(),
            subscription_id: "".into(),
            start_time: Timestamp::from_micros(1_794_907_830_000_000),
            duration_us: 1_500,
            progress_count: None,
            cancel_source: CancelSource::None,
            timed_out: false,
            status: McpStatus::Ok,
            error_class: ErrorClass::None,
            error_subclass: ErrorSubclass::None,
            jsonrpc_code: None,
            http_status: None,
            caller_fault: false,
            error_type: "".into(),
            error_message: "".into(),
            client_name: "".into(),
            client_version: "".into(),
            client_capabilities_hash: 0,
            oauth_client_id: "".into(),
            user_subject_hash: None,
            user_issuer: "".into(),
            user_id: "".into(),
            tenant: "".into(),
            identity_source: IdentitySource::None,
            session_key: None,
            handle_id: "".into(),
            group_key: "".into(),
            group_key_kind: GroupKeyKind::Process,
            ip_prefix: "".into(),
            user_agent: "".into(),
            request_bytes: 0,
            response_bytes: 0,
            content_blocks: Vec::new(),
            content_captured: true,
            content_sample_rate: 1.0,
            content_truncation: Truncation::None,
            args: Bytes::new(),
            result: Bytes::new(),
            args_bytes: 0,
            result_bytes: 0,
            ingestion: Ingestion {
                wal_epoch: WalEpoch::from_bytes([4; 16]),
                batch_seq: Lsn::new(1),
                lsn: Lsn::new(1),
                received_at: Timestamp::from_micros(1_794_907_831_000_000),
            },
            normalizer_version: 1,
        }
    }

    #[test]
    fn content_cap_cuts_on_character_boundaries() {
        let mut r = op();
        r.args = Bytes::from("{\"p\":\"\u{4e2d}\u{4e2d}\"}");
        r.result = Bytes::from_static(b"{}");
        r.enforce_content_cap(8);
        // `{"p":"` is 6 bytes; the next character takes 3, so the cut lands at 6.
        assert_eq!(&r.args[..], b"{\"p\":\"");
        assert_eq!((r.args_bytes, r.result_bytes), (14, 2));
        assert_eq!(&r.result[..], b"{}");
        assert_eq!(r.content_truncation, Truncation::CapExceeded);
        assert_eq!(r.start_minute(), 1_794_907_800);
    }

    #[test]
    fn content_cap_keeps_a_sender_truncation() {
        let mut r = op();
        r.args = Bytes::from_static(b"{}");
        r.content_truncation = Truncation::CapExceeded;
        r.enforce_content_cap(crate::caps::MCP_CONTENT_MAX_BYTES);
        assert_eq!(r.content_truncation, Truncation::CapExceeded);
        let mut r = op();
        r.args = Bytes::from_static(b"{}");
        r.enforce_content_cap(crate::caps::MCP_CONTENT_MAX_BYTES);
        assert_eq!(r.content_truncation, Truncation::None);
    }

    #[test]
    fn deploy_records_merge_by_min_and_max() {
        let a = McpDeployRecord {
            project_id: ProjectId::from_bytes([1; 16]),
            server_name: "files".into(),
            deploy_id: "d1".into(),
            server_version: "1.2.0".into(),
            capture_source: CaptureSource::ProxyStdio,
            first_seen: Timestamp::from_micros(20),
            last_seen: Timestamp::from_micros(30),
        };
        let b = McpDeployRecord {
            first_seen: Timestamp::from_micros(10),
            last_seen: Timestamp::from_micros(25),
            ..a.clone()
        };
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b;
        ba.merge(&a);
        assert_eq!(ab, ba);
        assert_eq!((ab.first_seen.micros(), ab.last_seen.micros()), (10, 30));
    }
}
