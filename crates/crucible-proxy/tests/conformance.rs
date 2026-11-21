//! The proxy plays the shared content-capture scenario (`conformance/scenarios/mcp-secret.json`) through the SDK
//! conformance driver, in its four variants (contract 9.10). A reference server answers behind the proxy's stdio
//! pumps or its HTTP reverse proxy, the proxy captures with the suite's rule and exports to the driver's collector,
//! and the driver judges the spans by the golden, the registry's rules and the records of the real normalizer, as
//! it judges an SDK's. The capture-overhead shapes then show how much of a result capture reads: at most the
//! scenario's work bound, however large the result.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use common::{loopback, prod_env};
use crucible_env::{
    ChaChaRng, Child, ChildControl, ChildError, ChildProcess, ChildSignal, ChildSpec, Env,
    ExitStatus, Instant, JoinHandle,
};
use crucible_fake_collector::suite::{ServerInfo, Transport};
use crucible_fake_collector::{
    Manifest, Options, ResponseEvent, RunOutcome, RunSpec, Runner, Status, Suite, run_suite,
};
use crucible_proxy::encode::ResourceInfo;
use crucible_proxy::http::{HostHeader, HttpProxyConfig, serve};
use crucible_proxy::identity::IdentityResolver;
use crucible_proxy::live::{ExportConfig, Telemetry};
use crucible_proxy::net::Target;
use crucible_proxy::scan::{Scanner, StateKey};
use crucible_proxy::stdio::{ClientIo, run};
use crucible_proxy::{CaptureRule, CoreConfig, ExportResult, Now, ProxyCore, StreamId};
use crucible_records::{CaptureSource, Direction, Timestamp};
use futures::StreamExt as _;
use http::header::CONTENT_TYPE;
use http::{Method, Request, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

/// The suite files the content-capture scenario needs.
fn suite() -> Suite {
    Suite::from_files([
        (
            "suite.json",
            &include_bytes!("../../../conformance/suite.json")[..],
        ),
        (
            "registry.json",
            include_bytes!("../../../conformance/registry.json"),
        ),
        (
            "scrub-rules.json",
            include_bytes!("../../../conformance/scrub-rules.json"),
        ),
        (
            "goldens/common.json",
            include_bytes!("../../../conformance/goldens/common.json"),
        ),
        (
            "scenarios/mcp-secret.json",
            include_bytes!("../../../conformance/scenarios/mcp-secret.json"),
        ),
        (
            "goldens/mcp-secret.json",
            include_bytes!("../../../conformance/goldens/mcp-secret.json"),
        ),
    ])
    .unwrap()
}

/// The proxy's capability manifest: it sends the spans of `proxy_stdio` and `proxy_http`.
fn manifest() -> Manifest {
    let manifest = json!({
        "schema": "crucible.conformance.manifest/v1",
        "sdk": "proxy",
        "language": "rust",
        "sdk_version": env!("CARGO_PKG_VERSION"),
        "runner": ["crucible-proxy-conformance"],
        "kinds": ["mcp"],
        "variants": ["legacy-stdio", "legacy-http", "modern-stdio", "modern-http"],
        "throw_outcome": "internal",
        "progress_count": "observed",
        "source": "proxy",
        "skips": [],
    });
    Manifest::parse("proxy manifest", manifest.to_string().as_bytes()).unwrap()
}

/// What the reference server answers: the `initialize` of a legacy session, and the scenario's tools. A modern
/// request is answered with the server's identity in the result's `_meta`.
fn reference_answer(request: &Json, server: &ServerInfo) -> Option<Json> {
    let id = request.get("id")?.clone();
    let info = json!({"name": server.name, "version": server.version});
    let text = |text: &Json| json!({"content": [{"type": "text", "text": text}]});
    let params = &request["params"];
    let mut result = match request["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": info,
        }),
        Some("tools/call") => match params["name"].as_str() {
            Some("secret") => text(&params["arguments"]["note"]),
            Some("echo") => text(&params["arguments"]["text"]),
            _ => {
                return Some(json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": "unknown tool"}}));
            }
        },
        _ => {
            return Some(json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "method not found"}}));
        }
    };
    if params
        .pointer("/_meta/io.modelcontextprotocol~1protocolVersion")
        .is_some()
    {
        result["_meta"] = json!({"io.modelcontextprotocol/serverInfo": info});
    }
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// The reference server as the child of the stdio pumps: one task behind the `ChildProcess` trait.
#[derive(Debug)]
struct ReferenceChild {
    env: Env,
    server: ServerInfo,
}

#[derive(Debug)]
struct ReferenceControl {
    task: JoinHandle<ExitStatus>,
}

#[async_trait]
impl ChildControl for ReferenceControl {
    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Ok(self.task.join().await.unwrap_or(ExitStatus::Signal(9)))
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.task.abort();
        Ok(())
    }

    fn signal(&mut self, _signal: ChildSignal) -> std::io::Result<()> {
        self.task.abort();
        Ok(())
    }
}

#[async_trait]
impl ChildProcess for ReferenceChild {
    async fn spawn(&self, _spec: ChildSpec) -> Result<Child, ChildError> {
        let (stdin_writer, stdin_reader) = tokio::io::duplex(64 * 1024);
        let (mut stdout_writer, stdout_reader) = tokio::io::duplex(64 * 1024);
        let server = self.server.clone();
        let task = self.env.spawn("reference-server", async move {
            let mut lines = BufReader::new(stdin_reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Json = serde_json::from_str(&line).unwrap_or(Json::Null);
                if let Some(answer) = reference_answer(&request, &server) {
                    let line = format!("{answer}\n");
                    if stdout_writer.write_all(line.as_bytes()).await.is_err() {
                        return ExitStatus::Code(141);
                    }
                }
            }
            ExitStatus::Code(0)
        });
        Ok(Child::new(
            Some(Box::new(stdin_writer)),
            Some(Box::new(stdout_reader)),
            None,
            Box::new(ReferenceControl { task }),
        ))
    }
}

type ServerBody = BoxBody<Bytes, Infallible>;

/// The reference server on Streamable HTTP: a legacy `initialize` opens a session, a notification is accepted
/// with 202 and no body.
async fn reference_http(env: &Env, server: ServerInfo) -> (std::net::SocketAddr, JoinHandle<()>) {
    let mut listener = env.net.listen(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = env.spawn("reference-http", {
        let env = env.clone();
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = server.clone();
                drop(env.spawn("reference-http-connection", async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let server = server.clone();
                        async move {
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            let message: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
                            let response = match reference_answer(&message, &server) {
                                None => Response::builder()
                                    .status(StatusCode::ACCEPTED)
                                    .body(Full::new(Bytes::new()).boxed())
                                    .unwrap(),
                                Some(answer) => {
                                    let mut response = Response::builder()
                                        .status(StatusCode::OK)
                                        .header(CONTENT_TYPE, "application/json");
                                    if message["method"] == "initialize" {
                                        response = response
                                            .header("mcp-session-id", "reference-session-1");
                                    }
                                    response
                                        .body(Full::new(Bytes::from(answer.to_string())).boxed())
                                        .unwrap()
                                }
                            };
                            Ok::<Response<ServerBody>, Infallible>(response)
                        }
                    });
                    let _closed = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                }));
            }
        }
    });
    (addr, accept)
}

/// The proxy as a conformance runner: it plays each run spec's steps as the MCP client, through a proxy in front
/// of the reference server.
#[derive(Debug)]
struct ProxyRunner {
    env: Env,
    /// Hand the run spec's capture rules to the proxy. Without them the proxy records metadata only.
    capture: bool,
}

#[async_trait]
impl Runner for ProxyRunner {
    async fn run(&self, spec: &RunSpec) -> RunOutcome {
        match self.play(spec).await {
            Ok(responses) => RunOutcome {
                responses,
                exit_code: Some(0),
                ..RunOutcome::default()
            },
            Err(error) => RunOutcome {
                error: Some(error),
                ..RunOutcome::default()
            },
        }
    }
}

impl ProxyRunner {
    async fn play(&self, spec: &RunSpec) -> Result<Vec<ResponseEvent>, String> {
        let transport = spec.transport.ok_or("an MCP case has a transport")?;
        let server = spec.server.clone().ok_or("an MCP case names its server")?;
        let source = match transport {
            Transport::Stdio => CaptureSource::ProxyStdio,
            Transport::StreamableHttp => CaptureSource::ProxyHttp,
        };
        // The rules as an operator writes them for `--capture`.
        let rules = spec
            .sdk
            .capture
            .iter()
            .filter(|_| self.capture)
            .map(|rule| {
                CaptureRule::parse(&format!(
                    "{},args={},result={},sample_rate={},max_bytes={}",
                    rule.tool, rule.args, rule.result, rule.sample_rate, rule.max_bytes
                ))
                .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let core = ProxyCore::new(
            CoreConfig {
                state_key: StateKey::new(&spec.keys.secret_key),
                capture: rules,
                ..CoreConfig::new(source)
            },
            self.env.rng.fork("proxy"),
        );
        let telemetry = Telemetry::start(
            &self.env,
            core,
            ExportConfig {
                endpoint: Target::parse(&spec.otlp.endpoint).map_err(|e| e.to_string())?,
                secret_key: spec.keys.secret_key.clone(),
                timeout: Duration::from_secs(5),
                resource: ResourceInfo {
                    service_name: spec.sdk.service_name.clone(),
                    instance_id: "conformance-proxy".to_owned(),
                    deploy_id: spec.sdk.deploy_id.clone(),
                    capture_source: source,
                },
            },
        );
        let responses = match transport {
            Transport::Stdio => self.play_stdio(spec, server, &telemetry).await?,
            Transport::StreamableHttp => self.play_http(spec, server, &telemetry).await?,
        };
        telemetry
            .finish()
            .await
            .ok_or("the proxy did not flush within its deadline")?;
        Ok(responses)
    }

    async fn play_stdio(
        &self,
        spec: &RunSpec,
        server: ServerInfo,
        telemetry: &Telemetry,
    ) -> Result<Vec<ResponseEvent>, String> {
        let (mut to_proxy, proxy_in) = tokio::io::duplex(64 * 1024);
        let (proxy_out, from_proxy) = tokio::io::duplex(64 * 1024);
        let (proxy_err, _stderr) = tokio::io::duplex(1024);
        let children = ReferenceChild {
            env: self.env.clone(),
            server,
        };
        let tap = telemetry.tap();
        let env = self.env.clone();
        let mut pumps = self.env.spawn("proxy-stdio", async move {
            let client = ClientIo {
                input: Box::new(proxy_in),
                output: Box::new(proxy_out),
                error: Box::new(proxy_err),
            };
            run(
                &env,
                &children,
                ChildSpec::default(),
                client,
                futures::stream::pending().boxed(),
                tap,
            )
            .await
        });
        let mut lines = BufReader::new(from_proxy).lines();
        let mut responses = Vec::new();
        for step in &spec.steps {
            let message = step
                .send
                .as_ref()
                .ok_or_else(|| format!("step {}: only send steps", step.step))?;
            to_proxy
                .write_all(format!("{message}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let answer = match message.get("id") {
                None => None,
                Some(id) => loop {
                    let line = lines
                        .next_line()
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or("the proxy closed its output")?;
                    let answer: Json = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                    if answer.get("id") == Some(id) {
                        break Some(answer);
                    }
                },
            };
            responses.push(ResponseEvent {
                step: step.step.clone(),
                http_status: None,
                session_id: None,
                message: answer,
                notifications: 0,
            });
        }
        // The end of the client's input ends the server, and the proxy with it.
        drop(to_proxy);
        let status = pumps.join().await.map_err(|_| "the pumps were cancelled")?;
        match status {
            Ok(ExitStatus::Code(0)) => Ok(responses),
            other => Err(format!("the proxied server ended with {other:?}")),
        }
    }

    async fn play_http(
        &self,
        spec: &RunSpec,
        server: ServerInfo,
        telemetry: &Telemetry,
    ) -> Result<Vec<ResponseEvent>, String> {
        let (upstream, _upstream_task) = reference_http(&self.env, server).await;
        let listener = self
            .env
            .net
            .listen(loopback())
            .await
            .map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let config = HttpProxyConfig {
            upstream: Target::parse(&format!("http://{upstream}")).map_err(|e| e.to_string())?,
            host_header: HostHeader::Preserve,
            connect_timeout: Duration::from_secs(5),
            identity: IdentityResolver::off(&self.env),
        };
        let proxy = self.env.spawn(
            "proxy-http",
            serve(self.env.clone(), listener, config, telemetry.tap()),
        );
        let deadline = self.env.clock.now().saturating_add(Duration::from_secs(5));
        let stream = self
            .env
            .net
            .connect(addr, deadline)
            .await
            .map_err(|e| e.to_string())?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        let connection = self.env.spawn("runner-connection", async move {
            let _closed = connection.await;
        });
        let mut session: Option<String> = None;
        let mut responses = Vec::new();
        for step in &spec.steps {
            let message = step
                .send
                .as_ref()
                .ok_or_else(|| format!("step {}: only send steps", step.step))?;
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/mcp")
                .header(CONTENT_TYPE, "application/json")
                .header("accept", "application/json, text/event-stream");
            for (name, value) in &step.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            if let Some(session) = &session {
                request = request.header("mcp-session-id", session.as_str());
            }
            let request = request
                .body(Full::new(Bytes::from(message.to_string())))
                .map_err(|e| e.to_string())?;
            sender.ready().await.map_err(|e| e.to_string())?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|e| e.to_string())?;
            let (parts, body) = response.into_parts();
            let body = body.collect().await.map_err(|e| e.to_string())?.to_bytes();
            let session_id = parts
                .headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if session_id.is_some() {
                session.clone_from(&session_id);
            }
            responses.push(ResponseEvent {
                step: step.step.clone(),
                http_status: Some(parts.status.as_u16()),
                session_id,
                message: if body.is_empty() {
                    None
                } else {
                    Some(serde_json::from_slice(&body).map_err(|e| e.to_string())?)
                },
                notifications: 0,
            });
        }
        connection.abort();
        proxy.abort();
        Ok(responses)
    }
}

fn options() -> Options {
    Options {
        cases: vec!["mcp-secret".to_owned()],
        settle: Duration::from_millis(50),
        settle_cap: Duration::from_secs(2),
        ..Options::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_passes_the_content_capture_scenario_in_every_variant() {
    let env = prod_env();
    let runner = ProxyRunner {
        env: env.clone(),
        capture: true,
    };
    let report = run_suite(&env, &suite(), &manifest(), &runner, &options())
        .await
        .unwrap();
    assert_eq!(report.results.len(), 4);
    for result in &report.results {
        assert_eq!(
            result.status,
            Status::Pass,
            "{}: {:#?} {:?}",
            result.case,
            result.failures,
            result.runner
        );
        assert!(
            result.checks >= 40,
            "{} ran only {} checks",
            result.case,
            result.checks
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_capture_rules_the_proxy_records_no_content() {
    // The proxy as it runs without `--capture`: the golden's content checks fail, and nothing planted leaks.
    let env = prod_env();
    let runner = ProxyRunner {
        env: env.clone(),
        capture: false,
    };
    let report = run_suite(&env, &suite(), &manifest(), &runner, &options())
        .await
        .unwrap();
    for result in &report.results {
        let checks: Vec<&str> = result.failures.iter().map(|f| f.check.as_str()).collect();
        println!("{} without capture rules fails: {checks:?}", result.case);
        assert_eq!(result.status, Status::Fail, "{}", result.case);
        assert!(
            result.failures.iter().all(|f| f
                .check
                .starts_with("attribute crucible.mcp.content.captured")
                || f.check.starts_with("attribute gen_ai.tool.call.")),
            "{}: {:#?}",
            result.case,
            result.failures
        );
    }
}

fn repeat_to(unit: &str, length: usize) -> String {
    unit.repeat(length / unit.len() + 1)[..length].to_owned()
}

/// A capture-overhead shape's result, filled as `conformance/capture-overhead.json` says.
fn shape_result(scenario: &Json, shape: &Json) -> Json {
    let blocks: Vec<Json> = shape["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            let length = block["length"].as_u64().unwrap() as usize;
            if block["type"] == "image" {
                let bytes: Vec<u8> = (0..length / 4 * 3).map(|i| (i % 256) as u8).collect();
                json!({"type": "image", "data": STANDARD.encode(bytes), "mimeType": block["mime_type"]})
            } else {
                let fill = scenario["fills"][block["fill"].as_str().unwrap()].as_str().unwrap();
                json!({"type": "text", "text": repeat_to(fill, length)})
            }
        })
        .collect();
    let mut result = json!({"content": blocks});
    if let Some(records) = shape["records"].as_u64() {
        let rows: Vec<Json> = (1..=records)
            .map(|i| json!({"id": i, "sku": format!("SKU-{i:05}"), "qty": i % 7}))
            .collect();
        result["structuredContent"] = json!({"records": rows});
    }
    result
}

#[test]
fn capture_reads_at_most_the_work_bound_of_every_overhead_shape() {
    let scenario: Json =
        serde_json::from_slice(include_bytes!("../../../conformance/capture-overhead.json"))
            .unwrap();
    let max_bytes = scenario["max_bytes"].as_u64().unwrap() as usize;
    let work_bound = scenario["work_bound"].as_u64().unwrap() as usize;
    let rule = CaptureRule::parse(&format!(
        "{},max_bytes={max_bytes}",
        scenario["tool"].as_str().unwrap()
    ))
    .unwrap();
    // What capture reads of a field is the window: the cap and the scrubbing lookahead.
    assert!(rule.window() <= work_bound);
    for shape in scenario["shapes"].as_array().unwrap() {
        let id = shape["id"].as_str().unwrap();
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": shape_result(&scenario, shape)})
            .to_string();
        // The scanner records each result member up to the window, however long the member.
        let mut scanner = Scanner::lines();
        scanner.record_results(rule.window());
        for chunk in response.as_bytes().chunks(64 * 1024) {
            scanner.push(chunk);
        }
        scanner.push(b"\n");
        let messages = scanner.take();
        let members = &messages[0].result_members;
        for member in [&members.content, &members.structured_content]
            .into_iter()
            .flatten()
        {
            assert!(
                member.text.len() <= rule.window(),
                "{id}: recorded {} bytes",
                member.text.len()
            );
        }

        // Through the core, the field that leaves the proxy is cut at the cap.
        let mut core = ProxyCore::new(
            CoreConfig {
                capture: vec![rule.clone()],
                ..CoreConfig::new(CaptureSource::ProxyStdio)
            },
            Arc::new(ChaChaRng::from_u64(1)),
        );
        let tap = core.tap();
        let now = Now {
            mono: Instant::from_nanos(1),
            wall: Timestamp::from_micros(1_800_000_000_000_000),
        };
        let call = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "shape", "arguments": {"shape": id}}})
        );
        tap.forward(
            StreamId::STDIO,
            Direction::ClientToServer,
            Bytes::from(call),
            now,
        );
        for chunk in format!("{response}\n").as_bytes().chunks(64 * 1024) {
            tap.forward(
                StreamId::STDIO,
                Direction::ServerToClient,
                Bytes::copy_from_slice(chunk),
                now,
            );
            core.drain();
        }
        let far = Instant::from_nanos(u64::MAX / 2);
        let batch = core.next_export(far, true).unwrap();
        let content = batch.ops[0].content.clone().unwrap();
        core.export_done(batch, ExportResult::Delivered, far);
        let result = content.result.unwrap();
        assert!(result.len() <= max_bytes, "{id}: {} bytes", result.len());
        assert_eq!(
            content.args.as_deref(),
            Some(format!(r#"{{"shape":"{id}"}}"#).as_str())
        );
        let whole = shape_result(&scenario, shape).to_string().len() <= max_bytes;
        assert_eq!(content.truncated, !whole, "{id}");
    }
}
