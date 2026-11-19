//! Caps named by the contract (sections 3.1 to 3.10 and 5.1).
//!
//! String caps are in bytes of UTF-8: an over-long string is cut at the longest prefix that fits and ends on a
//! character boundary ([`crate::truncate_str`]). `*_MAX_ITEMS` and `*_MAX_KEYS` caps count list items or map
//! keys. [`MAX_FIELD_BYTES`] and [`MAX_SPAN_BYTES`] are deployment defaults; a project snapshot pins the values
//! in force ([`crate::PayloadCaps`]).

const KIB: usize = 1_024;
const MIB: usize = 1_024 * KIB;

/// Span `name`.
pub const SPAN_NAME_BYTES: usize = 1_024;
/// Span `status_message`.
pub const STATUS_MESSAGE_BYTES: usize = 8 * KIB;
/// `service_name`, `service_version`, `scope_name` and `scope_version`.
pub const SERVICE_BYTES: usize = 256;
/// `telemetry_sdk_name`, `telemetry_sdk_language`, `telemetry_sdk_version`, `sdk_name` and `sdk_version`.
pub const SDK_BYTES: usize = 128;
/// `environment`.
pub const ENVIRONMENT_BYTES: usize = 128;
/// `release` and `version`.
pub const RELEASE_BYTES: usize = 256;
/// `trace_name`, `user_id` and `session_id` of a span.
pub const TRACE_ATTRIBUTE_BYTES: usize = 512;
/// Items in `trace_tags`, and items in `tags`.
pub const TAGS_MAX_ITEMS: usize = 64;
/// One item of `trace_tags` or `tags`.
pub const TAG_BYTES: usize = 256;
/// `model_requested`, `model_response` and `provider`.
pub const MODEL_BYTES: usize = 256;
/// Items in `finish_reasons`.
pub const FINISH_REASONS_MAX_ITEMS: usize = 16;
/// One finish reason.
pub const FINISH_REASON_BYTES: usize = 64;
/// Keys in `usage` (and so items in `cost`).
pub const USAGE_MAX_KEYS: usize = 32;
/// `prompt_name`.
pub const PROMPT_NAME_BYTES: usize = 256;
/// `prompt_version`.
pub const PROMPT_VERSION_BYTES: usize = 64;
/// Items in `tool_names`.
pub const TOOL_NAMES_MAX_ITEMS: usize = 128;
/// One item of `tool_names`.
pub const TOOL_NAME_BYTES: usize = 256;
/// `input_preview` and `output_preview`.
pub const PREVIEW_BYTES: usize = 1_024;

/// Default per-field payload cap (`ingest.max_field_bytes`).
pub const MAX_FIELD_BYTES: usize = 8 * MIB;
/// Default per-span cap over the sum of the payload fields (`ingest.max_span_bytes`).
pub const MAX_SPAN_BYTES: usize = 16 * MIB;
/// Keys in `metadata`.
pub const METADATA_MAX_KEYS: usize = 256;
/// One `metadata_kv` item (`key=value`).
pub const METADATA_KV_ITEM_BYTES: usize = 1_024;
/// Items in `events`.
pub const EVENTS_MAX_ITEMS: usize = 128;
/// Items in `links`.
pub const LINKS_MAX_ITEMS: usize = 32;
/// `model_parameters`.
pub const MODEL_PARAMETERS_BYTES: usize = 64 * KIB;
/// `trace_state`.
pub const TRACE_STATE_BYTES: usize = 4 * KIB;

/// MCP `server_name`, also used for the tool inventory and deploy records.
pub const MCP_SERVER_NAME_BYTES: usize = 256;
/// MCP `server_version`.
pub const MCP_SERVER_VERSION_BYTES: usize = 128;
/// MCP `deploy_id`, also used for the tool inventory and deploy records.
pub const MCP_DEPLOY_ID_BYTES: usize = 128;
/// MCP `instance_id`.
pub const MCP_INSTANCE_ID_BYTES: usize = 128;
/// MCP `protocol_version`.
pub const MCP_PROTOCOL_VERSION_BYTES: usize = 32;
/// MCP `method`.
pub const MCP_METHOD_BYTES: usize = 128;
/// MCP `target_name`.
pub const MCP_TARGET_NAME_BYTES: usize = 512;
/// MCP `jsonrpc_id`.
pub const MCP_JSONRPC_ID_BYTES: usize = 128;
/// MCP `task_id` and `subscription_id`.
pub const MCP_TASK_ID_BYTES: usize = 128;
/// MCP `error_type`.
pub const MCP_ERROR_TYPE_BYTES: usize = 128;
/// MCP `error_message`.
pub const MCP_ERROR_MESSAGE_BYTES: usize = 2 * KIB;
/// MCP `client_name`.
pub const MCP_CLIENT_NAME_BYTES: usize = 256;
/// MCP `client_version`.
pub const MCP_CLIENT_VERSION_BYTES: usize = 128;
/// MCP `oauth_client_id`.
pub const MCP_OAUTH_CLIENT_ID_BYTES: usize = 256;
/// MCP `user_issuer`.
pub const MCP_USER_ISSUER_BYTES: usize = 512;
/// MCP `user_id`.
pub const MCP_USER_ID_BYTES: usize = 512;
/// MCP `tenant`.
pub const MCP_TENANT_BYTES: usize = 256;
/// MCP `handle_id`.
pub const MCP_HANDLE_ID_BYTES: usize = 256;
/// MCP `group_key`.
pub const MCP_GROUP_KEY_BYTES: usize = 256;
/// MCP `ip_prefix`.
pub const MCP_IP_PREFIX_BYTES: usize = 64;
/// MCP `user_agent`.
pub const MCP_USER_AGENT_BYTES: usize = 512;
/// Keys in MCP `content_blocks`.
pub const MCP_CONTENT_BLOCKS_MAX_KEYS: usize = 16;
/// Default cut of MCP `args` and `result` in the SDKs and the proxy.
pub const MCP_CONTENT_BYTES: usize = 16 * KIB;
/// Hard cap of MCP `args` and `result` at the normalizer.
pub const MCP_CONTENT_MAX_BYTES: usize = 8 * MIB;
/// Tool inventory `tool_name`.
pub const MCP_INVENTORY_TOOL_NAME_BYTES: usize = 512;
/// Tool inventory rows kept per frame; the first in wire order are kept (contract 5.1).
pub const MCP_INVENTORY_MAX_ROWS_PER_FRAME: usize = 65_536;

/// Fact `key`.
pub const FACT_KEY_BYTES: usize = 256;
/// Score `value_str`.
pub const SCORE_VALUE_STR_BYTES: usize = KIB;
/// Score `comment`.
pub const SCORE_COMMENT_BYTES: usize = 4 * KIB;
/// Score `metadata`.
pub const SCORE_METADATA_BYTES: usize = 16 * KIB;
/// Correction `corrected`.
pub const CORRECTION_BYTES: usize = 8 * MIB;
/// Correction `note`.
pub const CORRECTION_NOTE_BYTES: usize = 4 * KIB;

/// Distinct `environments` kept by a merged `trace_stats` row (`groupUniqArrayArray(16)`).
pub const TRACE_ENVIRONMENTS_MAX_ITEMS: usize = 16;
/// Distinct `user_ids` kept by a merged `session_rollup` row (`groupUniqArrayArray(8)`).
pub const SESSION_USER_IDS_MAX_ITEMS: usize = 8;
/// Distinct `capture_sources` kept by a merged `mcp_tool_inventory` row (`groupUniqArrayArray(4)`).
pub const INVENTORY_CAPTURE_SOURCES_MAX_ITEMS: usize = 4;
