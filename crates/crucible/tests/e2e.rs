//! Milestone 1 end to end, on real infrastructure: the `crucible serve` binary in front of a real ClickHouse.
//!
//! A span sent over OTLP goes through ingest, the WAL, the shipper and ClickHouse, and comes back through the
//! REST API and, when a Chrome binary is given, the UI's traces table. A load burst and a `kill -9` in the
//! middle of three further runs must lose no acknowledged span and duplicate no row.
//!
//! The test needs a ClickHouse 26.3 or newer whose admin user may manage access, so it runs only when
//! `CRUCIBLE_E2E_CLICKHOUSE_URL` is set and passes as skipped otherwise:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `CRUCIBLE_E2E_CLICKHOUSE_URL` | The HTTP endpoint, for example `http://localhost:8123` |
//! | `CRUCIBLE_E2E_BIN` | The server binary, built on its own with `cargo build -p crucible --release` (with `--features embed-ui` for the UI check), so that no test-only feature of this crate's dev-dependencies is compiled in |
//! | `CRUCIBLE_E2E_CLICKHOUSE_USER` | The admin user; `crucible_admin` when unset |
//! | `CRUCIBLE_E2E_CLICKHOUSE_PASSWORD` | Its password |
//! | `CRUCIBLE_E2E_CHROME` | A Chrome or Chromium binary. When set, the UI check runs; it needs Node 24 and a binary built with `embed-ui` |
//! | `CRUCIBLE_E2E_NODE` | The Node binary; `node` when unset |
//! | `CRUCIBLE_E2E_OUT` | A directory for `report.json`, the server logs and the UI screenshot |
//! | `CRUCIBLE_E2E_POSTGRES_URL` | A Postgres 15 or newer, for example `postgres://crucible:e2e@localhost:5432/crucible`. When set, the control-plane path runs: the first owner signs in, creates a project and an API key, ingests with the key and reads the trace back with the session |
//!
//! `docs/self-hosting/quickstart.md` has the commands.

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crucible_env::{Env, StoreError, TlsRoots};
use crucible_env_prod::ProdConfig;
use crucible_loadgen::{
    AckLedger, LossCheck, Pool, PoolConfig, RUN_ID_KEY, RunConfig, SEQ_KEY, SequenceReader, verify,
};
use crucible_records::{Timestamp, Uuid};
use crucible_schema::ServiceIdentity;
use crucible_store_clickhouse::render::Statement;
use crucible_store_clickhouse::{ClickHouseStore, Credentials, StoreConfig};
use serde_json::{Value, json};

const MASTER_KEY: &str = "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e";
const SPANS_PER_REQUEST: u32 = 100;
/// How long the shipper gets to land everything that was acknowledged.
const DRAIN: Duration = Duration::from_secs(120);

fn secret(fill: char) -> String {
    format!("csk_{}", fill.to_string().repeat(43))
}

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// One HTTP/1.1 exchange on a fresh connection: the status and the body.
fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no response head"))?;
    let status = std::str::from_utf8(&response[..end])
        .ok()
        .and_then(|head| head.split(' ').nth(1)?.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status"))?;
    Ok((status, response[end + 4..].to_vec()))
}

/// A running `crucible serve`.
struct Server {
    child: Child,
    api: SocketAddr,
    ingest: SocketAddr,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            eprintln!("--- {} ---\n{log}", self.log.display());
        }
    }
}

/// Where the test keeps what it found.
struct Rig {
    binary: PathBuf,
    clickhouse_url: String,
    admin: Credentials,
    dir: PathBuf,
    out: PathBuf,
    starts: u32,
    /// The control plane, when the run has one.
    postgres_url: Option<String>,
}

impl Rig {
    /// Starts the binary on fresh ports over the rig's data directory and waits until `/readyz` answers 200.
    fn start(&mut self, migrate: bool) -> Server {
        self.starts += 1;
        let log = self.out.join(format!("serve-{}.log", self.starts));
        let (api, ingest, metrics) = (free_addr(), free_addr(), free_addr());
        let mut command = Command::new(&self.binary);
        command
            .arg("serve")
            .args(["--listen", &api.to_string()])
            .args(["--ingest-listen", &ingest.to_string()])
            .args(["--metrics-listen", &metrics.to_string()])
            .arg("--data-dir")
            .arg(self.dir.join("data"))
            .arg("--keys-file")
            .arg(self.dir.join("keys.toml"))
            .args(["--clickhouse-url", &self.clickhouse_url])
            .args(["--clickhouse-user", &self.admin.user])
            .arg("--clickhouse-password-file")
            .arg(self.dir.join("clickhouse-password"))
            .arg("--master-key-file")
            .arg(self.dir.join("master-key"))
            .env_remove("CRUCIBLE_CLICKHOUSE_URL")
            .env_remove("CRUCIBLE_POSTGRES_URL")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        if migrate {
            command.arg("--migrate");
        }
        if let Some(url) = &self.postgres_url {
            command.args(["--postgres-url", url]);
        }
        let mut server = Server {
            child: command.spawn().unwrap(),
            api,
            ingest,
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut last = String::new();
        loop {
            if let Ok((status, body)) = http(api, "GET", "/readyz", &[], b"") {
                if status == 200 {
                    return server;
                }
                last = String::from_utf8_lossy(&body).into_owned();
            }
            let exited = server.child.try_wait().unwrap();
            assert!(
                exited.is_none(),
                "crucible serve exited during startup: {exited:?}"
            );
            assert!(
                Instant::now() < deadline,
                "crucible serve was not ready in time: {last}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Reads ClickHouse as the admin user, around the API, so nothing de-duplicates or merges what is stored.
struct Stored {
    store: ClickHouseStore,
    project: String,
}

impl Stored {
    async fn text(&self, sql: String) -> Result<String, StoreError> {
        let statement = Statement {
            sql,
            params: Vec::new(),
            settings: Vec::new(),
        };
        self.store
            .raw_text(ServiceIdentity::Admin, &statement)
            .await
    }

    async fn numbers(&self, sql: String) -> Vec<u64> {
        let text = self.text(sql.clone()).await.expect(&sql);
        text.split_whitespace()
            .map(|n| n.parse().unwrap_or_else(|_| panic!("{sql}: {text}")))
            .collect()
    }

    /// Row counts per table for the project, and how many of them are distinct.
    async fn counts(&self) -> BTreeMap<&'static str, u64> {
        let project = &self.project;
        let one = |sql: String| async move { self.numbers(sql).await[0] };
        let from = |table: &str, select: &str| {
            format!("SELECT {select} FROM crucible.{table} WHERE project_id = toUUID('{project}')")
        };
        BTreeMap::from([
            ("span_core_rows", one(from("span_core", "count()")).await),
            (
                "span_core_distinct_spans",
                one(from("span_core", "uniqExact(trace_id, span_id)")).await,
            ),
            (
                "span_payload_rows",
                one(from("span_payload", "count()")).await,
            ),
            (
                "span_payload_distinct_spans",
                one(from("span_payload", "uniqExact(trace_id, span_id)")).await,
            ),
            (
                "trace_heads_rows",
                one(from("trace_heads", "count()")).await,
            ),
            (
                "trace_heads_distinct_traces",
                one(from("trace_heads", "uniqExact(trace_id)")).await,
            ),
            (
                "trace_stats_span_count",
                one(from("trace_stats", "sum(span_count)")).await,
            ),
            (
                "span_core_distinct_traces",
                one(from("span_core", "uniqExact(trace_id)")).await,
            ),
            (
                "span_metrics_count",
                one(from("span_metrics", "sum(count)")).await,
            ),
            (
                "span_metrics_durations",
                one(from(
                    "span_metrics",
                    "sum(arraySum(tupleElement(dur_hist, 2)))",
                ))
                .await,
            ),
            (
                "span_core_in_sessions",
                one(from("span_core", "countIf(session_id != '')")).await,
            ),
            (
                "session_rollup_span_count",
                one(from("session_rollup", "sum(span_count)")).await,
            ),
            (
                "span_core_of_users",
                one(from("span_core", "countIf(user_id != '')")).await,
            ),
            (
                "user_rollup_span_count",
                one(from("user_rollup", "sum(span_count)")).await,
            ),
        ])
    }
}

impl SequenceReader for Stored {
    type Error = StoreError;

    /// The `loadgen.seq` of every stored span of the run, duplicates included, from the payload tier.
    async fn sequences(&self, run_id: Uuid) -> Result<Vec<u64>, StoreError> {
        let text = self
            .text(format!(
                "SELECT toUInt64(JSONExtractString(attributes, '{SEQ_KEY}')) FROM crucible.span_payload \
                 WHERE project_id = toUUID('{}') AND JSONExtractString(attributes, '{RUN_ID_KEY}') = '{run_id}'",
                self.project
            ))
            .await?;
        Ok(text
            .split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect())
    }
}

/// Polls until no acknowledged span of the run is missing from the store, then returns the check.
async fn drained(stored: &Stored, ledger: &AckLedger) -> (LossCheck, Duration) {
    let start = Instant::now();
    loop {
        let check = verify(ledger, stored).await.unwrap();
        if check.lost_spans == 0 || start.elapsed() > DRAIN {
            return (check, start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The table counts must describe one row per stored span and one head per trace.
fn assert_no_duplicate_rows(counts: &BTreeMap<&'static str, u64>, at_least: u64, when: &str) {
    let spans = counts["span_core_rows"];
    assert!(spans >= at_least, "{when}: {counts:?}");
    for name in [
        "span_core_distinct_spans",
        "span_payload_rows",
        "span_payload_distinct_spans",
        "trace_stats_span_count",
        "span_metrics_count",
        "span_metrics_durations",
    ] {
        assert_eq!(counts[name], spans, "{when}: {name} in {counts:?}");
    }
    // The session and user rollups count exactly the spans that name one (invariant I7).
    assert_eq!(
        counts["session_rollup_span_count"], counts["span_core_in_sessions"],
        "{when}: {counts:?}"
    );
    assert_eq!(
        counts["user_rollup_span_count"], counts["span_core_of_users"],
        "{when}: {counts:?}"
    );
    assert_eq!(
        counts["trace_heads_rows"], counts["trace_heads_distinct_traces"],
        "{when}: {counts:?}"
    );
    assert_eq!(
        counts["trace_heads_rows"], counts["span_core_distinct_traces"],
        "{when}: {counts:?}"
    );
}

/// A two-span trace as OTLP/JSON: a root and an erroring child with attributes.
struct Trace {
    trace_id: String,
    root_id: String,
    child_id: String,
    name: String,
    start_nanos: u64,
}

impl Trace {
    fn new(env: &Env, name: &str) -> Self {
        let hex =
            |id: Uuid| -> String { id.to_bytes().iter().map(|b| format!("{b:02x}")).collect() };
        let (a, b) = (hex(env.new_id()), hex(env.new_id()));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        Self {
            trace_id: a.chars().rev().collect(),
            root_id: b[16..].to_owned(),
            child_id: b[16..].chars().rev().collect(),
            name: name.to_owned(),
            // Whole microseconds, so the stored times equal the sent ones.
            start_nanos: u64::try_from(now.as_micros()).unwrap() * 1_000,
        }
    }

    fn json(&self) -> String {
        let span =
            |id: &str, parent: Option<&str>, name: &str, offset: u64, nanos: u64, error: bool| {
                let mut span = json!({
                    "traceId": self.trace_id,
                    "spanId": id,
                    "name": name,
                    "kind": 1,
                    "startTimeUnixNano": (self.start_nanos + offset).to_string(),
                    "endTimeUnixNano": (self.start_nanos + offset + nanos).to_string(),
                    // The session and the user give the session and user rollups rows to hold.
                    "attributes": [
                        {"key": "e2e.case", "value": {"stringValue": self.name}},
                        {"key": "crucible.session.id", "value": {"stringValue": "e2e-session"}},
                        {"key": "user.id", "value": {"stringValue": "e2e-user"}},
                    ],
                });
                if let Some(parent) = parent {
                    span["parentSpanId"] = json!(parent);
                }
                if error {
                    span["status"] = json!({"code": 2, "message": "tool failed"});
                }
                span
            };
        json!({"resourceSpans": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "crucible-e2e"}}]},
            "scopeSpans": [{"scope": {"name": "e2e"}, "spans": [
                span(&self.root_id, None, &self.name, 0, 50_000_000, false),
                span(&self.child_id, Some(&self.root_id), "tool call", 1_000_000, 20_000_000, true),
            ]}],
        }]})
        .to_string()
    }

    fn time(&self, offset_nanos: u64) -> String {
        Timestamp::from_unix_nanos(self.start_nanos + offset_nanos).to_string()
    }

    /// Checks the trace as the list and the detail endpoints return it. The list's sums are store sums plus the
    /// recent-index entries above the watermark, so they can run ahead between an insert and its checkpoint
    /// (contract 8.9); `settled` says that this window has passed and the sums are exact.
    fn assert_round_trip(&self, list: &Value, detail: &Value, settled: bool, when: &str) {
        let item = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["trace_id"] == self.trace_id.as_str())
            .unwrap_or_else(|| panic!("{when}: {} is not in {list}", self.name));
        let expected = [
            ("name", json!(self.name)),
            ("head_kind", json!("root")),
            ("start_time", json!(self.time(0))),
            ("duration_us", json!(50_000)),
            ("span_count", json!(2)),
            ("error_count", json!(1)),
        ];
        for (field, value) in &expected {
            let sum = matches!(*field, "span_count" | "error_count");
            if settled || !sum {
                assert_eq!(&item[*field], value, "{when}: list {field} in {item}");
            }
        }
        assert_eq!(detail["trace_id"], self.trace_id.as_str(), "{when}");
        assert_eq!(detail["span_count"], 2, "{when}: {detail}");
        let spans = detail["spans"].as_array().unwrap();
        let span = |id: &str| {
            spans
                .iter()
                .find(|span| span["span_id"] == id)
                .unwrap_or_else(|| panic!("{when}: span {id} is not in {detail}"))
        };
        let (root, child) = (span(&self.root_id), span(&self.child_id));
        let expected = [
            (root, "parent_span_id", Value::Null),
            (root, "name", json!(self.name)),
            (root, "start_time", json!(self.time(0))),
            (root, "end_time", json!(self.time(50_000_000))),
            (root, "duration_us", json!(50_000)),
            (root, "status_code", json!("unset")),
            (root, "observation_type", json!("span")),
            (child, "parent_span_id", json!(self.root_id)),
            (child, "name", json!("tool call")),
            (child, "start_time", json!(self.time(1_000_000))),
            (child, "end_time", json!(self.time(21_000_000))),
            (child, "duration_us", json!(20_000)),
            (child, "status_code", json!("error")),
        ];
        for (span, field, value) in &expected {
            assert_eq!(&span[*field], value, "{when}: span {field} in {span}");
        }
    }
}

/// The API as one project's key sees it.
struct Api {
    addr: SocketAddr,
    project: String,
    bearer: String,
}

impl Api {
    fn get(&self, rest: &str) -> (u16, Value) {
        let path = format!("/api/v1/projects/{}{rest}", self.project);
        let (status, body) = http(
            self.addr,
            "GET",
            &path,
            &[("authorization", &self.bearer)],
            b"",
        )
        .unwrap_or_else(|e| panic!("GET {path}: {e}"));
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// The trace list of the two hours around now.
    fn list(&self) -> Value {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let micros = i64::try_from(now.as_micros()).unwrap();
        let hour = 3_600_000_000;
        let (from, to) = (
            Timestamp::from_micros(micros - hour),
            Timestamp::from_micros(micros + hour),
        );
        let (status, list) = self.get(&format!("/traces?from={from}&to={to}&limit=100"));
        assert_eq!(status, 200, "{list}");
        list
    }

    fn detail(&self, trace: &Trace) -> Value {
        let (status, detail) = self.get(&format!("/traces/{}", trace.trace_id));
        assert_eq!(status, 200, "{detail}");
        detail
    }

    fn lists(&self, trace: &Trace) -> bool {
        self.list()["data"].as_array().is_some_and(|data| {
            data.iter()
                .any(|item| item["trace_id"] == trace.trace_id.as_str())
        })
    }
}

/// Sends the trace and returns how long after the acknowledgement the API listed it, and how long after it
/// ClickHouse held both spans.
async fn send_and_time(
    server: &Server,
    api: &Api,
    stored: &Stored,
    trace: &Trace,
    protobuf: bool,
) -> (Duration, Duration) {
    let json = trace.json();
    let (addr, content_type, body) = if protobuf {
        let body = crucible_otlp::transcode_json(json.as_bytes()).unwrap();
        (server.ingest, "application/x-protobuf", body)
    } else {
        (server.api, "application/json", json.into_bytes())
    };
    let headers = [
        ("authorization", api.bearer.as_str()),
        ("content-type", content_type),
    ];
    let (status, response) = http(addr, "POST", "/v1/traces", &headers, &body).unwrap();
    let acked = Instant::now();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&response));
    // A full success rejects nothing: an empty protobuf message, or an empty JSON object.
    assert!(response.is_empty() || response == b"{}", "{response:?}");

    while !api.lists(trace) {
        assert!(
            acked.elapsed() < Duration::from_secs(30),
            "{} never listed",
            trace.name
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let listed = acked.elapsed();
    let count = format!(
        "SELECT count() FROM crucible.span_core WHERE project_id = toUUID('{}') AND trace_id = unhex('{}')",
        stored.project, trace.trace_id
    );
    while stored.numbers(count.clone()).await[0] < 2 {
        assert!(
            acked.elapsed() < DRAIN,
            "{} never reached the store",
            trace.name
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (listed, acked.elapsed())
}

/// The log without the terminal colour codes.
fn plain(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            chars.by_ref().find(|c| *c == 'm');
        } else {
            out.push(c);
        }
    }
    out
}

fn millis(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

fn run_config(server: &Server, bearer: &str, seconds: u64) -> RunConfig {
    RunConfig {
        target: server.ingest,
        authority: server.ingest.to_string(),
        rate: 100,
        duration: Duration::from_secs(seconds),
        concurrency: 16,
        gzip: false,
        timeout: Duration::from_secs(5),
        headers: vec![(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(bearer).unwrap(),
        )],
    }
}

fn pool(env: &Env) -> Arc<Pool> {
    let config = PoolConfig {
        span_size: "1k".parse().unwrap(),
        spans_per_request: SPANS_PER_REQUEST,
        templates: 4,
        seed: 1,
    };
    Arc::new(Pool::build(config, env.new_id()).unwrap())
}

/// Asks the process to stop as an operator would and returns whether it exited cleanly.
#[cfg(unix)]
fn terminate(server: &mut Server) -> bool {
    let pid = server.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            return status.success();
        }
        assert!(Instant::now() < deadline, "crucible serve ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Connects through the UI's dialog in headless Chrome and reads the traces table.
fn ui_check(chrome: &str, rig: &Rig, api: &Api, traces: [&Trace; 2]) -> Value {
    let (status, _) = http(api.addr, "GET", "/traces/", &[], b"").unwrap();
    assert_eq!(
        status, 200,
        "the binary serves no UI; build it with --features embed-ui for the UI check"
    );
    let screenshot = rig.out.join("traces-table.png");
    let node = std::env::var("CRUCIBLE_E2E_NODE").unwrap_or_else(|_| "node".to_owned());
    let output = Command::new(node)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/e2e/ui-traces.mjs"))
        .args(["--chrome", chrome])
        .args(["--url", &format!("http://{}", api.addr)])
        .args(["--project", &api.project])
        .args(["--key", api.bearer.trim_start_matches("Bearer ")])
        .args(["--expect", &traces[0].name, "--expect", &traces[1].name])
        .arg("--screenshot")
        .arg(&screenshot)
        .output()
        .expect("running node");
    assert!(
        output.status.success(),
        "the UI check failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(seen["connected_header"], true, "{seen}");
    assert_eq!(seen["console_errors"], json!([]), "{seen}");
    let rows = seen["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{seen}");
    for trace in traces {
        // Started, Name, Duration, Spans, Cost, Status.
        let row = rows
            .iter()
            .find(|row| row[1] == trace.name.as_str())
            .unwrap_or_else(|| panic!("{} is not in the table: {seen}", trace.name));
        assert_eq!(row[3], "2", "{seen}");
        assert_eq!(row[5], "1 error", "{seen}");
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn otlp_reaches_the_api_and_the_ui_and_survives_a_crash() {
    let Ok(clickhouse_url) = std::env::var("CRUCIBLE_E2E_CLICKHOUSE_URL") else {
        eprintln!("skipped: CRUCIBLE_E2E_CLICKHOUSE_URL is not set");
        return;
    };
    let admin = Credentials {
        user: std::env::var("CRUCIBLE_E2E_CLICKHOUSE_USER")
            .unwrap_or_else(|_| "crucible_admin".to_owned()),
        password: std::env::var("CRUCIBLE_E2E_CLICKHOUSE_PASSWORD")
            .expect("CRUCIBLE_E2E_CLICKHOUSE_PASSWORD is set"),
    };
    let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap();
    let env = &prod.env;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();
    let out = std::env::var_os("CRUCIBLE_E2E_OUT").map_or_else(|| dir.clone(), PathBuf::from);
    std::fs::create_dir_all(&out).unwrap();

    // Fresh projects on every run, so a ClickHouse that earlier runs wrote to is as good as an empty one.
    let (traces_project, load_project) = (env.new_id().to_string(), env.new_id().to_string());
    std::fs::write(
        dir.join("keys.toml"),
        format!(
            "[[project_key]]\nproject_id = \"{traces_project}\"\nsecret = \"{}\"\n\n\
             [[project_key]]\nproject_id = \"{load_project}\"\nsecret = \"{}\"\n",
            secret('a'),
            secret('b')
        ),
    )
    .unwrap();
    std::fs::write(dir.join("master-key"), format!("{MASTER_KEY}\n")).unwrap();
    std::fs::write(dir.join("clickhouse-password"), &admin.password).unwrap();

    let reader = |project: &str| Stored {
        store: ClickHouseStore::new(
            env.clone(),
            StoreConfig {
                url: clickhouse_url.clone(),
                read_url: None,
                admin: admin.clone(),
                ingest: admin.clone(),
                read_ui: admin.clone(),
                read_agent: admin.clone(),
                maint: admin.clone(),
                flavour: None,
                tls_roots: TlsRoots::WebPki,
                connect_timeout: Duration::from_secs(10),
            },
        ),
        project: project.to_owned(),
    };
    let (trace_rows, load_rows) = (reader(&traces_project), reader(&load_project));
    let mut rig = Rig {
        // Never the binary cargo builds beside this test: that one is compiled with the test-only features
        // of the dev-dependencies.
        binary: std::env::var_os("CRUCIBLE_E2E_BIN")
            .map(PathBuf::from)
            .expect("CRUCIBLE_E2E_BIN is set"),
        clickhouse_url: clickhouse_url.clone(),
        admin,
        dir,
        out,
        starts: 0,
        postgres_url: None,
    };
    let load_bearer = format!("Bearer {}", secret('b'));

    // 1. Start with migrations; the startup checks of contract 5.8 pass before `/readyz` answers 200.
    let boot = Instant::now();
    let mut server = rig.start(true);
    let ready_after = boot.elapsed();
    let api = |server: &Server| Api {
        addr: server.api,
        project: traces_project.clone(),
        bearer: format!("Bearer {}", secret('a')),
    };

    // 2. One trace as protobuf on the ingest listener and one as JSON on the main listener.
    let protobuf = Trace::new(env, "e2e protobuf trace");
    let json_trace = Trace::new(env, "e2e json trace");
    let (protobuf_listed, protobuf_stored) =
        send_and_time(&server, &api(&server), &trace_rows, &protobuf, true).await;
    let (json_listed, json_stored) =
        send_and_time(&server, &api(&server), &trace_rows, &json_trace, false).await;
    for trace in [&protobuf, &json_trace] {
        let api = api(&server);
        trace.assert_round_trip(
            &api.list(),
            &api.detail(trace),
            false,
            "while the recent index serves it",
        );
    }
    let (status, bounds) = api(&server).get("/data-bounds");
    assert_eq!(status, 200, "{bounds}");
    assert!(bounds["spans"]["first"].is_string(), "{bounds}");
    // Another project's key sees none of it.
    let other = Api {
        project: load_project.clone(),
        bearer: load_bearer.clone(),
        ..api(&server)
    };
    assert_eq!(other.list()["data"], json!([]));
    let (status, foreign) = other.get(&format!("/traces/{}", protobuf.trace_id));
    assert!(status == 404 || foreign["spans"] == json!([]), "{foreign}");

    // 3. A burst of 20,000 spans: nothing acknowledged is lost and nothing is stored twice.
    let burst_pool = pool(env);
    let burst = crucible_loadgen::run(
        env,
        Arc::clone(&burst_pool),
        &run_config(&server, &load_bearer, 2),
    )
    .await
    .unwrap();
    assert_eq!(
        burst.report.outcome.acked, 200,
        "{:?}",
        burst.report.outcome
    );
    let (burst_check, burst_drain) = drained(&load_rows, &burst.ledger).await;
    let burst_counts = load_rows.counts().await;
    assert_eq!(
        (
            burst_check.acked_spans,
            burst_check.lost_spans,
            burst_check.duplicate_spans
        ),
        (20_000, 0, 0),
        "after the burst: {burst_check:?}"
    );
    assert_no_duplicate_rows(&burst_counts, 20_000, "after the burst");
    assert_eq!(burst_counts["span_core_rows"], 20_000);

    // Once the checkpoint and the visibility grace have passed, the same answers come from the store, whole
    // and strong.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for trace in [&protobuf, &json_trace] {
        let api = api(&server);
        let (list, detail) = (api.list(), api.detail(trace));
        trace.assert_round_trip(&list, &detail, true, "from the store");
        for meta in [&list["meta"], &detail["meta"]] {
            assert_eq!(meta["partial"], false, "{meta}");
            assert_eq!(meta["consistency"], "strong", "{meta}");
        }
    }

    // 4. Stop the server in the middle of a run, three times at different points, each followed by a restart
    // on the same data directory: kill -9 twice, then SIGTERM under load, which must exit cleanly. Everything
    // before a restart is in doubt: sealed batches are probed and shipped again with their tokens, and the
    // unsealed tail is shipped for the first time.
    let mut ledgers = vec![burst.ledger.clone()];
    let mut crashes = Vec::new();
    let mut acked_spans = 20_000;
    for (kill_after_ms, graceful) in [(2_000, false), (2_650, false), (3_300, cfg!(unix))] {
        let config = run_config(&server, &load_bearer, 6);
        let (run_env, run_pool) = (env.clone(), pool(env));
        let running =
            tokio::spawn(async move { crucible_loadgen::run(&run_env, run_pool, &config).await });
        tokio::time::sleep(Duration::from_millis(kill_after_ms)).await;
        if graceful {
            #[cfg(unix)]
            assert!(
                terminate(&mut server),
                "SIGTERM under load must exit cleanly"
            );
        } else {
            server.child.kill().unwrap();
            let killed = server.child.wait().unwrap();
            assert!(!killed.success(), "the process was killed, not stopped");
        }
        let crashed = running.await.unwrap().unwrap();
        let acked = crashed.report.outcome.acked;
        assert!(
            acked > 0 && acked < 600,
            "the kill must fall inside the run: {:?}",
            crashed.report.outcome
        );
        let rows_at_kill = load_rows.counts().await["span_core_rows"];
        drop(server);

        let restart = Instant::now();
        server = rig.start(false);
        let ready_again = restart.elapsed();
        let (check, drain) = drained(&load_rows, &crashed.ledger).await;
        assert_eq!(
            (check.lost_spans, check.duplicate_spans),
            (0, 0),
            "after the stop at {kill_after_ms} ms: {check:?}"
        );
        acked_spans += check.acked_spans;
        let log = plain(&std::fs::read_to_string(&server.log).unwrap());
        let recovered = |field: &str| {
            let rest = log.split(&format!(" {field}=")).nth(1)?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        };
        crashes.push(json!({
            "stopped_after_ms": kill_after_ms,
            "stopped_with": if graceful { "SIGTERM" } else { "SIGKILL" },
            "acked_requests_before_stop": acked,
            "span_core_rows_at_stop": rows_at_kill,
            "ready_again_after_ms": millis(ready_again),
            "wal_last_lsn": recovered("last_lsn"),
            "checkpoint_watermark": recovered("watermark"),
            "sealed_batches_shipped_again": recovered("resealed"),
            "check": check,
            "drained_after_ms": millis(drain),
        }));
        ledgers.push(crashed.ledger);
    }
    // The shipper may still be landing frames that were durable but not acknowledged; wait for it to settle
    // before counting rows.
    let mut crash_counts = load_rows.counts().await;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let again = load_rows.counts().await;
        if again == crash_counts {
            break;
        }
        crash_counts = again;
    }
    let mut final_checks = Vec::new();
    for ledger in &ledgers {
        let check = verify(ledger, &load_rows).await.unwrap();
        assert_eq!(
            (check.lost_spans, check.duplicate_spans),
            (0, 0),
            "after every crash: {check:?}"
        );
        final_checks.push(check);
    }
    assert_no_duplicate_rows(&crash_counts, acked_spans, "after the crashes");
    // The traces of step 2 are long past the grace, so after the restart only the store can serve them.
    for trace in [&protobuf, &json_trace] {
        let api = api(&server);
        let (list, detail) = (api.list(), api.detail(trace));
        trace.assert_round_trip(&list, &detail, true, "after the restarts");
        assert_eq!(detail["meta"]["partial"], false, "{detail}");
    }

    // 5. The UI, when a Chrome binary is given.
    let ui = std::env::var("CRUCIBLE_E2E_CHROME")
        .ok()
        .map(|chrome| ui_check(&chrome, &rig, &api(&server), [&protobuf, &json_trace]));

    // 6. A graceful stop exits cleanly.
    #[cfg(unix)]
    let graceful = {
        let clean = terminate(&mut server);
        let log = std::fs::read_to_string(&server.log).unwrap();
        assert!(clean && log.contains("shutdown complete"), "{log}");
        true
    };
    #[cfg(not(unix))]
    let graceful = false;
    drop(server);

    let report = json!({
        "ready_after_ms": millis(ready_after),
        "freshness_ms": {
            "protobuf": {"ack_to_api_list": millis(protobuf_listed), "ack_to_clickhouse": millis(protobuf_stored)},
            "json": {"ack_to_api_list": millis(json_listed), "ack_to_clickhouse": millis(json_stored)},
        },
        "burst": {
            "acked_requests": burst.report.outcome.acked,
            "check": burst_check,
            "drained_after_ms": millis(burst_drain),
            "ack_latency_us": burst.report.latency_us,
            "counts": burst_counts,
        },
        "restarts": crashes,
        "after_all_restarts": {
            "acked_spans": acked_spans,
            "checks_per_run": final_checks,
            "counts": crash_counts,
        },
        "ui": ui,
        "graceful_shutdown_checked": graceful,
    });
    let pretty = serde_json::to_string_pretty(&report).unwrap();
    std::fs::write(rig.out.join("report.json"), &pretty).unwrap();
    eprintln!("{pretty}");
}

/// One HTTP/1.1 exchange that also answers the response's `Set-Cookie` values.
fn http_with_cookies(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<String>, Value) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&response[..end]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let cookies = head
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie")
                .then(|| value.trim().to_owned())
        })
        .collect();
    let body = serde_json::from_slice(&response[end + 4..]).unwrap_or(Value::Null);
    (status, cookies, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_in_owner_makes_a_key_that_ingests_and_reads() {
    let (Ok(clickhouse_url), Ok(postgres_url)) = (
        std::env::var("CRUCIBLE_E2E_CLICKHOUSE_URL"),
        std::env::var("CRUCIBLE_E2E_POSTGRES_URL"),
    ) else {
        eprintln!(
            "skipped: CRUCIBLE_E2E_CLICKHOUSE_URL and CRUCIBLE_E2E_POSTGRES_URL are not both set"
        );
        return;
    };
    let admin = Credentials {
        user: std::env::var("CRUCIBLE_E2E_CLICKHOUSE_USER")
            .unwrap_or_else(|_| "crucible_admin".to_owned()),
        password: std::env::var("CRUCIBLE_E2E_CLICKHOUSE_PASSWORD")
            .expect("CRUCIBLE_E2E_CLICKHOUSE_PASSWORD is set"),
    };
    let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap();
    let env = &prod.env;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();
    let out = std::env::var_os("CRUCIBLE_E2E_OUT")
        .map_or_else(|| dir.clone(), |o| PathBuf::from(o).join("control-plane"));
    std::fs::create_dir_all(&out).unwrap();
    // No keys file: with a control plane the keys come from Postgres.
    std::fs::write(dir.join("keys.toml"), "").unwrap();
    std::fs::write(dir.join("master-key"), format!("{MASTER_KEY}\n")).unwrap();
    std::fs::write(dir.join("clickhouse-password"), &admin.password).unwrap();
    let password = "e2e owner password 1";
    std::fs::write(dir.join("owner-password"), format!("{password}\n")).unwrap();
    let binary = std::env::var_os("CRUCIBLE_E2E_BIN")
        .map(PathBuf::from)
        .expect("CRUCIBLE_E2E_BIN is set");
    let mut rig = Rig {
        binary: binary.clone(),
        clickhouse_url,
        admin,
        dir: dir.clone(),
        out,
        starts: 0,
        postgres_url: Some(postgres_url.clone()),
    };
    // 1. Migrations run, the key snapshot loads, and the server becomes ready.
    let server = rig.start(true);

    // 2. The first owner, from the command line; a second one is refused.
    let email = format!("owner-{}@e2e.test", env.new_id());
    let init = |email: &str| {
        Command::new(&binary)
            .args(["admin", "init", "--org-name", "E2E", "--name", "Owner"])
            .args(["--email", email])
            .arg("--password-file")
            .arg(dir.join("owner-password"))
            .args(["--postgres-url", &postgres_url])
            .arg("--master-key-file")
            .arg(dir.join("master-key"))
            .env_remove("CRUCIBLE_POSTGRES_URL")
            .output()
            .unwrap()
    };
    let first = init(&email);
    assert!(
        first.status.success(),
        "crucible admin init failed (the control-plane path needs a fresh database): {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = init("other@e2e.test");
    assert!(!second.status.success(), "a second owner was created");
    let owner_email = email;

    // 3. Sign in with the prelogin CSRF header from the server's own origin.
    let login = json!({"email": owner_email, "password": password}).to_string();
    let json_headers = [
        ("content-type", "application/json"),
        ("origin", "http://e2e"),
    ];
    let (status, cookies, session) = http_with_cookies(
        server.api,
        "POST",
        "/api/v1/auth/login",
        &[
            json_headers[0],
            json_headers[1],
            ("x-crucible-csrf", "prelogin"),
        ],
        login.as_bytes(),
    );
    assert_eq!(status, 200, "{session}");
    let cookie = cookies
        .iter()
        .find_map(|c| c.strip_prefix("crucible_session="))
        .map(|rest| format!("crucible_session={}", rest.split(';').next().unwrap()))
        .expect("the session cookie");
    let csrf = session["csrf_token"].as_str().unwrap().to_owned();
    let as_owner = |method: &str, path: &str, body: Value| {
        let body = body.to_string();
        let mut headers = vec![("cookie", cookie.as_str())];
        if method != "GET" {
            headers.extend([
                json_headers[0],
                json_headers[1],
                ("x-crucible-csrf", csrf.as_str()),
            ]);
        }
        http_with_cookies(server.api, method, path, &headers, body.as_bytes())
    };

    // 4. A project and a key for it; the secret is shown once.
    let slug = format!("e2e-{}", &env.new_id().to_string()[24..]);
    let (status, _, project) = as_owner(
        "POST",
        "/api/v1/projects",
        json!({"name": "E2E", "slug": slug}),
    );
    assert_eq!(status, 201, "{project}");
    let project_id = project["id"].as_str().unwrap().to_owned();
    let keys = format!("/api/v1/projects/{project_id}/api-keys");
    let (status, _, created) = as_owner("POST", &keys, json!({"note": "e2e"}));
    assert_eq!(status, 201, "{created}");
    let bearer = format!("Bearer {}", created["secret_key"].as_str().unwrap());

    // 5. Ingest with the key, then read the trace back with the session.
    let trace = Trace::new(env, "e2e control-plane trace");
    let (status, body) = http(
        server.ingest,
        "POST",
        "/v1/traces",
        &[
            ("content-type", "application/json"),
            ("authorization", &bearer),
        ],
        trace.json().as_bytes(),
    )
    .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let micros = i64::try_from(now.as_micros()).unwrap();
    let (from, to) = (
        Timestamp::from_micros(micros - 3_600_000_000),
        Timestamp::from_micros(micros + 3_600_000_000),
    );
    let list = format!("/api/v1/projects/{project_id}/traces?from={from}&to={to}&limit=100");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, _, page) = as_owner("GET", &list, Value::Null);
        assert_eq!(status, 200, "{page}");
        let listed = page["data"]
            .as_array()
            .is_some_and(|d| d.iter().any(|t| t["trace_id"] == trace.trace_id.as_str()));
        if listed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the trace was not listed: {page}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // 6. Revoking the key stops it at once; signing out ends the session.
    let key_id = created["key"]["id"].as_str().unwrap();
    let etag = format!("\"{}\"", created["key"]["version"]);
    let (status, _, body) = {
        let mut headers = vec![
            ("cookie", cookie.as_str()),
            ("origin", "http://e2e"),
            ("x-crucible-csrf", csrf.as_str()),
        ];
        headers.push(("if-match", etag.as_str()));
        http_with_cookies(
            server.api,
            "DELETE",
            &format!("{keys}/{key_id}"),
            &headers,
            b"",
        )
    };
    assert_eq!(status, 204, "{body}");
    let (status, _) = http(
        server.ingest,
        "POST",
        "/v1/traces",
        &[
            ("content-type", "application/json"),
            ("authorization", &bearer),
        ],
        trace.json().as_bytes(),
    )
    .unwrap();
    assert_eq!(status, 401);
    let (status, _, _) = as_owner("POST", "/api/v1/auth/logout", Value::Null);
    assert_eq!(status, 204);
    let (status, _, _) = as_owner("GET", "/api/v1/auth/session", Value::Null);
    assert_eq!(status, 401);
    drop(server);
}
