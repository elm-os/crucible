//! The key registry of the MCP wire attribute contract (contract 9.1): the OTel MCP semantic-convention keys and
//! the `crucible.mcp.*` extension keys that SDKs and the proxy put on a span, and that the normalizer turns into
//! an [`crate::McpOpRecord`]. A span is an MCP operation if and only if it carries [`METHOD`].
//!
//! Keys are span attributes unless their name starts with `RESOURCE_`, in which case they are resource attributes.

/// `method`. The key that makes a span an MCP operation.
pub const METHOD: &str = "mcp.method.name";
/// `protocol_version`.
pub const PROTOCOL_VERSION: &str = "mcp.protocol.version";
/// `session_key`: the legacy session id, hashed by the sender or else by the normalizer (contract 9.4).
pub const SESSION_ID: &str = "mcp.session.id";
/// `side`, when the span kind is neither server nor client.
pub const SIDE: &str = "crucible.mcp.side";
/// `capture_source`.
pub const CAPTURE_SOURCE: &str = "crucible.mcp.capture_source";
/// `target_name` of a tool.
pub const TOOL_NAME: &str = "gen_ai.tool.name";
/// `target_name` of a prompt.
pub const PROMPT_NAME: &str = "gen_ai.prompt.name";
/// `target_name` of a resource: its URI template.
pub const RESOURCE_TEMPLATE: &str = "crucible.mcp.resource.template";
/// `era`.
pub const ERA: &str = "crucible.mcp.era";
/// `transport`.
pub const TRANSPORT: &str = "crucible.mcp.transport";
/// `transport`, when [`TRANSPORT`] is absent: `pipe` is stdio and `tcp` is streamable HTTP.
pub const NETWORK_TRANSPORT: &str = "network.transport";
/// `jsonrpc_id`.
pub const JSONRPC_ID: &str = "jsonrpc.request.id";
/// `direction`.
pub const DIRECTION: &str = "crucible.mcp.direction";
/// `is_notification`.
pub const IS_NOTIFICATION: &str = "crucible.mcp.is_notification";
/// `is_batch`.
pub const IS_BATCH: &str = "crucible.mcp.is_batch";
/// `error_type`: the JSON-RPC code as text, `tool_error`, or what failed.
pub const ERROR_TYPE: &str = "error.type";
/// `jsonrpc_code`.
pub const RPC_STATUS_CODE: &str = "rpc.response.status_code";
/// The error class the capture source observed or an SDK annotation chose (contract 9.2).
pub const ERROR_CLASS: &str = "crucible.mcp.error.class";
/// `error_subclass`.
pub const ERROR_SUBCLASS: &str = "crucible.mcp.error.subclass";
/// `result_type`.
pub const RESULT_TYPE: &str = "crucible.mcp.result_type";
/// `mrtr_round`.
pub const MRTR_ROUND: &str = "crucible.mcp.mrtr.round";
/// `mrtr_state_in`: 32 hex characters (contract 9.6).
pub const MRTR_STATE_IN: &str = "crucible.mcp.mrtr.state_in";
/// `mrtr_state_out`: 32 hex characters (contract 9.6).
pub const MRTR_STATE_OUT: &str = "crucible.mcp.mrtr.state_out";
/// `task_id`.
pub const TASK_ID: &str = "crucible.mcp.task.id";
/// `subscription_id`.
pub const SUBSCRIPTION_ID: &str = "crucible.mcp.subscription.id";
/// `progress_count`; absent means unobserved.
pub const PROGRESS_COUNT: &str = "crucible.mcp.progress.count";
/// `cancel_source`.
pub const CANCEL_SOURCE: &str = "crucible.mcp.cancel.source";
/// `timed_out`.
pub const TIMED_OUT: &str = "crucible.mcp.timed_out";
/// `client_name`, self-reported.
pub const CLIENT_NAME: &str = "crucible.mcp.client.name";
/// `client_version`, self-reported.
pub const CLIENT_VERSION: &str = "crucible.mcp.client.version";
/// `client_capabilities_hash`: an integer, or its decimal or 16-digit hexadecimal text.
pub const CLIENT_CAPABILITIES_HASH: &str = "crucible.mcp.client.capabilities_hash";
/// `oauth_client_id`.
pub const OAUTH_CLIENT_ID: &str = "crucible.mcp.oauth.client_id";
/// `tenant`.
pub const TENANT: &str = "crucible.mcp.tenant";
/// `user_id`, from the `identify` hook.
pub const USER_ID: &str = "user.id";
/// `handle_id` (contract 9.8).
pub const HANDLE_ID: &str = "crucible.mcp.handle.id";
/// `user_issuer`.
pub const USER_ISSUER: &str = "crucible.mcp.user.issuer";
/// The raw user subject. It is hashed in stage B and never stored (contract 9.3).
pub const USER_SUBJECT: &str = "crucible.mcp.user.subject";
/// `identity_source`.
pub const IDENTITY_SOURCE: &str = "crucible.mcp.identity.source";
/// `request_bytes`.
pub const REQUEST_BYTES: &str = "crucible.mcp.request.bytes";
/// `response_bytes`.
pub const RESPONSE_BYTES: &str = "crucible.mcp.response.bytes";
/// Prefix of the `content_blocks` counts: `crucible.mcp.content.blocks.<type>`.
pub const CONTENT_BLOCKS_PREFIX: &str = "crucible.mcp.content.blocks.";
/// `args` (opt-in content).
pub const TOOL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
/// `result` (opt-in content).
pub const TOOL_RESULT: &str = "gen_ai.tool.call.result";
/// `content_captured`.
pub const CONTENT_CAPTURED: &str = "crucible.mcp.content.captured";
/// `content_sample_rate`.
pub const CONTENT_SAMPLE_RATE: &str = "crucible.mcp.content.sample_rate";
/// `content_truncation`.
pub const CONTENT_TRUNCATION: &str = "crucible.mcp.content.truncation";
/// `server_name`; else the resource's `service.name`.
pub const SERVER_NAME: &str = "crucible.mcp.server.name";
/// `server_version`; else the resource's `service.version`.
pub const SERVER_VERSION: &str = "crucible.mcp.server.version";
/// `ip_prefix`: a /24 or /48 prefix, truncated by the sender.
pub const CLIENT_IP_PREFIX: &str = "crucible.mcp.client.ip_prefix";
/// `user_agent`.
pub const USER_AGENT: &str = "user_agent.original";
/// `http_status`.
pub const HTTP_STATUS: &str = "http.response.status_code";
/// The tools a `tools/list` or `server/discover` result lists, or an SDK's snapshot of its registered tools: the
/// JSON text of an array of tool objects as the protocol sends them (`name`, `inputSchema`, `outputSchema`). The
/// normalizer makes one [`crate::McpToolInventoryRecord`] per tool and computes its `schema_hash`.
pub const TOOLS: &str = "crucible.mcp.tools";

/// `server_name` when [`SERVER_NAME`] is absent.
pub const RESOURCE_SERVICE_NAME: &str = "service.name";
/// `server_version` when [`SERVER_VERSION`] is absent.
pub const RESOURCE_SERVICE_VERSION: &str = "service.version";
/// `deploy_id` (contract 9.7).
pub const RESOURCE_DEPLOY_ID: &str = "crucible.deploy.id";
/// `instance_id`.
pub const RESOURCE_INSTANCE_ID: &str = "service.instance.id";

/// The first protocol version of the modern era: a version at or after it is modern (contract 9.1).
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// Span attributes that must never be stored, in any record or residual attribute (contracts 3.5 and 9.1): a
/// client address, and request headers that carry credentials. The normalizer drops them from every span.
pub const NEVER_STORED: [&str; 5] = [
    "client.address",
    "network.peer.address",
    "http.request.header.authorization",
    "http.request.header.cookie",
    "http.response.header.set-cookie",
];

/// Prefixes of span attributes that must never be stored: the `Mcp-Param-*` request headers and baggage.
pub const NEVER_STORED_PREFIXES: [&str; 2] = ["http.request.header.mcp-param-", "baggage."];

/// Whether a span attribute must never be stored, whatever span carries it: the keys and prefixes above, the raw
/// user subject and the session id (which is stored only as its hash, in `session_key`). Keys compare without
/// regard to ASCII case, as header names do.
pub fn never_stored(key: &str) -> bool {
    let is = |name: &str| key.eq_ignore_ascii_case(name);
    let starts = |prefix: &str| {
        key.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    is(USER_SUBJECT)
        || is(SESSION_ID)
        || NEVER_STORED.iter().any(|name| is(name))
        || NEVER_STORED_PREFIXES.iter().any(|prefix| starts(prefix))
}

/// Whether the projector consumes a span attribute of an MCP operation, so that it leaves the residual
/// `attributes` (contract 9.1). Keys that the span mapping reads as well (`gen_ai.tool.name`, `user.id`, the tool
/// call arguments and result) are consumed there.
pub fn consumed(key: &str) -> bool {
    const KEYS: [&str; 42] = [
        METHOD,
        PROTOCOL_VERSION,
        SESSION_ID,
        SIDE,
        CAPTURE_SOURCE,
        TOOL_NAME,
        PROMPT_NAME,
        RESOURCE_TEMPLATE,
        ERA,
        TRANSPORT,
        NETWORK_TRANSPORT,
        JSONRPC_ID,
        DIRECTION,
        IS_NOTIFICATION,
        IS_BATCH,
        ERROR_TYPE,
        RPC_STATUS_CODE,
        ERROR_CLASS,
        ERROR_SUBCLASS,
        RESULT_TYPE,
        MRTR_ROUND,
        MRTR_STATE_IN,
        MRTR_STATE_OUT,
        TASK_ID,
        SUBSCRIPTION_ID,
        PROGRESS_COUNT,
        CANCEL_SOURCE,
        TIMED_OUT,
        CLIENT_NAME,
        CLIENT_VERSION,
        CLIENT_CAPABILITIES_HASH,
        OAUTH_CLIENT_ID,
        TENANT,
        HANDLE_ID,
        USER_ISSUER,
        USER_SUBJECT,
        IDENTITY_SOURCE,
        REQUEST_BYTES,
        RESPONSE_BYTES,
        CONTENT_CAPTURED,
        CONTENT_SAMPLE_RATE,
        CONTENT_TRUNCATION,
    ];
    const MORE: [&str; 6] = [
        SERVER_NAME,
        SERVER_VERSION,
        CLIENT_IP_PREFIX,
        USER_AGENT,
        HTTP_STATUS,
        TOOLS,
    ];
    KEYS.contains(&key) || MORE.contains(&key) || key.starts_with(CONTENT_BLOCKS_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subject_and_the_session_id_are_never_stored_and_consumed() {
        for key in [USER_SUBJECT, SESSION_ID] {
            assert!(never_stored(key) && consumed(key), "{key}");
        }
        assert!(never_stored("Client.Address"));
        assert!(never_stored("http.request.header.Mcp-Param-Region"));
        assert!(never_stored("baggage.tenant"));
        assert!(!never_stored(METHOD));
        assert!(!never_stored("http.request.header.accept"));
    }

    #[test]
    fn content_block_counts_are_consumed_by_prefix() {
        assert!(consumed("crucible.mcp.content.blocks.text"));
        assert!(consumed(METHOD) && consumed(TOOLS) && consumed(HTTP_STATUS));
        assert!(!consumed("sim.ordinal"));
        // The span mapping consumes these.
        assert!(!consumed(USER_ID) && !consumed(TOOL_ARGUMENTS) && !consumed(TOOL_RESULT));
    }
}
