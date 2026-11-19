//! The MCP analytics under `projects/{project_id}/mcp/` (contract 9.9): the parameters every route takes and the
//! responses of servers, tools, one tool, errors, clients, users, sessions, operations, one operation, deploys
//! and compare.
//!
//! Every response names the capture source each server was read from (`capture_sources`, contract 9.5). Client
//! names and versions are self-reported by the clients (contract 3.5) and are labelled so.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::enums::mirror;
use crate::meta::Meta;
use crate::metrics::Granularity;
use crate::wire::SafeU64;
use crucible_records as records;

mirror! {
    /// The side of an MCP operation.
    McpSide {
        /// Neither (not an MCP row).
        None,
        /// Captured on the client side.
        Client,
        /// Captured on the server side.
        Server,
    }
}

mirror! {
    /// Who observed an MCP operation.
    CaptureSource {
        /// An SDK wrapper.
        Sdk,
        /// The proxy over stdio.
        ProxyStdio,
        /// The proxy over HTTP.
        ProxyHttp,
        /// Plain semantic-convention spans.
        Otlp,
    }
}

mirror! {
    /// The protocol era of an operation.
    Era {
        /// Before protocol version 2026-07-28.
        Legacy,
        /// From protocol version 2026-07-28.
        Modern,
    }
}

mirror! {
    /// The transport of an operation.
    Transport {
        /// Not known.
        Unknown,
        /// Standard input and output.
        Stdio,
        /// Streamable HTTP.
        StreamableHttp,
        /// Server-sent events.
        Sse,
    }
}

mirror! {
    /// What a method targets.
    McpTargetKind {
        /// No target.
        None,
        /// A tool.
        Tool,
        /// A prompt.
        Prompt,
        /// A resource.
        Resource,
    }
}

mirror! {
    /// What an operation's result was.
    ResultType {
        /// No result.
        None,
        /// A complete result.
        Complete,
        /// The server asked for more input.
        InputRequired,
        /// A task was started.
        Task,
    }
}

mirror! {
    /// The outcome of an operation (contract 9.2).
    McpStatus {
        /// Succeeded.
        Ok,
        /// The tool reported an error.
        ToolError,
        /// A JSON-RPC or HTTP error.
        ProtocolError,
        /// The transport failed or the call timed out.
        TransportError,
        /// Cancelled.
        Cancelled,
    }
}

mirror! {
    /// The error class of an operation (contract 9.2).
    ErrorClass {
        /// No error.
        None,
        /// A protocol error.
        Protocol,
        /// Invalid arguments.
        Validation,
        /// The target does not exist.
        NotFound,
        /// The tool failed.
        ToolExecution,
        /// Authentication or authorization failed.
        Auth,
        /// Rate limited.
        RateLimit,
        /// Timed out.
        Timeout,
        /// The transport failed.
        Transport,
        /// Cancelled.
        Cancelled,
        /// An internal error.
        Internal,
    }
}

mirror! {
    /// The subclass of a tool error that an SDK annotated.
    ErrorSubclass {
        /// None.
        None,
        /// Authentication upstream.
        Auth,
        /// Rate limited upstream.
        RateLimit,
        /// An upstream failure.
        Upstream,
        /// A business rule.
        Business,
    }
}

mirror! {
    /// What an MCP session's group key is (contract 9.4).
    GroupKeyKind {
        /// A legacy-era session id.
        LegacySession,
        /// A handle from the SDK's identify hook or a handle parameter.
        Handle,
        /// The trace of a propagated context.
        Trace,
        /// The server process.
        Process,
    }
}

mirror! {
    /// How a user identity was established (contract 9.3).
    IdentitySource {
        /// No identity.
        None,
        /// The SDK's identify hook.
        IdentifyHook,
        /// Token claims decoded without verification.
        TokenClaims,
        /// A verified token.
        TokenVerified,
    }
}

/// The side an MCP analytics request reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum McpSideParam {
    /// The client side: agents calling servers.
    Client,
    /// The server side: servers answering calls.
    Server,
}

/// A class of capture sources (contract 9.5).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum CaptureClass {
    /// An SDK wrapper.
    Sdk,
    /// The proxy, over stdio or HTTP.
    Proxy,
    /// Plain semantic-convention spans.
    Otlp,
}

/// The `source` parameter (contract 9.5).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SourceParam {
    /// Per server, the best-ranked class with rows in the window: SDK, then proxy, then OTLP.
    Auto,
    /// The SDK.
    Sdk,
    /// The proxy (both proxy sources).
    Proxy,
    /// OTLP.
    Otlp,
}

/// The parameters of every route under `projects/{project_id}/mcp/`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct McpParams {
    /// The side to read: `client` or `server`. The two sides are never summed.
    pub side: McpSideParam,
    /// Start of the window, inclusive (RFC 3339).
    #[param(format = DateTime)]
    pub from: String,
    /// End of the window, exclusive (RFC 3339). The window may span at most the project's maximum (default
    /// 30 days).
    #[param(format = DateTime)]
    pub to: String,
    /// Which capture source each server is read from; default `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub source: Option<SourceParam>,
    /// Whether calls the caller caused (contract 9.2) count in calls, errors and latency; default false. They
    /// are always reported apart as `caller_faults`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub include_caller_faults: Option<bool>,
    /// Only this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub server: Option<String>,
}

/// The page size of the MCP user and session lists.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct McpListParams {
    /// Rows: 1 to 200, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 200)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// Which section of `errors` or `clients` a request reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum McpView {
    /// From the rollup: error classes, or clients by name. The default.
    Summary,
    /// From the operations: errors by message and code, or clients by version.
    Breakdown,
}

/// The section and page size of `errors` and `clients`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct McpViewParams {
    /// `summary` (default) or `breakdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub view: Option<McpView>,
    /// Rows of the breakdown: 1 to 200, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 200)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// What the operation list is narrowed to, and its page.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct McpOperationsParams {
    /// Page size: 1 to 200, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 200)]
    #[ts(optional)]
    pub limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cursor: Option<String>,
    /// A tool or prompt name, or a resource template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub target: Option<String>,
    /// A method, such as `tools/call`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub method: Option<String>,
    /// A self-reported client name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub client: Option<String>,
    /// One error class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error_class: Option<ErrorClass>,
    /// Failed operations only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub errors_only: Option<bool>,
    /// One MCP session, by group key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session: Option<String>,
    /// One user: a user id, or a subject hash in 32 hex characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub user: Option<String>,
}

/// The two sides of a compare: two deploys inside the window, or a baseline window against the window.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct McpCompareParams {
    /// The deploy compared against; with `candidate_deploy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub baseline_deploy: Option<String>,
    /// The deploy compared; with `baseline_deploy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub candidate_deploy: Option<String>,
    /// Start of the window compared against (RFC 3339); with `baseline_to`, instead of two deploys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(format = DateTime)]
    #[ts(optional)]
    pub baseline_from: Option<String>,
    /// End of the window compared against (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(format = DateTime)]
    #[ts(optional)]
    pub baseline_to: Option<String>,
}

/// The capture source of one server (contract 9.5).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpServerSource {
    /// The server.
    pub server_name: String,
    /// The classes the server has rows of on the side in the window, best first.
    pub available: Vec<CaptureClass>,
    /// The class read: the request's, or under `auto` the best available. Null when the request names a class
    /// the server has no rows of.
    #[schema(required = true)]
    pub chosen: Option<CaptureClass>,
}

/// Latency percentiles in microseconds, from the deterministic histogram (contract 6.5); null without calls.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpLatency {
    /// The median.
    #[schema(required = true)]
    pub p50_us: Option<SafeU64>,
    /// The 90th percentile.
    #[schema(required = true)]
    pub p90_us: Option<SafeU64>,
    /// The 99th percentile.
    #[schema(required = true)]
    pub p99_us: Option<SafeU64>,
}

/// The measures of a set of calls.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpStats {
    /// Calls, without the caller-fault calls unless the request includes them.
    pub calls: SafeU64,
    /// Calls with an error class other than `none`, under the same rule.
    pub errors: SafeU64,
    /// `errors / calls`; null without calls.
    #[schema(required = true)]
    pub error_rate: Option<f64>,
    /// Calls the caller caused (contract 9.2), counted whether or not they are included.
    pub caller_faults: SafeU64,
    /// Latency of the calls counted.
    pub latency: McpLatency,
    /// When the last call was seen.
    #[schema(required = true, format = DateTime)]
    pub last_seen: Option<String>,
}

/// One server.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpServerRow {
    /// The server.
    pub server_name: String,
    /// Its calls.
    pub stats: McpStats,
    /// Distinct self-reported client names that called it.
    pub clients: SafeU64,
    /// The version of its most recently seen deploy; null when no deploy was seen in the window.
    #[schema(required = true)]
    pub version: Option<String>,
    /// Deploys seen in the window.
    pub deploys: SafeU64,
}

/// `GET projects/{project_id}/mcp/servers`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpServerList {
    /// The servers, most calls first.
    pub data: Vec<McpServerRow>,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One tool of a server.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpToolRow {
    /// The server.
    pub server_name: String,
    /// The tool.
    pub tool_name: String,
    /// Its calls.
    pub stats: McpStats,
    /// Request bytes of the calls counted.
    pub request_bytes: SafeU64,
    /// Response bytes of the calls counted.
    pub response_bytes: SafeU64,
    /// The xxh3-64 of its latest listed schema, 16 lowercase hex characters; null when the inventory has no
    /// definition in the window.
    #[schema(required = true)]
    pub schema_hash: Option<String>,
    /// Listed in the inventory with no call in the window.
    pub unused: bool,
}

/// `GET projects/{project_id}/mcp/tools`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpToolList {
    /// The called tools, most calls first, then the unused ones.
    pub data: Vec<McpToolRow>,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One bucket of a tool's calls.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpSeriesPoint {
    /// The bucket's first instant.
    #[schema(format = DateTime)]
    pub bucket: String,
    /// Calls in the bucket.
    pub calls: SafeU64,
    /// Failed calls in the bucket.
    pub errors: SafeU64,
    /// Latency in the bucket.
    pub latency: McpLatency,
}

/// Calls of one error class.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpClassCount {
    /// The error class.
    pub error_class: ErrorClass,
    /// The calls.
    pub calls: SafeU64,
}

/// The calls of one client, by self-reported name.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpClientRow {
    /// The client's name as it reported it; empty when it sent none.
    pub client_name: String,
    /// Its calls.
    pub stats: McpStats,
}

/// One deploy (contract 9.7).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpDeployRow {
    /// The server.
    pub server_name: String,
    /// The deploy id; empty (shown as unknown) when the server reported neither a deploy id nor a version.
    pub deploy_id: String,
    /// The server version.
    pub server_version: String,
    /// Who observed it.
    pub capture_source: CaptureSource,
    /// First seen.
    #[schema(format = DateTime)]
    pub first_seen: String,
    /// Last seen.
    #[schema(format = DateTime)]
    pub last_seen: String,
}

/// `GET projects/{project_id}/mcp/tools/{name}`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpToolDetail {
    /// The tool.
    pub tool_name: String,
    /// Its calls in the window.
    pub stats: McpStats,
    /// Request bytes of the calls counted.
    pub request_bytes: SafeU64,
    /// Response bytes of the calls counted.
    pub response_bytes: SafeU64,
    /// The bucket width of `series`: the finest that keeps the window under 500 buckets.
    pub granularity: Granularity,
    /// Every bucket of the window, oldest first.
    pub series: Vec<McpSeriesPoint>,
    /// Calls per error class, most first.
    pub error_classes: Vec<McpClassCount>,
    /// The clients with the most calls, at most 10.
    pub clients: Vec<McpClientRow>,
    /// The deploys of the tool's servers seen in the window, oldest first: the series' markers.
    pub deploys: Vec<McpDeployRow>,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// Failed calls of one method and target in one class.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpTargetErrorRow {
    /// The method.
    pub method: String,
    /// The tool or prompt name, or the resource template.
    pub target_name: String,
    /// The error class.
    pub error_class: ErrorClass,
    /// The calls.
    pub calls: SafeU64,
}

/// Failed calls that share a server, method, target, class, subclass, code and message.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpErrorGroupRow {
    /// The server.
    pub server_name: String,
    /// The method.
    pub method: String,
    /// The tool or prompt name, or the resource template.
    pub target_name: String,
    /// The error class.
    pub error_class: ErrorClass,
    /// The subclass an SDK annotated.
    pub error_subclass: ErrorSubclass,
    /// The JSON-RPC error code; null when the error had none.
    #[schema(required = true)]
    pub jsonrpc_code: Option<i32>,
    /// The scrubbed error message.
    pub error_message: String,
    /// The calls.
    pub calls: SafeU64,
    /// When the last of them started.
    #[schema(format = DateTime)]
    pub last_seen: String,
}

/// `GET projects/{project_id}/mcp/errors`: the summary (error classes, from the rollup) or the breakdown
/// (failed calls by message and code, from the operations), as `view` asks.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpErrors {
    /// The section this answer holds.
    pub view: McpView,
    /// Summary: every call of the read. Null in the breakdown.
    #[schema(required = true)]
    pub stats: Option<McpStats>,
    /// Summary: calls per error class, most first.
    pub classes: Vec<McpClassCount>,
    /// Summary: failed calls per method, target and class, most first.
    pub targets: Vec<McpTargetErrorRow>,
    /// Breakdown: the groups, most calls first.
    pub breakdown: Vec<McpErrorGroupRow>,
    /// Breakdown: whether more groups exist than `breakdown` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// The calls of one self-reported client version.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpClientVersionRow {
    /// The client's name as it reported it.
    pub client_name: String,
    /// The client's version as it reported it.
    pub client_version: String,
    /// Its calls.
    pub calls: SafeU64,
    /// When its last call started.
    #[schema(format = DateTime)]
    pub last_seen: String,
}

/// `GET projects/{project_id}/mcp/clients`: clients by name (from the rollup) or by version (from the
/// operations), as `view` asks.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpClientList {
    /// The section this answer holds.
    pub view: McpView,
    /// Always true: client names and versions are what the clients reported about themselves.
    pub self_reported: bool,
    /// Summary: clients by name, most calls first.
    pub data: Vec<McpClientRow>,
    /// Breakdown: client versions, most calls first.
    pub versions: Vec<McpClientVersionRow>,
    /// Breakdown: whether more versions exist than `versions` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One user of the MCP servers.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpUserRow {
    /// The user id from the SDK's identify hook; empty when absent.
    pub user_id: String,
    /// The hashed token subject, 32 lowercase hex characters; null when absent.
    #[schema(required = true)]
    pub subject_hash: Option<String>,
    /// How the identity was established, at its strongest.
    pub identity_source: IdentitySource,
    /// Its calls.
    pub calls: SafeU64,
    /// Its failed calls.
    pub errors: SafeU64,
    /// Its distinct MCP sessions.
    pub sessions: SafeU64,
    /// First seen.
    #[schema(format = DateTime)]
    pub first_seen: String,
    /// Last seen.
    #[schema(format = DateTime)]
    pub last_seen: String,
}

/// `GET projects/{project_id}/mcp/users`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpUserList {
    /// The users, last seen first.
    pub data: Vec<McpUserRow>,
    /// Whether more users exist than `data` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One MCP session: the operations of one group key (contract 9.4).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpSessionRow {
    /// The group key.
    pub group_key: String,
    /// What the group key is.
    pub group_key_kind: GroupKeyKind,
    /// Its calls.
    pub calls: SafeU64,
    /// Its failed calls.
    pub errors: SafeU64,
    /// A self-reported client name of the session.
    pub client_name: String,
    /// First seen.
    #[schema(format = DateTime)]
    pub first_seen: String,
    /// Last seen.
    #[schema(format = DateTime)]
    pub last_seen: String,
}

/// `GET projects/{project_id}/mcp/sessions`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpSessionList {
    /// The sessions, last seen first.
    pub data: Vec<McpSessionRow>,
    /// Whether more sessions exist than `data` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One MCP operation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpOperationRow {
    /// 32 lowercase hex characters.
    pub trace_id: String,
    /// 16 lowercase hex characters.
    pub span_id: String,
    /// The parent span; null when the operation has none.
    #[schema(required = true)]
    pub parent_span_id: Option<String>,
    /// Start.
    #[schema(format = DateTime)]
    pub start_time: String,
    /// Duration in microseconds.
    pub duration_us: SafeU64,
    /// The server.
    pub server_name: String,
    /// The server's version.
    pub server_version: String,
    /// The deploy.
    pub deploy_id: String,
    /// Who observed it.
    pub capture_source: CaptureSource,
    /// The protocol era.
    pub era: Era,
    /// The transport.
    pub transport: Transport,
    /// The method.
    pub method: String,
    /// What the method targets.
    pub target_kind: McpTargetKind,
    /// The tool or prompt name, or the resource template.
    pub target_name: String,
    /// The outcome.
    pub status: McpStatus,
    /// The error class.
    pub error_class: ErrorClass,
    /// The subclass an SDK annotated.
    pub error_subclass: ErrorSubclass,
    /// Whether the caller caused the error (contract 9.2).
    pub caller_fault: bool,
    /// The JSON-RPC error code; null when there was none.
    #[schema(required = true)]
    pub jsonrpc_code: Option<i32>,
    /// The semantic-convention error type.
    pub error_type: String,
    /// The scrubbed error message.
    pub error_message: String,
    /// The client's name as it reported it.
    pub client_name: String,
    /// The client's version as it reported it.
    pub client_version: String,
    /// The user id from the SDK's identify hook.
    pub user_id: String,
    /// The hashed token subject, 32 lowercase hex characters; null when absent.
    #[schema(required = true)]
    pub user_subject_hash: Option<String>,
    /// How the identity was established.
    pub identity_source: IdentitySource,
    /// The MCP session's group key.
    pub group_key: String,
    /// What the group key is.
    pub group_key_kind: GroupKeyKind,
    /// Request bytes; 0 when unknown.
    pub request_bytes: SafeU64,
    /// Response bytes; 0 when unknown.
    pub response_bytes: SafeU64,
    /// Whether arguments and results were captured.
    pub content_captured: bool,
    /// What the result was.
    pub result_type: ResultType,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
}

/// `GET projects/{project_id}/mcp/operations`: one page, newest first.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpOperationPage {
    /// The operations.
    pub data: Vec<McpOperationRow>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The capture source of each server.
    pub capture_sources: Vec<McpServerSource>,
    /// How complete and how fresh the page is; `as_of` is the one the first page fixed.
    pub meta: Meta,
}

/// The first bytes of captured content. The whole field is read through the span payload route.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpContentPreview {
    /// The first 16 KiB as text; a character cut at the end is replaced.
    pub preview: String,
    /// The stored length in bytes.
    pub bytes: SafeU64,
    /// Whether the field is longer than the preview.
    pub truncated: bool,
}

/// One link of a multi-round exchange (contract 9.6).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpChainLink {
    /// 16 lowercase hex characters.
    pub span_id: String,
    /// Start.
    #[schema(format = DateTime)]
    pub start_time: String,
    /// The round: the sender's count when it sent one, else the position in the chain from 1.
    pub round: u16,
    /// The method.
    pub method: String,
    /// The target.
    pub target_name: String,
    /// What the result was.
    pub result_type: ResultType,
}

/// Everything known about one operation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpOperation {
    /// The columns the list shows.
    pub operation: McpOperationRow,
    /// The side.
    pub side: McpSide,
    /// The server instance.
    pub instance_id: String,
    /// The protocol version.
    pub protocol_version: String,
    /// The JSON-RPC id.
    pub jsonrpc_id: String,
    /// The HTTP status; null when the transport is not HTTP.
    #[schema(required = true)]
    pub http_status: Option<u16>,
    /// The OAuth client id.
    pub oauth_client_id: String,
    /// The tenant.
    pub tenant: String,
    /// Captured arguments; null when nothing was captured or the content expired.
    #[schema(required = true)]
    pub args: Option<McpContentPreview>,
    /// The captured result; null when nothing was captured or the content expired.
    #[schema(required = true)]
    pub result: Option<McpContentPreview>,
    /// The other side's row of the call in the same trace; under double capture the best-ranked capture
    /// source's; null when it was not captured.
    #[schema(required = true)]
    pub other_side: Option<McpOperationRow>,
    /// The multi-round exchange the operation belongs to, oldest first; empty when it is not part of one.
    pub chain: Vec<McpChainLink>,
}

/// `GET projects/{project_id}/mcp/operations/{trace_id}/{span_id}`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpOperationDetail {
    /// The operation; null only when the answer is not whole (`meta` says why), so it may yet appear.
    #[schema(required = true)]
    pub operation: Option<McpOperation>,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// `GET projects/{project_id}/mcp/deploys`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpDeployList {
    /// The deploys seen in the window, newest first.
    pub data: Vec<McpDeployRow>,
    /// The capture source of each server, chosen among its deploys' own.
    pub capture_sources: Vec<McpServerSource>,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// One tool on both sides of a compare.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpToolDeltaRow {
    /// The tool.
    pub tool_name: String,
    /// Its calls on the baseline side.
    pub baseline: McpStats,
    /// Its calls on the candidate side.
    pub candidate: McpStats,
}

/// `GET projects/{project_id}/mcp/compare`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpCompare {
    /// The server compared.
    pub server_name: String,
    /// Each tool called on either side, by name.
    pub tools: Vec<McpToolDeltaRow>,
    /// Tools listed on the candidate side only.
    pub added: Vec<String>,
    /// Tools listed on the baseline side only.
    pub removed: Vec<String>,
    /// Tools listed on both sides whose latest schema hash differs.
    pub changed: Vec<String>,
    /// The capture source of the server.
    pub capture_sources: Vec<McpServerSource>,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}
