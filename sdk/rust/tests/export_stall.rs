//! The stalled-collector scenario of the shared SDK suite (`conformance/export-stall.json`, contract 9.10): with
//! capture on and a collector that never answers, the export queue stays under its byte ceiling and drops.

mod common;

use std::time::Duration;

use common::{Collector, GLOBAL, Mode, init};
use crucible_sdk::mcp::{self, CaptureRule, Instrumentation};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ContentBlock, ErrorData, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::{ClientLifecycleMode, RequestContext, RoleServer, serve_client_with_lifecycle};
use rmcp::{ServerHandler, ServiceExt as _};
use serde::Deserialize;

const SCENARIO: &str = include_str!("../../../conformance/export-stall.json");
const OVERHEAD: &str = include_str!("../../../conformance/capture-overhead.json");

#[derive(Deserialize)]
struct Scenario {
    schema: String,
    tool: String,
    max_bytes: usize,
    result_bytes: usize,
    calls: usize,
    queue_spans: usize,
    queue_bytes: u64,
}

#[derive(Deserialize)]
struct Fills {
    fills: std::collections::BTreeMap<String, String>,
}

#[derive(Clone)]
struct Text(String);

impl ServerHandler for Text {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("export-stall", "1.0.0"))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(self.0.clone())]).into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_collector_keeps_the_queue_under_its_byte_ceiling() {
    let _turn = GLOBAL.lock().await;
    let scenario: Scenario = serde_json::from_str(SCENARIO).unwrap();
    assert_eq!(scenario.schema, "crucible.conformance.export-stall/v1");
    assert!(scenario.calls < scenario.queue_spans);
    let prose = &serde_json::from_str::<Fills>(OVERHEAD).unwrap().fills["prose"];
    let text =
        prose.repeat(scenario.result_bytes / prose.len() + 1)[..scenario.result_bytes].to_owned();
    let collector = Collector::start(Mode::Stall).await;
    init(&collector, |o| o.export_timeout = Duration::from_secs(2)).await;
    let instrumentation = Instrumentation::new(mcp::Options {
        capture: vec![CaptureRule {
            tool: scenario.tool.clone(),
            args: true,
            result: true,
            sample_rate: 1.0,
            max_bytes: scenario.max_bytes,
        }],
        ..mcp::Options::default()
    })
    .unwrap();
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let server = instrumentation.wrap(Text(text));
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _quit = running.waiting().await;
        }
    });
    let client = serve_client_with_lifecycle(
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("export-stall-client", "1.0.0"),
        ),
        client_io,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .unwrap();
    let mut most = 0;
    for _ in 0..scenario.calls {
        client
            .call_tool(CallToolRequestParams::new(scenario.tool.clone()))
            .await
            .unwrap();
        most = most.max(crucible_sdk::stats().queued_bytes);
    }
    let dropped = crucible_sdk::stats().dropped;
    let _closed = client.cancel().await;
    drop(collector);
    let _stopped = crucible_sdk::shutdown().await;
    println!(
        "export-stall rust: most queued {most} bytes (ceiling {}), dropped {dropped}",
        scenario.queue_bytes
    );
    assert!(most <= scenario.queue_bytes, "bytes queued: {most}");
    assert!(dropped > 0, "no span was dropped");
}
