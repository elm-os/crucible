//! Trace context propagation for MCP clients built on rmcp.
//!
//! [`TracedClient`] wraps a client's `Peer`. Each request it sends becomes a CLIENT span, a child of the span in
//! the current OpenTelemetry context, and carries that span's trace context to the server in `params._meta`
//! (`traceparent`, `tracestate`; baggage is never sent). A server instrumented with Crucible then records its
//! span in the same trace, so an agent's tool call and the server's handling of it link up:
//!
//! ```no_run
//! # async fn example(peer: rmcp::Peer<rmcp::RoleClient>) -> Result<(), rmcp::ServiceError> {
//! use crucible_sdk::{Context, ObservationType, observe};
//! use opentelemetry::context::FutureExt as _;
//!
//! let client = crucible_sdk::mcp_client::TracedClient::new(peer);
//! let step = observe(&Context::current(), "look up the order", ObservationType::Tool);
//! let params = rmcp::model::CallToolRequestParams::new("lookup_order");
//! let response = client.call_tool(params).with_context(step.context()).await?;
//! step.end();
//! # let _ = response;
//! # Ok(())
//! # }
//! ```

use opentelemetry::trace::{SpanBuilder, SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};
use rmcp::Peer;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, GetMeta as _,
    ServerResult,
};
use rmcp::service::{PeerRequestOptions, RoleClient, ServiceError};

use crate::keys::mcp as k;
pub use crate::mcp_common::Transport;
use crate::mcp_common::{self, Annotations, Outcome};
use crate::scrub;

/// The cap on an error message (`error_message`, contract 3.5).
const MAX_ERROR_MESSAGE: usize = 2 * 1024;

/// An rmcp client peer whose requests are recorded as CLIENT spans and carry their trace context.
#[derive(Clone, Debug)]
pub struct TracedClient {
    peer: Peer<RoleClient>,
    transport: Transport,
}

impl TracedClient {
    /// Wraps the peer of a running client (`RunningService::peer`).
    pub fn new(peer: Peer<RoleClient>) -> Self {
        Self {
            peer,
            transport: Transport::Unknown,
        }
    }

    /// Sets the transport reported on the spans (`crucible.mcp.transport`); unknown by default.
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// The wrapped peer, for anything not routed through this wrapper.
    pub fn peer(&self) -> &Peer<RoleClient> {
        &self.peer
    }

    /// Sends one request in a CLIENT span that is a child of the current context's span. Before
    /// `crucible_sdk::init` the request is sent as it is.
    pub async fn send_request(
        &self,
        mut request: ClientRequest,
    ) -> Result<ServerResult, ServiceError> {
        let Some(client) = crate::client::active() else {
            return self.peer.send_request(request).await;
        };
        let method = request.method().to_owned();
        let target = match &request {
            ClientRequest::CallToolRequest(r) => Some((k::TOOL_NAME, r.params.name.to_string())),
            ClientRequest::GetPromptRequest(r) => Some((k::PROMPT_NAME, r.params.name.clone())),
            _ => None,
        };
        let mut attributes = vec![
            KeyValue::new(k::METHOD, method.clone()),
            KeyValue::new(k::CAPTURE_SOURCE, "sdk"),
            KeyValue::new(k::TRANSPORT, self.transport.as_str()),
        ];
        if let Some(info) = self.peer.peer_info() {
            attributes.extend(mcp_common::era(info.protocol_version.as_str()));
            if let Some(server) = &info.server_info {
                attributes.push(KeyValue::new(k::SERVER_NAME, server.name.clone()));
                attributes.push(KeyValue::new(k::SERVER_VERSION, server.version.clone()));
            }
        }
        let name = match &target {
            Some((_, target)) => format!("{method} {target}"),
            None => method,
        };
        if let Some(target) = target {
            attributes.push(KeyValue::new(target.0, target.1));
        }
        let parent = Context::current();
        let span = client.tracer.build_with_context(
            SpanBuilder::from_name(name)
                .with_kind(SpanKind::Client)
                .with_attributes(attributes),
            &parent,
        );
        let cx = parent.with_span(span);
        mcp_common::inject(&cx, &mut request.get_meta_mut().0.0);
        let mut outcome = Outcome::default();
        let result = match self
            .peer
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => {
                cx.span()
                    .set_attribute(KeyValue::new(k::REQUEST_ID, handle.id.to_string()));
                handle.await_response().await
            }
            Err(error) => Err(error),
        };
        match &result {
            Ok(ServerResult::CallToolResult(r)) => outcome.tool_result(r),
            Ok(ServerResult::InputRequiredResult(r)) => {
                outcome.result_type = Some("input_required");
                outcome.state_out = r.request_state.clone();
            }
            Ok(ServerResult::CreateTaskResult(task)) => {
                outcome.result_type = Some("task");
                outcome.task_id = Some(task.task.task_id.clone());
            }
            Ok(_) => outcome.result_type = Some("complete"),
            Err(ServiceError::McpError(error)) => {
                outcome.code = Some(error.code.0);
                outcome.message = error.message.to_string();
            }
            Err(ServiceError::Cancelled { .. }) => outcome.cancel = Some("notification"),
            Err(error) => {
                outcome.transport = true;
                outcome.message = error.to_string();
            }
        }
        let (attributes, class) = outcome.attributes(Annotations::default(), &client.secret_key);
        let span = cx.span();
        span.set_attributes(attributes);
        if class != "none" {
            let message = scrub::capped(&outcome.message, MAX_ERROR_MESSAGE).0;
            span.set_status(Status::error(message));
        }
        span.end();
        result
    }

    /// Sends one `tools/call` request (see [`TracedClient::send_request`]). It drives no multi-round
    /// follow-up: an `input_required` answer is returned for the caller to retry.
    pub async fn call_tool(
        &self,
        params: CallToolRequestParams,
    ) -> Result<CallToolResponse, ServiceError> {
        match self
            .send_request(ClientRequest::CallToolRequest(CallToolRequest::new(params)))
            .await?
        {
            ServerResult::CallToolResult(result) => Ok(CallToolResponse::Complete(result)),
            ServerResult::InputRequiredResult(result) => {
                Ok(CallToolResponse::InputRequired(result))
            }
            ServerResult::CreateTaskResult(result) => Ok(CallToolResponse::Task(result)),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }
}
