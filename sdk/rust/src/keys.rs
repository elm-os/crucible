//! The span and resource keys the SDK writes: contract 10.6 for agents, the OTel GenAI and MCP semantic
//! conventions, and the `crucible.mcp.*` extension keys of contract 9.1.

// Resource (contracts 3.1, 9.1, 9.7, 10.6).
pub(crate) const SERVICE_NAME: &str = "service.name";
pub(crate) const SERVICE_VERSION: &str = "service.version";
pub(crate) const SERVICE_INSTANCE_ID: &str = "service.instance.id";
pub(crate) const DEPLOYMENT_ENVIRONMENT: &str = "deployment.environment.name";
pub(crate) const DEPLOY_ID: &str = "crucible.deploy.id";

// Agent spans (contract 10.6).
pub(crate) const OBSERVATION_TYPE: &str = "crucible.observation.type";
pub(crate) const SESSION_ID: &str = "crucible.session.id";
pub(crate) const USER_ID: &str = "crucible.user.id";
pub(crate) const TAGS: &str = "crucible.tags";
pub(crate) const METADATA_PREFIX: &str = "crucible.metadata.";
pub(crate) const RELEASE: &str = "crucible.release";
pub(crate) const INPUT: &str = "crucible.input";
pub(crate) const OUTPUT: &str = "crucible.output";
pub(crate) const COST_PREFIX: &str = "crucible.cost.";
pub(crate) const REQUEST_MODEL: &str = "gen_ai.request.model";
pub(crate) const INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub(crate) const OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub(crate) const CACHE_READ_TOKENS: &str = "gen_ai.usage.cache_read.input_tokens";
pub(crate) const CACHE_WRITE_TOKENS: &str = "gen_ai.usage.cache_creation.input_tokens";
pub(crate) const REASONING_TOKENS: &str = "gen_ai.usage.reasoning.output_tokens";

/// MCP operations (contract 9.1). The keys under `cfg(feature = "mcp")` are written only by the server wrapper.
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
pub(crate) mod mcp {
    pub(crate) const METHOD: &str = "mcp.method.name";
    pub(crate) const REQUEST_ID: &str = "jsonrpc.request.id";
    pub(crate) const PROTOCOL_VERSION: &str = "mcp.protocol.version";
    #[cfg(feature = "mcp")]
    pub(crate) const SESSION_ID: &str = "mcp.session.id";
    pub(crate) const TOOL_NAME: &str = "gen_ai.tool.name";
    pub(crate) const PROMPT_NAME: &str = "gen_ai.prompt.name";
    pub(crate) const ERROR_TYPE: &str = "error.type";
    pub(crate) const RPC_STATUS_CODE: &str = "rpc.response.status_code";
    pub(crate) const CAPTURE_SOURCE: &str = "crucible.mcp.capture_source";
    pub(crate) const ERA: &str = "crucible.mcp.era";
    pub(crate) const TRANSPORT: &str = "crucible.mcp.transport";
    #[cfg(feature = "mcp")]
    pub(crate) const IS_NOTIFICATION: &str = "crucible.mcp.is_notification";
    pub(crate) const ERROR_CLASS: &str = "crucible.mcp.error.class";
    pub(crate) const ERROR_SUBCLASS: &str = "crucible.mcp.error.subclass";
    pub(crate) const RESULT_TYPE: &str = "crucible.mcp.result_type";
    #[cfg(feature = "mcp")]
    pub(crate) const STATE_IN: &str = "crucible.mcp.mrtr.state_in";
    pub(crate) const STATE_OUT: &str = "crucible.mcp.mrtr.state_out";
    pub(crate) const TASK_ID: &str = "crucible.mcp.task.id";
    pub(crate) const CANCEL_SOURCE: &str = "crucible.mcp.cancel.source";
    pub(crate) const CONTENT_BLOCKS_PREFIX: &str = "crucible.mcp.content.blocks.";
    pub(crate) const SERVER_NAME: &str = "crucible.mcp.server.name";
    pub(crate) const SERVER_VERSION: &str = "crucible.mcp.server.version";

    #[cfg(feature = "mcp")]
    pub(crate) const PROGRESS_COUNT: &str = "crucible.mcp.progress.count";
    #[cfg(feature = "mcp")]
    pub(crate) const CLIENT_NAME: &str = "crucible.mcp.client.name";
    #[cfg(feature = "mcp")]
    pub(crate) const CLIENT_VERSION: &str = "crucible.mcp.client.version";
    #[cfg(feature = "mcp")]
    pub(crate) const USER_ID: &str = "user.id";
    #[cfg(feature = "mcp")]
    pub(crate) const TENANT: &str = "crucible.mcp.tenant";
    #[cfg(feature = "mcp")]
    pub(crate) const HANDLE_ID: &str = "crucible.mcp.handle.id";
    #[cfg(feature = "mcp")]
    pub(crate) const USER_ISSUER: &str = "crucible.mcp.user.issuer";
    #[cfg(feature = "mcp")]
    pub(crate) const USER_SUBJECT: &str = "crucible.mcp.user.subject";
    #[cfg(feature = "mcp")]
    pub(crate) const IDENTITY_SOURCE: &str = "crucible.mcp.identity.source";
    #[cfg(feature = "mcp")]
    pub(crate) const REQUEST_BYTES: &str = "crucible.mcp.request.bytes";
    #[cfg(feature = "mcp")]
    pub(crate) const USER_AGENT: &str = "user_agent.original";
    #[cfg(feature = "mcp")]
    pub(crate) const TOOL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
    #[cfg(feature = "mcp")]
    pub(crate) const TOOL_RESULT: &str = "gen_ai.tool.call.result";
    #[cfg(feature = "mcp")]
    pub(crate) const CONTENT_CAPTURED: &str = "crucible.mcp.content.captured";
    #[cfg(feature = "mcp")]
    pub(crate) const CONTENT_SAMPLE_RATE: &str = "crucible.mcp.content.sample_rate";
    #[cfg(feature = "mcp")]
    pub(crate) const CONTENT_TRUNCATION: &str = "crucible.mcp.content.truncation";
}
