//! The rmcp server handler: the server's description, `tools/list` from the registry, and `tools/call`, which
//! takes the caller's project from the request's verified key, applies the rate limit, and turns failures the
//! caller can act on into tool errors.

use std::sync::Arc;

use axum::http::request::Parts;
use crucible_api::ProjectSettingsSource;
use crucible_cursor::KeyRing;
use crucible_env::Env;
use crucible_plan::Caller;
use crucible_query::{QueryService, ReadContext};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::McpConfig;
use crate::auth::Authorized;
use crate::cursor::Cursors;
use crate::error::ToolError;
use crate::rate::Limiter;
use crate::tools::{self, Call};

/// What the server tells an agent at `initialize` and `server/discover`.
const INSTRUCTIONS: &str = "Crucible holds the traces of AI agents and MCP servers. Every tool reads the project \
    of the key you connected with, and never writes. Start with get_data_bounds to see where the data lies, list \
    traces in a window with list_traces or find spans by their text with search_spans, then read one trace with \
    get_trace and, for large traces, get_span_children. Results carry previews of inputs and outputs and stay \
    under 64 KiB: when one is cut, pass its `next_cursor` back as `cursor`. get_span_payload reads a whole input, \
    output or other payload field, 32 KiB at a time.";

/// The server, shared by every session and stateless request.
#[derive(Clone, Debug)]
pub(crate) struct McpServer(Arc<Shared>);

#[derive(Debug)]
struct Shared {
    env: Env,
    service: QueryService,
    settings: Arc<dyn ProjectSettingsSource>,
    cursors: Arc<KeyRing>,
    limiter: Option<Limiter>,
}

impl McpServer {
    pub(crate) fn new(
        env: &Env,
        service: QueryService,
        settings: Arc<dyn ProjectSettingsSource>,
        cursors: Arc<KeyRing>,
        config: McpConfig,
    ) -> Self {
        Self(Arc::new(Shared {
            env: env.clone(),
            service,
            settings,
            cursors,
            limiter: Limiter::new(config),
        }))
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("crucible", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = tools::TOOLS.iter().map(tools::Entry::definition).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        tools::find(name).map(tools::Entry::definition)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = tools::find(&request.name) else {
            let message = format!(
                "unknown tool `{}`; tools/list names the tools",
                request.name
            );
            return Err(ErrorData::invalid_params(message, None));
        };
        // The authentication layer runs before rmcp, so every request that reaches a tool carries its principal
        // among the HTTP request's parts.
        let verified = context.extensions.get::<Parts>().and_then(|parts| {
            let Authorized(principal) = *parts.extensions.get::<Authorized>()?;
            Some((parts, principal))
        });
        let Some((parts, principal)) = verified else {
            return Err(ErrorData::internal_error(
                "the request reached a tool without a verified key",
                None,
            ));
        };
        let shared = &self.0;
        let request_id = crucible_api::request_id(&parts.extensions, &shared.env);
        if let Some(limiter) = &shared.limiter
            && let Err(wait) = limiter.check(principal.api_key_id, shared.env.clock.now())
        {
            return Ok(ToolError::rate_limited(wait).into_result(request_id).into());
        }
        let project_id = principal.scope.project_id();
        let settings = shared.settings.settings(project_id);
        let call = Call {
            request_id,
            ctx: ReadContext::new(principal.scope, settings.retention, Caller::Agent),
            settings,
            service: &shared.service,
            cursors: Cursors {
                ring: &shared.cursors,
                project_id,
                now: shared.env.clock.wall_now(),
            },
        };
        let result = match tool
            .call(&call, request.arguments.unwrap_or_default())
            .await
        {
            Ok(output) => CallToolResult::structured(output),
            Err(error) => error.into_result(request_id),
        };
        Ok(result.into())
    }
}
