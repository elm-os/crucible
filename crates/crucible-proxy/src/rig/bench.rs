//! `crucible-proxy-rig bench`: the overhead benchmark of the proxy (invariant I11: added p99 at most 1 ms).
//!
//! It measures the round trip a client observes with and without the proxy in between, in rounds that
//! alternate the two so that drift of the machine cancels, and repeats the measurement with the collector
//! healthy, refusing connections and never answering. The proxy is the real `crucible` binary (`--crucible`).
//!
//! - stdio: the scripted server of this rig as the child; sequential calls, and a pipelined run that sends
//!   100 requests before it reads.
//! - http: a small HTTP server in this process as the upstream; JSON answers, and event-stream answers with
//!   the time to the first byte.
//!
//! It prints a Markdown table and exits with 1 when an added p99 is over the gate.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const SECRET: &str = "csk_bench000000000000000000000000000000000000000";
const GATE: Duration = Duration::from_millis(1);
const ROUNDS: usize = 20;
const PIPELINE: usize = 100;

struct Options {
    crucible: String,
    calls: usize,
    label: String,
}

fn options(args: &[String]) -> Options {
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|at| args.get(at + 1))
            .cloned()
    };
    Options {
        crucible: value("--crucible").expect("--crucible <path to the crucible binary>"),
        calls: value("--calls").map_or(100_000, |calls| calls.parse().expect("--calls <number>")),
        label: value("--label").unwrap_or_else(|| "unlabelled host".to_owned()),
    }
}

/// The state of the collector for a run, and the endpoint the proxy is given.
#[derive(Clone, Copy, Debug)]
enum Collector {
    Healthy,
    Refused,
    Stalled,
}

impl Collector {
    const ALL: [Self; 3] = [Self::Healthy, Self::Refused, Self::Stalled];

    fn name(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Refused => "refused",
            Self::Stalled => "stalled",
        }
    }

    /// Starts the collector and returns its endpoint.
    fn start(self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        match self {
            Self::Refused => drop(listener),
            Self::Stalled => {
                std::thread::spawn(move || {
                    let mut open = Vec::new();
                    for stream in listener.incoming().flatten() {
                        open.push(stream);
                    }
                });
            }
            Self::Healthy => {
                std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        std::thread::spawn(move || serve_http(stream, false));
                    }
                });
            }
        }
        endpoint
    }
}

/// Reads one HTTP/1.1 message head and its `Content-Length` body. `None` at end of stream.
fn read_message(reader: &mut BufReader<TcpStream>) -> Option<(String, Vec<u8>)> {
    let mut head = String::new();
    loop {
        let before = head.len();
        if reader.read_line(&mut head).ok()? == 0 {
            return None;
        }
        if head.len() - before <= 2 {
            break;
        }
    }
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some((head, body))
}

const JSON_ANSWER: &str =
    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}"#;

/// A keep-alive HTTP server for one connection: an OTLP sink, or (with `mcp`) the benchmark's MCP upstream,
/// which answers a request whose body contains `"stream"` with an event stream of three events.
fn serve_http(stream: TcpStream, mcp: bool) {
    let _nodelay = stream.set_nodelay(true);
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    while let Some((_head, body)) = read_message(&mut reader) {
        let sent = if mcp && body.windows(8).any(|window| window == b"\"stream\"") {
            let event = format!("event: message\ndata: {JSON_ANSWER}\n\n");
            let chunk = format!("{:x}\r\n{event}\r\n", event.len());
            write!(
                writer,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            )
            .and_then(|()| {
                for _ in 0..3 {
                    writer.write_all(chunk.as_bytes())?;
                    writer.flush()?;
                }
                writer.write_all(b"0\r\n\r\n")
            })
        } else {
            let answer = if mcp { JSON_ANSWER } else { "" };
            write!(
                writer,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}",
                answer.len()
            )
        };
        if sent.is_err() {
            return;
        }
    }
}

fn percentile(sorted: &[Duration], percent: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * percent / 100.0).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn millis(duration: Duration) -> String {
    format!("{:.4}", duration.as_secs_f64() * 1_000.0)
}

/// One row of the report: the two variants and what the proxy added. Returns whether the gate held.
fn report(
    path: &str,
    collector: Collector,
    mut direct: Vec<Duration>,
    mut proxied: Vec<Duration>,
) -> bool {
    direct.sort_unstable();
    proxied.sort_unstable();
    let (d50, d99) = (percentile(&direct, 50.0), percentile(&direct, 99.0));
    let (p50, p99) = (percentile(&proxied, 50.0), percentile(&proxied, 99.0));
    let added = |with: Duration, without: Duration| with.as_secs_f64() - without.as_secs_f64();
    let (a50, a99) = (added(p50, d50), added(p99, d99));
    let pass = a99 <= GATE.as_secs_f64();
    println!(
        "| {path} | {} | {} | {} | {} | {} | {:.4} | {:.4} | {} |",
        collector.name(),
        millis(d50),
        millis(d99),
        millis(p50),
        millis(p99),
        a50 * 1_000.0,
        a99 * 1_000.0,
        if pass { "pass" } else { "FAIL" }
    );
    pass
}

struct StdioPeer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    line: String,
}

impl StdioPeer {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        Self {
            stdin: child.stdin.take().expect("stdin"),
            stdout: BufReader::new(child.stdout.take().expect("stdout")),
            child,
            line: String::new(),
        }
    }

    fn call(&mut self, id: usize) -> Duration {
        let request = format!(
            "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{{\"name\":\"echo\",\"arguments\":{{\"text\":\"hello\"}}}}}}\n"
        );
        let started = Instant::now();
        self.stdin.write_all(request.as_bytes()).expect("write");
        self.line.clear();
        self.stdout.read_line(&mut self.line).expect("read");
        started.elapsed()
    }

    /// Sends `PIPELINE` requests, then reads their answers; returns the time per request.
    fn pipelined(&mut self, first_id: usize) -> Duration {
        let mut batch = String::new();
        for id in first_id..first_id + PIPELINE {
            batch.push_str(&format!(
                "{{\"id\":{id},\"jsonrpc\":\"2.0\",\"method\":\"ping\"}}\n"
            ));
        }
        let started = Instant::now();
        self.stdin.write_all(batch.as_bytes()).expect("write");
        for _ in 0..PIPELINE {
            self.line.clear();
            self.stdout.read_line(&mut self.line).expect("read");
        }
        started.elapsed() / PIPELINE as u32
    }
}

impl Drop for StdioPeer {
    fn drop(&mut self) {
        let _killed = self.child.kill();
        let _reaped = self.child.wait();
    }
}

fn bench_stdio(options: &Options) -> bool {
    let rig = std::env::current_exe().expect("exe");
    let mut pass = true;
    for collector in Collector::ALL {
        let endpoint = collector.start();
        let mut direct_command = Command::new(&rig);
        direct_command.arg("server");
        let mut proxy_command = Command::new(&options.crucible);
        proxy_command
            .args(["proxy", "stdio", "--endpoint", &endpoint, "--"])
            .arg(&rig)
            .arg("server")
            .env("CRUCIBLE_SECRET_KEY", SECRET);
        let mut direct = StdioPeer::start(direct_command);
        let mut proxied = StdioPeer::start(proxy_command);
        for id in 0..2_000 {
            direct.call(id);
            proxied.call(id);
        }
        let per_round = options.calls / ROUNDS;
        let (mut direct_times, mut proxied_times) = (Vec::new(), Vec::new());
        let (mut direct_pipelined, mut proxied_pipelined) = (Vec::new(), Vec::new());
        for round in 0..ROUNDS {
            for call in 0..per_round {
                direct_times.push(direct.call(round * per_round + call));
            }
            for call in 0..per_round {
                proxied_times.push(proxied.call(round * per_round + call));
            }
            for batch in 0..(per_round / PIPELINE / 10).max(1) {
                direct_pipelined.push(direct.pipelined(batch * PIPELINE));
                proxied_pipelined.push(proxied.pipelined(batch * PIPELINE));
            }
        }
        pass &= report("stdio, sequential", collector, direct_times, proxied_times);
        pass &= report(
            "stdio, pipelined (per request)",
            collector,
            direct_pipelined,
            proxied_pipelined,
        );
    }
    pass
}

struct HttpPeer {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl HttpPeer {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect");
        let _nodelay = stream.set_nodelay(true);
        Self {
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
        }
    }

    /// One POST. Returns the time to the first byte of the response and to its end.
    fn call(&mut self, stream: bool) -> (Duration, Duration) {
        let body = if stream {
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"stream"}}"#
        } else {
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello"}}}"#
        };
        let started = Instant::now();
        write!(
            self.writer,
            "POST /mcp HTTP/1.1\r\nhost: bench.test\r\ncontent-type: application/json\r\nauthorization: Bearer opaque\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("write");
        let mut first = [0u8; 1];
        self.reader.read_exact(&mut first).expect("first byte");
        let first_byte = started.elapsed();
        let mut head = String::from(first[0] as char);
        loop {
            let before = head.len();
            self.reader.read_line(&mut head).expect("head");
            if head.len() - before <= 2 {
                break;
            }
        }
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            loop {
                let mut size = String::new();
                self.reader.read_line(&mut size).expect("chunk size");
                let size = usize::from_str_radix(size.trim(), 16).expect("chunk size");
                let mut chunk = vec![0u8; size + 2];
                self.reader.read_exact(&mut chunk).expect("chunk");
                if size == 0 {
                    break;
                }
            }
        } else {
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")?
                        .trim()
                        .parse()
                        .ok()
                })
                .expect("content-length");
            let mut body = vec![0u8; length];
            self.reader.read_exact(&mut body).expect("body");
        }
        (first_byte, started.elapsed())
    }
}

fn bench_http(options: &Options) -> bool {
    let upstream = TcpListener::bind("127.0.0.1:0").expect("bind");
    let upstream_addr = upstream.local_addr().expect("address");
    std::thread::spawn(move || {
        for stream in upstream.incoming().flatten() {
            std::thread::spawn(move || serve_http(stream, true));
        }
    });
    let mut pass = true;
    for collector in Collector::ALL {
        let endpoint = collector.start();
        let mut proxy = Command::new(&options.crucible)
            .args(["proxy", "http", "--listen", "127.0.0.1:0", "--upstream"])
            .arg(format!("http://{upstream_addr}"))
            .args(["--endpoint", &endpoint])
            .env("CRUCIBLE_SECRET_KEY", SECRET)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the proxy");
        let mut stderr = BufReader::new(proxy.stderr.take().expect("stderr"));
        let proxy_addr: SocketAddr = loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).expect("stderr"),
                0,
                "the proxy exited"
            );
            if let Some(rest) = line.split("listen=").nth(1) {
                break rest
                    .split_whitespace()
                    .next()
                    .expect("address")
                    .parse()
                    .expect("address");
            }
        };
        // Keep draining the proxy's log so it never blocks on a full pipe.
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _read = stderr.read_to_end(&mut sink);
        });
        let mut direct = HttpPeer::connect(upstream_addr);
        let mut proxied = HttpPeer::connect(proxy_addr);
        for _ in 0..2_000 {
            direct.call(false);
            proxied.call(false);
        }
        let per_round = options.calls / ROUNDS;
        let mut times: [Vec<Duration>; 6] = Default::default();
        for _ in 0..ROUNDS {
            for _ in 0..per_round {
                times[0].push(direct.call(false).1);
            }
            for _ in 0..per_round {
                times[1].push(proxied.call(false).1);
            }
            for _ in 0..(per_round / 10).max(1) {
                let (first, total) = direct.call(true);
                times[2].push(first);
                times[4].push(total);
            }
            for _ in 0..(per_round / 10).max(1) {
                let (first, total) = proxied.call(true);
                times[3].push(first);
                times[5].push(total);
            }
        }
        let [
            json_direct,
            json_proxied,
            first_direct,
            first_proxied,
            sse_direct,
            sse_proxied,
        ] = times;
        pass &= report("http, JSON answer", collector, json_direct, json_proxied);
        pass &= report(
            "http, SSE first byte",
            collector,
            first_direct,
            first_proxied,
        );
        pass &= report("http, SSE whole stream", collector, sse_direct, sse_proxied);
        let _killed = proxy.kill();
        let _reaped = proxy.wait();
    }
    pass
}

/// Runs the benchmark. Never returns.
pub fn run(args: &[String]) -> ! {
    let options = options(args);
    println!(
        "Host: {}. {} calls per path and collector state, in {ROUNDS} alternating rounds.",
        options.label, options.calls
    );
    println!();
    println!(
        "| Path | Collector | Direct p50 ms | Direct p99 ms | Proxy p50 ms | Proxy p99 ms | Added p50 ms | Added p99 ms | Gate (added p99 at most 1 ms) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let stdio = bench_stdio(&options);
    let http = bench_http(&options);
    std::process::exit(i32::from(!(stdio && http)))
}
