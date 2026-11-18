//! The reference server of the runner contract on rmcp, instrumented through the SDK's public API.

use std::sync::Arc;
use std::time::Duration;

use crucible_sdk::mcp::{self, CaptureRule, Instrumentation, Subclass};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequest,
    ElicitRequestParams, ErrorData, Implementation, InputRequest, InputRequests,
    InputRequiredResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt as _};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::RunSpec;

/// The state the `needs_input` tool hands back, fixed by the runner contract.
const REQUEST_STATE: &str = "crucible-conformance-request-state-1";

/// The instrumentation built from the run spec's `sdk` options.
pub(crate) fn instrumentation(spec: &RunSpec) -> Result<Instrumentation, String> {
    let server = spec
        .server
        .clone()
        .ok_or("an MCP run spec names its server")?;
    Instrumentation::new(mcp::Options {
        server_name: server.name,
        server_version: server.version,
        capture: spec
            .sdk
            .capture
            .iter()
            .map(|r| CaptureRule {
                tool: r.tool.clone(),
                args: r.args,
                result: r.result,
                sample_rate: r.sample_rate,
                max_bytes: r.max_bytes,
            })
            .collect(),
        handle_params: spec.sdk.handle_params.clone(),
        ..mcp::Options::default()
    })
    .map_err(|e| e.to_string())
}

/// The reference server: dual era, no identify hook, and the seven tools of the runner contract.
#[derive(Clone, Debug)]
pub(crate) struct Reference {
    info: Implementation,
}

impl Reference {
    pub(crate) fn new(spec: &RunSpec) -> Self {
        let server = spec.server.clone().map_or_else(
            || ("crucible-conformance-server".to_owned(), "0.0.0".to_owned()),
            |s| (s.name, s.version),
        );
        Self {
            info: Implementation::new(server.0, server.1),
        }
    }
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn arg<'a>(request: &'a CallToolRequestParams, name: &str) -> &'a str {
    request
        .arguments
        .as_ref()
        .and_then(|a| a.get(name))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn number(request: &CallToolRequestParams, name: &str) -> u64 {
    request
        .arguments
        .as_ref()
        .and_then(|a| a.get(name))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn tool(name: &'static str, description: &'static str, properties: Value) -> Tool {
    let schema = json!({ "type": "object", "properties": properties });
    let schema: JsonObject = serde_json::from_value(schema).unwrap_or_default();
    Tool::new(name, description, Arc::new(schema))
}

impl ServerHandler for Reference {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = self.info.clone();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let string = json!({ "type": "string" });
        let integer = json!({ "type": "integer" });
        Ok(ListToolsResult::with_all_items(vec![
            tool("echo", "Returns its text.", json!({ "text": string })),
            tool(
                "fail",
                "Fails with the reason, annotated as an upstream error.",
                json!({ "reason": string }),
            ),
            tool(
                "throw",
                "Returns a JSON-RPC internal error, a handler's error path.",
                json!({ "message": string }),
            ),
            tool(
                "slow",
                "Waits, sending progress when asked.",
                json!({ "ms": integer, "steps": integer }),
            ),
            tool(
                "secret",
                "Returns its note unchanged.",
                json!({ "note": string }),
            ),
            tool(
                "needs_input",
                "Asks for a confirmation before answering.",
                json!({ "item": string }),
            ),
            tool(
                "lookup_order",
                "Looks an order up.",
                json!({ "order_id": string }),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = match request.name.as_ref() {
            "echo" => text(arg(&request, "text")),
            "fail" => {
                mcp::annotate_error(&context, Subclass::Upstream);
                CallToolResult::error(vec![ContentBlock::text(arg(&request, "reason"))])
            }
            // A handler's error: rmcp answers it with a JSON-RPC error.
            "throw" => {
                return Err(ErrorData::internal_error(
                    arg(&request, "message").to_owned(),
                    None,
                ));
            }
            "slow" => slow(&request, &context).await?,
            "secret" => text(arg(&request, "note")),
            "needs_input" => return needs_input(&request),
            "lookup_order" => text(format!("order {}: shipped", arg(&request, "order_id"))),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool {other}"),
                    None,
                ));
            }
        };
        Ok(CallToolResponse::Complete(result))
    }
}

/// Waits `ms` milliseconds in all, stopping early when cancelled. When the request carries a progress token it
/// sends `steps` progress notifications spread over the wait.
async fn slow(
    request: &CallToolRequestParams,
    context: &RequestContext<RoleServer>,
) -> Result<CallToolResult, ErrorData> {
    let total = Duration::from_millis(number(request, "ms"));
    let token = context.meta.get_progress_token();
    let steps = if token.is_some() {
        number(request, "steps")
    } else {
        0
    };
    let start = Instant::now();
    let cancelled = || ErrorData::internal_error("cancelled", None);
    for i in 1..=steps {
        let at = start + total.mul_f64(i as f64 / (steps + 1) as f64);
        tokio::select! {
            () = tokio::time::sleep_until(at) => {}
            () = context.ct.cancelled() => return Err(cancelled()),
        }
        if let Some(token) = &token {
            let params =
                ProgressNotificationParam::new(token.clone(), i as f64).with_total(steps as f64);
            mcp::notify_progress(context, params)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        }
    }
    tokio::select! {
        () = tokio::time::sleep_until(start + total) => Ok(text("done")),
        () = context.ct.cancelled() => Err(cancelled()),
    }
}

/// Answers `input_required` with a form elicitation until it is retried with the state and an accepted
/// confirmation.
fn needs_input(request: &CallToolRequestParams) -> Result<CallToolResponse, ErrorData> {
    let item = arg(request, "item");
    if request.request_state.is_none() {
        let schema = json!({
            "type": "object",
            "properties": { "confirmed": { "type": "boolean" } },
            "required": ["confirmed"],
        });
        let schema = serde_json::from_value(schema)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let mut requests = InputRequests::new();
        requests.insert(
            "confirm".to_owned(),
            InputRequest::Elicitation(ElicitRequest::new(
                ElicitRequestParams::FormElicitationParams {
                    meta: None,
                    message: format!("Confirm {item}?"),
                    requested_schema: schema,
                },
            )),
        );
        return Ok(CallToolResponse::InputRequired(InputRequiredResult::new(
            Some(requests),
            Some(REQUEST_STATE.to_owned()),
        )));
    }
    let accepted = request
        .input_responses
        .as_ref()
        .and_then(|r| r.get("confirm"))
        .is_some_and(|r| r.get("action").and_then(Value::as_str) == Some("accept"));
    if !accepted {
        return Err(ErrorData::invalid_params(
            "the confirmation was not accepted",
            None,
        ));
    }
    Ok(CallToolResponse::Complete(text(format!(
        "confirmed {item}"
    ))))
}

/// The stdio reference server: serves on stdin and stdout until stdin closes, then flushes.
pub(crate) async fn serve_stdio(spec: &str) -> Result<(), String> {
    let spec: RunSpec = serde_json::from_str(spec).map_err(|e| format!("the run spec: {e}"))?;
    crate::init_sdk(&spec)?;
    let service = instrumentation(&spec)?.wrap(Reference::new(&spec));
    match service.serve(rmcp::transport::stdio()).await {
        Ok(running) => {
            let _quit = running.waiting().await;
        }
        Err(error) => eprintln!("reference server stopped: {error}"),
    }
    crucible_sdk::shutdown().await.map_err(|e| e.to_string())
}
