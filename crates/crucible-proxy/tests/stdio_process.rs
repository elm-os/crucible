//! The stdio proxy against real processes (invariant I11, process tests): a scripted server as a real child,
//! the production `Env`, and a collector that is healthy, refusing connections, never answering, or absent.
//!
//! The same bytes are exchanged with the server directly and through the proxy, and must be identical in both
//! directions. Exit codes, a crash in the middle of an answer, signals, the end of the client's input and lines
//! over 1 MiB all pass through. The tests run on Linux and Windows; the helper is `crucible-proxy-rig server`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use crucible_env::{ChildProcess, ChildSignal, ChildSpec, Env, ExitStatus, JoinHandle, StderrMode};
use crucible_env_prod::{ProdConfig, TokioChildProcess};
use crucible_fake_collector::registry::Registry;
use crucible_fake_collector::value::{Value, get};
use crucible_fake_collector::{Collector, Event, RecordedSpan};
use crucible_proxy::encode::ResourceInfo;
use crucible_proxy::live::{ExportConfig, Telemetry};
use crucible_proxy::net::Target;
use crucible_proxy::stdio::{ClientIo, StdioError, run};
use crucible_proxy::{CoreConfig, ProxyCore};
use crucible_records::CaptureSource;
use futures::StreamExt as _;
use futures::channel::mpsc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

const RIG: &str = env!("CARGO_BIN_EXE_crucible-proxy-rig");
const SECRET: &str = "csk_test0000000000000000000000000000000000000000";

fn prod_env() -> Env {
    crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap()
        .env
}

fn server(flags: &[&str]) -> ChildSpec {
    ChildSpec {
        program: RIG.to_owned(),
        args: std::iter::once("server")
            .chain(flags.iter().copied())
            .map(str::to_owned)
            .collect(),
        ..ChildSpec::default()
    }
}

/// The state of the collector for a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sink {
    Healthy,
    /// Nothing listens on the port.
    Refused,
    /// The port accepts connections and never answers.
    Stalled,
    /// No collector is configured.
    Absent,
}

struct Proxy {
    to_proxy: Option<DuplexStream>,
    from_proxy: DuplexStream,
    stderr: DuplexStream,
    signals: mpsc::UnboundedSender<ChildSignal>,
    run: JoinHandle<Result<ExitStatus, StdioError>>,
    telemetry: Telemetry,
    collector: Option<Collector>,
    /// Keeps the stalled listener's connections open.
    _stalled: Option<JoinHandle<()>>,
}

async fn start(env: &Env, spec: ChildSpec, sink: Sink) -> Proxy {
    let mut collector = None;
    let mut stalled = None;
    let endpoint = match sink {
        Sink::Healthy => {
            let started = Collector::start(env).await.unwrap();
            let endpoint = started.endpoint();
            collector = Some(started);
            Some(endpoint)
        }
        Sink::Refused | Sink::Stalled => {
            let mut listener = env
                .net
                .listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            if sink == Sink::Stalled {
                stalled = Some(env.spawn("stalled-collector", async move {
                    let mut open = Vec::new();
                    while let Ok((stream, _)) = listener.accept().await {
                        open.push(stream);
                    }
                }));
            }
            // For `Refused` the listener is dropped here, so the port is closed.
            Some(endpoint)
        }
        Sink::Absent => None,
    };
    let telemetry = match endpoint {
        Some(endpoint) => Telemetry::start(
            env,
            ProxyCore::new(
                CoreConfig::new(CaptureSource::ProxyStdio),
                env.rng.fork("proxy"),
            ),
            ExportConfig {
                endpoint: Target::parse(&endpoint).unwrap(),
                secret_key: SECRET.to_owned(),
                timeout: Duration::from_secs(5),
                resource: ResourceInfo {
                    service_name: "scripted".to_owned(),
                    instance_id: "instance-1".to_owned(),
                    deploy_id: None,
                    capture_source: CaptureSource::ProxyStdio,
                },
            },
        ),
        None => Telemetry::disabled(env),
    };
    let (to_proxy, proxy_in) = tokio::io::duplex(64 * 1024);
    let (proxy_out, from_proxy) = tokio::io::duplex(64 * 1024);
    let (proxy_err, stderr) = tokio::io::duplex(64 * 1024);
    let (signals, signal_stream) = mpsc::unbounded();
    let tap = telemetry.tap();
    let run = env.spawn("proxy-under-test", {
        let env = env.clone();
        async move {
            let children = TokioChildProcess::new();
            let client = ClientIo {
                input: Box::new(proxy_in),
                output: Box::new(proxy_out),
                error: Box::new(proxy_err),
            };
            run(&env, &children, spec, client, signal_stream.boxed(), tap).await
        }
    });
    Proxy {
        to_proxy: Some(to_proxy),
        from_proxy,
        stderr,
        signals,
        run,
        telemetry,
        collector,
        _stalled: stalled,
    }
}

impl Proxy {
    async fn send(&mut self, bytes: &[u8]) {
        self.to_proxy
            .as_mut()
            .unwrap()
            .write_all(bytes)
            .await
            .unwrap();
    }

    /// Reads up to and including the next newline.
    async fn line(&mut self) -> Vec<u8> {
        let mut line = Vec::new();
        loop {
            let byte = self.from_proxy.read_u8().await.unwrap();
            line.push(byte);
            if byte == b'\n' {
                return line;
            }
        }
    }

    /// Closes the client's end and returns everything the proxy still wrote, with how the child ended.
    async fn finish(mut self) -> (Vec<u8>, ExitStatus, Vec<RecordedSpan>) {
        drop(self.to_proxy.take());
        let mut rest = Vec::new();
        self.from_proxy.read_to_end(&mut rest).await.unwrap();
        let status = self.run.join().await.unwrap().unwrap();
        self.telemetry.finish().await;
        let spans = self.collector.map_or_else(Vec::new, |collector| {
            collector
                .events()
                .into_iter()
                .flat_map(|event| match event {
                    Event::Export(export) => {
                        assert_eq!(export.status, 200, "{:?}", export.refusal);
                        assert_eq!(export.path, "/v1/traces");
                        assert_eq!(
                            export.content_type.as_deref(),
                            Some("application/x-protobuf")
                        );
                        assert_eq!(
                            export.authorization.as_deref(),
                            Some(format!("Bearer {SECRET}").as_str())
                        );
                        export.spans
                    }
                    Event::Checkpoint(_) => Vec::new(),
                })
                .collect()
        });
        (rest, status, spans)
    }
}

/// What the server answers when the client talks to it with no proxy in between.
async fn direct(spec: ChildSpec, input: &[u8]) -> (Vec<u8>, ExitStatus) {
    let mut child = TokioChildProcess::new().spawn(spec).await.unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let input = input.to_vec();
    let writer = async move {
        stdin.write_all(&input).await.unwrap();
        drop(stdin);
    };
    let reader = async {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.unwrap();
        out
    };
    let ((), out) = tokio::join!(writer, reader);
    (out, child.wait().await.unwrap())
}

/// A session with every kind of message, a request line over 1 MiB and an answer over 2 MiB.
fn transcript() -> Vec<u8> {
    let mut lines = vec![
        r#"{"id":0,"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"process-test","version":"0.1"}}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
    ];
    for id in 1..=40 {
        lines.push(format!(
            r#"{{"id":{id},"jsonrpc":"2.0","method":"tools/call","params":{{"name":"echo","arguments":{{"n":{id},"text":"héllo \"quoted\" {{}}"}}}}}}"#
        ));
    }
    lines.push(format!(
        r#"{{"id":41,"jsonrpc":"2.0","method":"tools/call","params":{{"name":"echo","arguments":{{"blob":"{}"}}}}}}"#,
        "z".repeat(1_500_000)
    ));
    lines.push(r#"{"id":42,"jsonrpc":"2.0","method":"big","params":{"bytes":2500000}}"#.to_owned());
    lines.push(r#"{"id":"s-43","jsonrpc":"2.0","method":"fail","params":{"name":"x"}}"#.to_owned());
    lines.push(r#"{"id":44,"jsonrpc":"2.0","method":"unknown"}"#.to_owned());
    lines.push(r#"{"id":45,"jsonrpc":"2.0","method":"sleep","params":{"ms":30}}"#.to_owned());
    let mut bytes = lines.join("\n").into_bytes();
    bytes.push(b'\n');
    bytes
}

fn registry() -> Registry {
    Registry::parse(
        "registry.json",
        include_bytes!("../../../conformance/registry.json"),
    )
    .unwrap()
}

fn text<'a>(span: &'a RecordedSpan, key: &str) -> Option<&'a str> {
    get(&span.attributes, key).and_then(Value::as_str)
}

fn by_id<'a>(spans: &'a [RecordedSpan], id: &str) -> &'a RecordedSpan {
    spans
        .iter()
        .find(|span| text(span, "jsonrpc.request.id") == Some(id))
        .unwrap_or_else(|| panic!("no span for request {id}"))
}

/// Every attribute is a registry key of the right type, nothing forbidden is sent, and the derived fields say
/// which proxy captured the row.
fn check_wire_contract(spans: &[RecordedSpan], capture_source: &str) {
    let registry = registry();
    for span in spans {
        for (key, value) in span.attributes.iter().chain(&span.resource) {
            assert!(registry.key(key).is_some(), "{key} is not a registry key");
            assert!(!registry.is_forbidden(key), "{key} is forbidden");
            assert_eq!(registry.type_error(key, value), None);
        }
        let field = |name: &str| registry.derive(name, span).unwrap_or_default();
        assert_eq!(field("capture_source"), capture_source);
        assert_eq!(field("side"), "server");
        assert!(text(span, "mcp.method.name").is_some());
        assert!(span.end >= span.start);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytes_are_identical_with_and_without_the_proxy_in_every_collector_state() {
    let env = prod_env();
    let input = transcript();
    let flags = ["--banner", "--eof-exit", "0"];
    let (baseline, status) = direct(server(&flags), &input).await;
    assert_eq!(status, ExitStatus::Code(0));
    // The banner, then one answer per request (the notification has none).
    assert_eq!(baseline.iter().filter(|byte| **byte == b'\n').count(), 47);
    assert!(baseline.len() > 2_500_000);

    for sink in [Sink::Healthy, Sink::Refused, Sink::Stalled, Sink::Absent] {
        let started = env.clock.now();
        let mut proxy = start(&env, server(&flags), sink).await;
        // The client writes while it reads, as a real one does; the answers do not fit in any pipe.
        let mut to_proxy = proxy.to_proxy.take().unwrap();
        let writer = {
            let input = input.clone();
            env.spawn("client-writer", async move {
                // Uneven writes, so chunk boundaries fall inside messages.
                for chunk in input.chunks(7_919) {
                    to_proxy.write_all(chunk).await.unwrap();
                }
                drop(to_proxy);
            })
        };
        let (output, status, spans) = proxy.finish().await;
        drop(writer);
        assert_eq!(status, ExitStatus::Code(0), "{sink:?}");
        // The answers carry the length and hash of each request line, so equal output means the server read
        // the same bytes too.
        assert!(
            output == baseline,
            "{sink:?}: the output differs from the direct run"
        );
        let elapsed = env.clock.now().saturating_duration_since(started);
        assert!(
            elapsed < Duration::from_secs(20),
            "{sink:?} took {elapsed:?}"
        );

        if sink == Sink::Healthy {
            check_wire_contract(&spans, "proxy_stdio");
            // 46 requests and one notification.
            assert_eq!(
                spans.len(),
                47,
                "{:?}",
                spans.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            let initialize = by_id(&spans, "0");
            assert_eq!(text(initialize, "mcp.protocol.version"), Some("2025-06-18"));
            assert_eq!(text(initialize, "crucible.mcp.era"), Some("legacy"));
            let call = by_id(&spans, "7");
            assert_eq!(call.name, "tools/call echo");
            assert_eq!(text(call, "gen_ai.tool.name"), Some("echo"));
            assert_eq!(text(call, "crucible.mcp.client.name"), Some("process-test"));
            assert_eq!(text(call, "crucible.mcp.transport"), Some("stdio"));
            assert_eq!(text(call, "crucible.mcp.result_type"), Some("complete"));
            assert_eq!(text(call, "error.type"), None);
            let large = by_id(&spans, "41");
            assert!(matches!(
                get(&large.attributes, "crucible.mcp.request.bytes"),
                Some(Value::Int(bytes)) if *bytes > 1_500_000
            ));
            assert!(matches!(
                get(&by_id(&spans, "42").attributes, "crucible.mcp.response.bytes"),
                Some(Value::Int(bytes)) if *bytes > 2_500_000
            ));
            let failed = by_id(&spans, "s-43");
            assert_eq!(
                text(failed, "crucible.mcp.error.class"),
                Some("tool_execution")
            );
            assert_eq!(text(failed, "error.type"), Some("tool_error"));
            let unknown = by_id(&spans, "44");
            assert_eq!(text(unknown, "rpc.response.status_code"), Some("-32601"));
            assert_eq!(text(unknown, "crucible.mcp.error.class"), Some("protocol"));
            let slow = by_id(&spans, "45");
            assert!(slow.end.micros() - slow.start.micros() >= 30_000);
            assert!(spans.iter().any(|span| {
                text(span, "mcp.method.name") == Some("notifications/initialized")
                    && get(&span.attributes, "crucible.mcp.is_notification")
                        == Some(&Value::Bool(true))
            }));
            // No planted argument text reaches telemetry: content capture is off.
            assert!(!format!("{spans:?}").contains("quoted"));
        } else {
            assert!(spans.is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_codes_and_a_crash_in_mid_answer_pass_through() {
    let env = prod_env();
    let mut proxy = start(&env, server(&[]), Sink::Refused).await;
    proxy.send(b"{\"id\":1,\"method\":\"ping\"}\n").await;
    assert!(
        proxy
            .line()
            .await
            .starts_with(b"{\"jsonrpc\":\"2.0\",\"id\":1,")
    );
    proxy
        .send(b"{\"id\":2,\"method\":\"exit\",\"params\":{\"code\":7}}\n")
        .await;
    let (rest, status, _) = proxy.finish().await;
    assert_eq!(status, ExitStatus::Code(7));
    assert!(rest.is_empty());

    // A crash: the half answer arrives exactly as the server wrote it, and the pending request is reported as
    // a transport error.
    let input = b"{\"id\":1,\"method\":\"ping\"}\n{\"id\":2,\"method\":\"crash\"}\n";
    let (baseline, direct_status) = direct(server(&[]), input).await;
    assert_eq!(direct_status, ExitStatus::Code(3));
    assert!(!baseline.ends_with(b"\n"));
    let mut proxy = start(&env, server(&[]), Sink::Healthy).await;
    proxy.send(input).await;
    let (output, status, spans) = proxy.finish().await;
    assert_eq!(status, ExitStatus::Code(3));
    assert_eq!(output, baseline);
    check_wire_contract(&spans, "proxy_stdio");
    let crashed = by_id(&spans, "2");
    assert_eq!(text(crashed, "crucible.mcp.error.class"), Some("transport"));
    assert_eq!(text(crashed, "error.type"), Some("transport"));
    assert_eq!(text(by_id(&spans, "1"), "crucible.mcp.error.class"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_end_of_the_clients_input_closes_the_childs_input() {
    let env = prod_env();
    let mut proxy = start(&env, server(&["--eof-exit", "5"]), Sink::Absent).await;
    proxy.send(b"{\"id\":1,\"method\":\"ping\"}\n").await;
    proxy.line().await;
    let (rest, status, _) = proxy.finish().await;
    assert_eq!(status, ExitStatus::Code(5));
    assert!(rest.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signal_is_forwarded_and_the_child_ends_by_it() {
    let env = prod_env();
    let mut proxy = start(&env, server(&[]), Sink::Absent).await;
    proxy.send(b"{\"id\":1,\"method\":\"ping\"}\n").await;
    proxy.line().await;
    proxy
        .signals
        .unbounded_send(ChildSignal::Terminate)
        .unwrap();
    // The client's input stays open: only the signal ends the child.
    let status = env
        .timeout(Duration::from_secs(30), proxy.run.join())
        .await
        .expect("the child ended")
        .unwrap()
        .unwrap();
    if cfg!(unix) {
        assert_eq!(status, ExitStatus::Signal(15));
    } else {
        // Windows has no signals: the job is ended, and its processes exit with code 1.
        assert_eq!(status, ExitStatus::Code(1));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_captured_standard_error_is_copied_and_never_reaches_standard_output() {
    let env = prod_env();
    let spec = ChildSpec {
        stderr: StderrMode::Pipe,
        ..server(&["--stderr"])
    };
    let mut proxy = start(&env, spec, Sink::Absent).await;
    proxy.send(b"{\"id\":1,\"method\":\"ping\"}\n").await;
    let answer = proxy.line().await;
    assert!(answer.starts_with(b"{\"jsonrpc\""));
    let mut stderr = std::mem::replace(&mut proxy.stderr, tokio::io::duplex(1).0);
    let (rest, status, _) = proxy.finish().await;
    assert_eq!(status, ExitStatus::Code(0));
    assert!(rest.is_empty());
    let mut copied = String::new();
    stderr.read_to_string(&mut copied).await.unwrap();
    assert_eq!(copied.replace('\r', ""), "ready\nbye\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_collector_does_not_delay_the_exit_beyond_the_flush_deadline() {
    let env = prod_env();
    let mut proxy = start(&env, server(&[]), Sink::Stalled).await;
    for id in 0..200 {
        proxy
            .send(format!("{{\"id\":{id},\"method\":\"ping\"}}\n").as_bytes())
            .await;
        proxy.line().await;
    }
    drop(proxy.to_proxy.take());
    let status = proxy.run.join().await.unwrap().unwrap();
    assert_eq!(status, ExitStatus::Code(0));
    let started = env.clock.now();
    assert!(
        proxy.telemetry.finish().await.is_none(),
        "the export cannot have finished"
    );
    let waited = env.clock.now().saturating_duration_since(started);
    assert!(waited >= Duration::from_millis(450), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
}

/// Whether something still listens on a loopback port.
async fn listening(env: &Env, port: u16) -> bool {
    let deadline = env.clock.now().saturating_add(Duration::from_secs(2));
    env.net
        .connect(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            deadline,
        )
        .await
        .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_child_ends_the_processes_it_started() {
    // Unix: the process group is killed. Windows: the Job Object closes with kill-on-close.
    let env = prod_env();
    let children: Arc<dyn ChildProcess> = Arc::new(TokioChildProcess::new());
    let mut child = children.spawn(server(&["--spawn-listener"])).await.unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin
        .write_all(b"{\"id\":1,\"method\":\"port\"}\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    while !answer.ends_with(b"\n") {
        answer.push(stdout.read_u8().await.unwrap());
    }
    let answer = String::from_utf8(answer).unwrap();
    let port: u16 = answer
        .split("\"port\":")
        .nth(1)
        .and_then(|rest| {
            rest.trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()
        })
        .unwrap_or_else(|| panic!("no port in {answer}"));
    assert!(
        listening(&env, port).await,
        "the grandchild listens on {port}"
    );
    drop(child);
    for _ in 0..100 {
        if !listening(&env, port).await {
            return;
        }
        env.clock.sleep(Duration::from_millis(50)).await;
    }
    panic!("the grandchild outlived its tree");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cmd_shim_is_found_through_pathext_and_proxied() {
    use std::io::Write as _;

    // `shim` with no extension resolves to `shim.cmd`, which starts the scripted server as its own child:
    // the suspended start, the Job Object and the command resolution all take part.
    let dir = tempfile::tempdir().unwrap();
    let mut shim = tempfile::Builder::new()
        .prefix("shim")
        .suffix(".cmd")
        .tempfile_in(dir.path())
        .unwrap();
    write!(shim, "@echo off\r\n\"{RIG}\" server %*\r\n").unwrap();
    let path = shim.into_temp_path();
    let without_extension = path.to_str().unwrap().trim_end_matches(".cmd").to_owned();
    let env = prod_env();
    let spec = ChildSpec {
        program: without_extension,
        args: vec!["--eof-exit".to_owned(), "9".to_owned()],
        ..ChildSpec::default()
    };
    let mut proxy = start(&env, spec, Sink::Absent).await;
    proxy.send(b"{\"id\":1,\"method\":\"ping\"}\n").await;
    assert!(
        proxy
            .line()
            .await
            .starts_with(b"{\"jsonrpc\":\"2.0\",\"id\":1,")
    );
    let (_, status, _) = proxy.finish().await;
    assert_eq!(status, ExitStatus::Code(9));
}
