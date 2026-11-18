package cruciblemcp

// Span keys of the MCP wire attribute contract (contract 9.1): the OTel MCP semantic-convention keys and the
// crucible.mcp.* extension keys.
const (
	keyMethod          = "mcp.method.name"
	keyProtocolVersion = "mcp.protocol.version"
	keySessionID       = "mcp.session.id"
	keyToolName        = "gen_ai.tool.name"
	keyPromptName      = "gen_ai.prompt.name"
	keyErrorType       = "error.type"
	keyRPCStatusCode   = "rpc.response.status_code"
	keyUserID          = "user.id"
	keyUserAgent       = "user_agent.original"
	keyToolArguments   = "gen_ai.tool.call.arguments"
	keyToolResult      = "gen_ai.tool.call.result"

	keyCaptureSource     = "crucible.mcp.capture_source"
	keyEra               = "crucible.mcp.era"
	keyTransport         = "crucible.mcp.transport"
	keyIsNotification    = "crucible.mcp.is_notification"
	keyErrorClass        = "crucible.mcp.error.class"
	keyErrorSubclass     = "crucible.mcp.error.subclass"
	keyResultType        = "crucible.mcp.result_type"
	keyStateIn           = "crucible.mcp.mrtr.state_in"
	keyStateOut          = "crucible.mcp.mrtr.state_out"
	keyProgressCount     = "crucible.mcp.progress.count"
	keyCancelSource      = "crucible.mcp.cancel.source"
	keyTimedOut          = "crucible.mcp.timed_out"
	keyClientName        = "crucible.mcp.client.name"
	keyClientVersion     = "crucible.mcp.client.version"
	keyOAuthClientID     = "crucible.mcp.oauth.client_id"
	keyTenant            = "crucible.mcp.tenant"
	keyHandleID          = "crucible.mcp.handle.id"
	keyUserIssuer        = "crucible.mcp.user.issuer"
	keyUserSubject       = "crucible.mcp.user.subject"
	keyIdentitySource    = "crucible.mcp.identity.source"
	keyRequestBytes      = "crucible.mcp.request.bytes"
	keyContentBlocks     = "crucible.mcp.content.blocks."
	keyContentCaptured   = "crucible.mcp.content.captured"
	keyContentSampleRate = "crucible.mcp.content.sample_rate"
	keyContentTruncation = "crucible.mcp.content.truncation"
	keyServerName        = "crucible.mcp.server.name"
	keyServerVersion     = "crucible.mcp.server.version"
)

// modernVersion is the first protocol version of the modern era (contract 9.1).
const modernVersion = "2026-07-28"
