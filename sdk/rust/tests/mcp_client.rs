//! The client wrapper against an instrumented server over rmcp's in-memory transport: an agent's tool call and
//! the server's handling of it land in one trace.

mod common;

use common::{Collector, GLOBAL, Mode, init};
use crucible_sdk::mcp::{self, Instrumentation};
use crucible_sdk::mcp_client::{TracedClient, Transport};
use crucible_sdk::{Context, ObservationType, observe};
use opentelemetry::context::FutureExt as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt as _};
use serde_json::json;

struct Weather;

impl ServerHandler for Weather {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("weather", "1.0.0"))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "forecast" => Ok(CallToolResult::success(vec![ContentBlock::text("sunny")]).into()),
            _ => Err(ErrorData::invalid_params("unknown tool", None)),
        }
    }
}

#[tokio::test]
async fn a_client_call_and_its_server_span_share_one_trace() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let server = Instrumentation::new(mcp::Options::default())
        .unwrap()
        .wrap(Weather);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _quit = running.waiting().await;
        }
    });
    let config = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("planner", "3.0.0"),
    );
    let mut running = config.serve(client_io).await.unwrap();
    let client = TracedClient::new(running.peer().clone()).with_transport(Transport::Stdio);

    let step = observe(&Context::new(), "plan a picnic", ObservationType::Agent);
    let response = client
        .call_tool(CallToolRequestParams::new("forecast"))
        .with_context(step.context())
        .await
        .unwrap();
    assert!(matches!(response, CallToolResponse::Complete(_)));
    let unknown = client
        .call_tool(CallToolRequestParams::new("tides"))
        .with_context(step.context())
        .await;
    assert!(unknown.is_err());
    step.end();
    running.close().await.unwrap();
    crucible_sdk::flush().await.unwrap();

    let spans = collector.spans();
    let find = |name: &str, kind: i32| {
        spans
            .iter()
            .find(|s| s.name == name && s.kind == kind)
            .unwrap_or_else(|| panic!("no {name} span of kind {kind}: {spans:#?}"))
    };
    let agent = find("plan a picnic", 1);
    let client_span = find("tools/call forecast", 3);
    let server_span = find("tools/call forecast", 2);
    assert_eq!(client_span.trace_id, agent.trace_id);
    assert_eq!(client_span.parent_span_id, agent.span_id);
    assert_eq!(server_span.trace_id, agent.trace_id);
    assert_eq!(server_span.parent_span_id, client_span.span_id);
    for (key, value) in [
        ("mcp.method.name", json!("tools/call")),
        ("gen_ai.tool.name", json!("forecast")),
        ("crucible.mcp.capture_source", json!("sdk")),
        ("crucible.mcp.transport", json!("stdio")),
        ("crucible.mcp.era", json!("legacy")),
        ("crucible.mcp.server.name", json!("weather")),
        ("crucible.mcp.server.version", json!("1.0.0")),
        ("crucible.mcp.result_type", json!("complete")),
    ] {
        assert_eq!(client_span.attr(key), value, "{key}");
    }
    assert_eq!(
        client_span.attr("jsonrpc.request.id"),
        server_span.attr("jsonrpc.request.id")
    );
    assert_eq!(
        server_span.attr("crucible.mcp.client.name"),
        json!("planner")
    );

    let failed = find("tools/call tides", 3);
    assert_eq!(failed.attr("error.type"), json!("-32602"));
    assert_eq!(failed.attr("crucible.mcp.error.class"), json!("validation"));
    assert_eq!(failed.status_code, 2);
    crucible_sdk::shutdown().await.unwrap();
}
