"""Wire keys the SDK sends (contracts 9.1 and 10.6). Renaming one is a breaking change."""

# Contract 10.6: fields without a semantic convention.
OBSERVATION_TYPE = "crucible.observation.type"
SESSION_ID = "crucible.session.id"
USER_ID = "crucible.user.id"
TAGS = "crucible.tags"
TRACE_TAGS = "crucible.trace.tags"
METADATA_PREFIX = "crucible.metadata."
INPUT = "crucible.input"
OUTPUT = "crucible.output"
COST_PREFIX = "crucible.cost."
DEPLOY_ID = "crucible.deploy.id"

# OpenTelemetry GenAI semantic conventions read by dialect 0.
REQUEST_MODEL = "gen_ai.request.model"
RESPONSE_MODEL = "gen_ai.response.model"
PROVIDER_NAME = "gen_ai.provider.name"
USAGE = {
    "input": "gen_ai.usage.input_tokens",
    "output": "gen_ai.usage.output_tokens",
    "cache_read": "gen_ai.usage.cache_read.input_tokens",
    "cache_write": "gen_ai.usage.cache_creation.input_tokens",
    "reasoning": "gen_ai.usage.reasoning.output_tokens",
}

# Resource keys.
SERVICE_NAME = "service.name"
SERVICE_VERSION = "service.version"
SERVICE_INSTANCE_ID = "service.instance.id"
ENVIRONMENT = "deployment.environment.name"

# Contract 9.1: MCP operations. The built-in mcp span sets the method, protocol version, request id, tool and
# prompt names and `error.type`; the SDK adds the rest.
MCP_METHOD = "mcp.method.name"
MCP_SESSION_ID = "mcp.session.id"
MCP_PROTOCOL_VERSION = "mcp.protocol.version"
TOOL_NAME = "gen_ai.tool.name"
TOOL_CALL_ARGUMENTS = "gen_ai.tool.call.arguments"
TOOL_CALL_RESULT = "gen_ai.tool.call.result"
CAPTURE_SOURCE = "crucible.mcp.capture_source"
ERA = "crucible.mcp.era"
TRANSPORT = "crucible.mcp.transport"
IS_NOTIFICATION = "crucible.mcp.is_notification"
ERROR_CLASS = "crucible.mcp.error.class"
ERROR_SUBCLASS = "crucible.mcp.error.subclass"
RESULT_TYPE = "crucible.mcp.result_type"
MRTR_STATE_IN = "crucible.mcp.mrtr.state_in"
MRTR_STATE_OUT = "crucible.mcp.mrtr.state_out"
CANCEL_SOURCE = "crucible.mcp.cancel.source"
CLIENT_NAME = "crucible.mcp.client.name"
CLIENT_VERSION = "crucible.mcp.client.version"
CLIENT_CAPABILITIES_HASH = "crucible.mcp.client.capabilities_hash"
OAUTH_CLIENT_ID = "crucible.mcp.oauth.client_id"
TENANT = "crucible.mcp.tenant"
MCP_USER_ID = "user.id"
HANDLE_ID = "crucible.mcp.handle.id"
USER_ISSUER = "crucible.mcp.user.issuer"
USER_SUBJECT = "crucible.mcp.user.subject"
IDENTITY_SOURCE = "crucible.mcp.identity.source"
REQUEST_BYTES = "crucible.mcp.request.bytes"
CONTENT_BLOCKS_PREFIX = "crucible.mcp.content.blocks."
CONTENT_CAPTURED = "crucible.mcp.content.captured"
CONTENT_SAMPLE_RATE = "crucible.mcp.content.sample_rate"
CONTENT_TRUNCATION = "crucible.mcp.content.truncation"
SERVER_NAME = "crucible.mcp.server.name"
SERVER_VERSION = "crucible.mcp.server.version"
CLIENT_IP_PREFIX = "crucible.mcp.client.ip_prefix"
USER_AGENT = "user_agent.original"

# The keys `identity="none"` suppresses (contract 9.3).
IDENTITY_KEYS = frozenset(
    {
        OAUTH_CLIENT_ID,
        TENANT,
        MCP_USER_ID,
        HANDLE_ID,
        USER_ISSUER,
        USER_SUBJECT,
        IDENTITY_SOURCE,
        CLIENT_IP_PREFIX,
        USER_AGENT,
    }
)
