//! A slow or down collector adds no latency to the caller (`conformance/export-latency.json`, contract 9.10): the
//! same calls run against a collector that answers, one that never answers and a port where nothing listens. A
//! call that waited for an export would take the export timeout; every call stays far below it, and `flush` and
//! `shutdown` keep to the export timeout. The added p99 over the healthy run is printed, and gated only on the
//! laptop rig (`CRUCIBLE_CAPTURE_GATE=1`).

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::{Collector, GLOBAL, Mode, init};
use crucible_sdk::mcp::{self, CaptureRule, Instrumentation};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ContentBlock, ErrorData, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::{ClientLifecycleMode, RequestContext, RoleServer, serve_client_with_lifecycle};
use rmcp::{ServerHandler, ServiceExt as _};
use serde::Deserialize;

const SCENARIO: &str = include_str!("../../../conformance/export-latency.json");
const OVERHEAD: &str = include_str!("../../../conformance/capture-overhead.json");

#[derive(Deserialize)]
struct Scenario {
    schema: String,
    tool: String,
    max_bytes: usize,
    result_bytes: usize,
    collectors: Vec<String>,
    warmup: usize,
    calls: usize,
    export_timeout_ms: u64,
    call_ceiling_ms: f64,
    slack_ms: u64,
    gate_added_p99_ms: f64,
}

#[derive(Deserialize)]
struct Fills {
    fills: BTreeMap<String, String>,
}

#[derive(Clone)]
struct Text(String);

impl ServerHandler for Text {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("export-latency", "1.0.0"))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(self.0.clone())]).into())
    }
}

/// In milliseconds.
fn quantile(samples: &[f64], q: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
}

/// A port that was free a moment ago: nothing listens on it.
async fn closed_port() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_or_refusing_collector_adds_no_latency_to_calls() {
    let _turn = GLOBAL.lock().await;
    let scenario: Scenario = serde_json::from_str(SCENARIO).unwrap();
    assert_eq!(scenario.schema, "crucible.conformance.export-latency/v1");
    assert!(
        scenario.calls > 512,
        "the calls must fill a batch, so that an export runs while they do"
    );
    let prose = &serde_json::from_str::<Fills>(OVERHEAD).unwrap().fills["prose"];
    let text =
        prose.repeat(scenario.result_bytes / prose.len() + 1)[..scenario.result_bytes].to_owned();
    let deadline = Duration::from_millis(scenario.export_timeout_ms + scenario.slack_ms);
    let mut p99 = BTreeMap::new();
    for state in &scenario.collectors {
        let collector = Collector::start(if state == "stalled" {
            Mode::Stall
        } else {
            Mode::Accept
        })
        .await;
        let refused = closed_port().await;
        init(&collector, |o| {
            o.export_timeout = Duration::from_millis(scenario.export_timeout_ms);
            if state == "refused" {
                o.endpoint = refused.clone();
            }
        })
        .await;
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
        let server = instrumentation.wrap(Text(text.clone()));
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_io).await {
                let _quit = running.waiting().await;
            }
        });
        let client = serve_client_with_lifecycle(
            ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("export-latency-client", "1.0.0"),
            ),
            client_io,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
        let call = || client.call_tool(CallToolRequestParams::new(scenario.tool.clone()));
        for _ in 0..scenario.warmup {
            call().await.unwrap();
        }
        let mut samples = Vec::with_capacity(scenario.calls);
        for _ in 0..scenario.calls {
            let start = Instant::now();
            call().await.unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let _closed = client.cancel().await;
        let start = Instant::now();
        let _flushed = crucible_sdk::flush().await;
        let flushed = start.elapsed();
        let start = Instant::now();
        let _stopped = crucible_sdk::shutdown().await;
        let stopped = start.elapsed();
        drop(collector);
        let slowest = samples.iter().copied().fold(0.0, f64::max);
        p99.insert(state.clone(), quantile(&samples, 0.99));
        println!(
            "export-latency rust {state}: p50 {:.3} ms, p99 {:.3} ms, slowest {slowest:.3} ms, flush {} ms, shutdown {} ms",
            quantile(&samples, 0.5),
            p99[state],
            flushed.as_millis(),
            stopped.as_millis()
        );
        if state != "healthy" {
            assert!(
                slowest <= scenario.call_ceiling_ms,
                "{state}: a call took {slowest:.1} ms"
            );
        }
        assert!(flushed <= deadline, "{state}: flush took {flushed:?}");
        assert!(stopped <= deadline, "{state}: shutdown took {stopped:?}");
    }
    for state in ["stalled", "refused"] {
        let added = p99[state] - p99["healthy"];
        println!("export-latency rust {state}: added p99 {added:.3} ms over the healthy collector");
        if std::env::var("CRUCIBLE_CAPTURE_GATE").as_deref() == Ok("1") {
            assert!(
                added <= scenario.gate_added_p99_ms,
                "{state}: added p99 {added:.3} ms"
            );
        }
    }
}
