//! The capture-overhead scenario of the shared SDK suite (`conformance/capture-overhead.json`, contract 9.10): the
//! same tool with capture off and on, in paired alternating rounds over fixed result shapes. The work hook bounds
//! what the capture path serializes and scrubs per field; the added p99 is printed, and gated only on the laptop
//! rig (`CRUCIBLE_CAPTURE_GATE=1`, with `--release`).

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use common::{Collector, GLOBAL, Mode, init};
use crucible_sdk::mcp::test_support::{set_capture_work_hook, set_matcher_hook};
use crucible_sdk::mcp::{self, CaptureRule, Instrumentation};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ContentBlock, ErrorData, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::{
    ClientLifecycleMode, RequestContext, RoleClient, RoleServer, RunningService,
    serve_client_with_lifecycle,
};
use rmcp::{ServerHandler, ServiceExt as _};
use serde::Deserialize;
use serde_json::{Value, json};

const SCENARIO: &str = include_str!("../../../conformance/capture-overhead.json");
const RULES: &str = include_str!("../src/scrub-rules.json");

fn rule_count() -> usize {
    #[derive(Deserialize)]
    struct Rules {
        rules: Vec<Value>,
    }
    serde_json::from_str::<Rules>(RULES).unwrap().rules.len()
}

#[derive(Deserialize)]
struct Scenario {
    schema: String,
    tool: String,
    max_bytes: usize,
    work_bound: usize,
    matcher_steps_per_rule_char: usize,
    ceiling_ms: f64,
    gate_added_p99_ms: f64,
    runs: BTreeMap<String, Run>,
    fills: BTreeMap<String, String>,
    shapes: Vec<Shape>,
}

#[derive(Deserialize)]
struct Run {
    warmup: usize,
    rounds: usize,
    calls: usize,
}

#[derive(Deserialize)]
struct Shape {
    id: String,
    blocks: Vec<Block>,
    #[serde(default)]
    records: usize,
    /// The share of the uncaptured p99 the added p99 may reach, when that is more than the absolute gate.
    #[serde(default)]
    gate_relative_p99: f64,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    fill: String,
    length: usize,
    #[serde(default)]
    mime_type: String,
}

fn repeat_to(unit: &str, length: usize) -> String {
    unit.repeat(length / unit.len() + 1)[..length].to_owned()
}

/// The standard base64 encoding, with padding, of the bytes 0, 1, ..., 255, 0, 1, ...
fn base64_fill(length: usize) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = (0..length / 4 * 3).map(|i| (i % 256) as u8).collect();
    let mut out = String::with_capacity(length);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn shape_result(scenario: &Scenario, shape: &Shape) -> CallToolResult {
    let content = shape
        .blocks
        .iter()
        .map(|b| match b.kind.as_str() {
            "image" => ContentBlock::image(base64_fill(b.length), b.mime_type.clone()),
            _ => ContentBlock::text(repeat_to(&scenario.fills[&b.fill], b.length)),
        })
        .collect();
    let mut result = CallToolResult::success(content);
    if shape.records > 0 {
        let rows: Vec<Value> = (1..=shape.records)
            .map(|id| json!({ "id": id, "sku": format!("SKU-{id:05}"), "qty": id % 7 }))
            .collect();
        result.structured_content = Some(json!({ "records": rows }));
    }
    result
}

#[derive(Clone)]
struct Shapes(Arc<BTreeMap<String, CallToolResult>>);

impl ServerHandler for Shapes {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("capture-overhead", "1.0.0"))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let shape = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("shape"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.0
            .get(shape)
            .cloned()
            .map(Into::into)
            .ok_or_else(|| ErrorData::invalid_params(format!("no shape {shape}"), None))
    }
}

async fn connect(
    instrumentation: &Instrumentation,
    shapes: &Shapes,
) -> RunningService<RoleClient, ClientConfig> {
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let server = instrumentation.wrap(shapes.clone());
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _quit = running.waiting().await;
        }
    });
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("capture-overhead-client", "1.0.0"),
    );
    let lifecycle = ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    };
    serve_client_with_lifecycle(client, client_io, lifecycle)
        .await
        .unwrap()
}

fn quantile(samples: &[f64], percent: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() * percent / 100).min(sorted.len() - 1)]
}

fn p99(samples: &[f64]) -> f64 {
    quantile(samples, 99)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_capture_path_stays_within_the_work_bound() {
    let _turn = GLOBAL.lock().await;
    let scenario: Scenario = serde_json::from_str(SCENARIO).unwrap();
    assert_eq!(scenario.schema, "crucible.conformance.capture-overhead/v1");
    let gated = std::env::var("CRUCIBLE_CAPTURE_GATE").as_deref() == Ok("1");
    let run = &scenario.runs[if gated { "laptop" } else { "ci" }];
    // The collector never answers, so the export queue fills and drops instead of exporting during the rounds.
    let collector = Collector::start(Mode::Stall).await;
    init(&collector, |_| {}).await;
    let shapes = Shapes(Arc::new(
        scenario
            .shapes
            .iter()
            .map(|s| (s.id.clone(), shape_result(&scenario, s)))
            .collect(),
    ));
    let plain = Instrumentation::new(mcp::Options::default()).unwrap();
    let captured = Instrumentation::new(mcp::Options {
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
    let off = connect(&plain, &shapes).await;
    let on = connect(&captured, &shapes).await;
    let mut failures = Vec::new();
    for shape in &scenario.shapes {
        let args = json!({ "shape": shape.id });
        let Value::Object(args) = args else {
            unreachable!()
        };
        let request = CallToolRequestParams::new(scenario.tool.clone()).with_arguments(args);
        let call = async |client: &RunningService<RoleClient, ClientConfig>| {
            let started = Instant::now();
            client.call_tool(request.clone()).await.unwrap();
            started.elapsed().as_secs_f64() * 1000.0
        };
        for _ in 0..run.warmup {
            call(&off).await;
            call(&on).await;
        }
        // The work of one captured call, with the hooks installed; the timed rounds run without them.
        let work = Arc::new(Mutex::new((0usize, 0usize, 0usize)));
        let seen = work.clone();
        set_capture_work_hook(Some(Arc::new(move |serialized, scrubbed| {
            let mut w = seen.lock().unwrap_or_else(PoisonError::into_inner);
            *w = (w.0.max(serialized), w.1.max(scrubbed), w.2 + scrubbed);
        })));
        let steps = Arc::new(AtomicUsize::new(0));
        let counted = steps.clone();
        set_matcher_hook(Some(Arc::new(move |_, n| {
            counted.fetch_add(n, Ordering::Relaxed);
        })));
        call(&on).await;
        set_capture_work_hook(None);
        set_matcher_hook(None);
        let (serialized, scrubbed, scrubbed_in_call) = *work.lock().unwrap();
        let steps = steps.load(Ordering::Relaxed);
        let steps_per_byte = steps as f64 / scrubbed_in_call.max(1) as f64;
        let (mut without, mut with) = (Vec::new(), Vec::new());
        for round in 0..run.rounds {
            let arms = if round % 2 == 0 {
                [&off, &on]
            } else {
                [&on, &off]
            };
            for arm in arms {
                for _ in 0..run.calls {
                    let ms = call(arm).await;
                    if std::ptr::eq(arm, &on) {
                        with.push(ms);
                    } else {
                        without.push(ms);
                    }
                }
            }
        }
        let added = p99(&with) - p99(&without);
        let gate = scenario
            .gate_added_p99_ms
            .max(shape.gate_relative_p99 * p99(&without));
        let excess = with.iter().copied().fold(0.0, f64::max) - quantile(&without, 50);
        println!(
            "capture-overhead rust {}: added p99 {added:.3} ms (off {:.3}, on {:.3}, gate {gate:.3}), slowest excess {excess:.3} ms, serialized {serialized}, scrubbed {scrubbed}, matcher steps {steps} ({steps_per_byte:.1} per character)",
            shape.id,
            p99(&without),
            p99(&with)
        );
        let step_bound = scenario.matcher_steps_per_rule_char * rule_count();
        if steps_per_byte > step_bound as f64 {
            failures.push(format!(
                "{}: matcher steps per byte scrubbed: {steps_per_byte:.1} > {step_bound}",
                shape.id
            ));
        }
        if serialized > scenario.work_bound {
            failures.push(format!(
                "{}: bytes serialized for one field: {serialized} > {}",
                shape.id, scenario.work_bound
            ));
        }
        if scrubbed > scenario.work_bound {
            failures.push(format!(
                "{}: bytes handed to the scrubber for one field: {scrubbed} > {}",
                shape.id, scenario.work_bound
            ));
        }
        if excess > scenario.ceiling_ms {
            failures.push(format!(
                "{}: slowest captured call over the median uncaptured one: {excess:.1} ms",
                shape.id
            ));
        }
        if gated && added > gate {
            failures.push(format!(
                "{}: added p99: {added:.3} ms > {gate:.3} ms",
                shape.id
            ));
        }
    }
    let _closed = off.cancel().await;
    let _closed = on.cancel().await;
    drop(collector);
    let _stopped = crucible_sdk::shutdown().await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
