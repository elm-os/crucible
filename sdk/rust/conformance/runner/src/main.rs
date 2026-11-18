//! The Rust SDK's runner for the shared SDK conformance suite (docs/testing/sdk-conformance.md, runner contract
//! version 1). `crucible-sdk-driver` starts it once per case and writes the run spec to its stdin. For an agent
//! case the runner is the agent program; for an MCP case it starts the reference server, as a child process for
//! stdio and in process for streamable HTTP, and plays the steps against it. `serve-stdio <spec>` is the stdio
//! reference server.

mod agent;
mod mcp;
mod server;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct RunSpec {
    case: String,
    kind: String,
    transport: Option<String>,
    otlp: Otlp,
    checkpoint_url: String,
    keys: Keys,
    sdk: SdkOptions,
    server: Option<ServerInfo>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Otlp {
    endpoint: String,
}

#[derive(Debug, Deserialize)]
struct Keys {
    secret_key: String,
}

#[derive(Debug, Deserialize)]
struct SdkOptions {
    service_name: String,
    #[serde(default)]
    deploy_id: String,
    #[serde(default)]
    environment: String,
    #[serde(default)]
    release: String,
    flush_interval_ms: u64,
    #[serde(default)]
    capture: Vec<CaptureSpec>,
    #[serde(default)]
    handle_params: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct CaptureSpec {
    tool: String,
    args: bool,
    result: bool,
    sample_rate: f64,
    max_bytes: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct ServerInfo {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct Step {
    step: String,
    send: Option<Value>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    wait: Option<bool>,
    #[serde(rename = "await")]
    await_step: Option<String>,
    sleep_ms: Option<u64>,
    close: Option<String>,
    observe: Option<agent::Node>,
    flush: Option<Value>,
    checkpoint: Option<String>,
    shutdown: Option<Value>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("runner: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        match args.get(1).map(String::as_str) {
            Some("serve-stdio") => server::serve_stdio(args.get(2).map_or("", String::as_str)).await,
            _ => run().await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runner: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("reading the run spec: {e}"))?;
    let raw: Value = serde_json::from_str(&input).map_err(|e| format!("the run spec: {e}"))?;
    let spec: RunSpec =
        serde_json::from_value(raw.clone()).map_err(|e| format!("the run spec: {e}"))?;
    match spec.kind.as_str() {
        "agent" => agent::run(&spec).await,
        "mcp" => mcp::run(&spec, raw).await,
        other => Err(format!("case {}: unknown kind {other:?}", spec.case)),
    }
}

/// Starts the SDK with the run spec's options.
fn init_sdk(spec: &RunSpec) -> Result<(), String> {
    crucible_sdk::init(crucible_sdk::Options {
        endpoint: spec.otlp.endpoint.clone(),
        secret_key: spec.keys.secret_key.clone(),
        service_name: spec.sdk.service_name.clone(),
        environment: spec.sdk.environment.clone(),
        release: spec.sdk.release.clone(),
        deploy_id: spec.sdk.deploy_id.clone(),
        flush_interval: Duration::from_millis(spec.sdk.flush_interval_ms),
        ..crucible_sdk::Options::default()
    })
    .map_err(|e| e.to_string())
}
