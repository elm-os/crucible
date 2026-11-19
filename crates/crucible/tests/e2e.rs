//! Milestone 1 end to end, on real infrastructure: the `crucible serve` binary in front of a real ClickHouse.
//!
//! A span sent over OTLP goes through ingest, the WAL, the shipper and ClickHouse, and comes back through the
//! REST API and, when a Chrome binary is given, the UI's traces table. Four traces take the four ways in:
//! OTLP/HTTP protobuf, OTLP/HTTP JSON, OTLP/gRPC with gzip, and a compat ingest route with gzip and the key pair
//! as Basic auth. A load burst and a `kill -9` in the middle of three further runs must lose no acknowledged
//! span and duplicate no row. At startup the recent-span index is sized from the memory the server may use
//! (about 200 MiB in a container limited to 2 GiB) and the memory budgets fit without a warning.
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
//!
//! `docs/development.md` ("The end-to-end test") has the commands.

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
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use prost::Message as _;
use serde_json::{Value, json};
use tonic::codec::CompressionEncoding;

const MASTER_KEY: &str = "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e";
const SPANS_PER_REQUEST: u32 = 100;
/// How long the shipper gets to land everything that was acknowledged.
const DRAIN: Duration = Duration::from_secs(120);

fn secret(fill: char) -> String {
    format!("csk_{}", fill.to_string().repeat(43))
}

fn public(fill: char) -> String {
    format!("cpk_{}", fill.to_string().repeat(43))
}

/// `Authorization: Basic` with the key pair of `fill`.
fn basic(fill: char) -> String {
    use base64::Engine as _;
    let pair = format!("{}:{}", public(fill), secret(fill));
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
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
    grpc: SocketAddr,
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
}

impl Rig {
    /// Starts the binary on fresh ports over the rig's data directory and waits until `/readyz` answers 200.
    fn start(&mut self, migrate: bool) -> Server {
        self.starts += 1;
        let log = self.out.join(format!("serve-{}.log", self.starts));
        let (api, ingest, grpc, metrics) = (free_addr(), free_addr(), free_addr(), free_addr());
        let mut command = Command::new(&self.binary);
        command
            .arg("serve")
            .args(["--listen", &api.to_string()])
            .args(["--ingest-listen", &ingest.to_string()])
            .args(["--grpc-listen", &grpc.to_string()])
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
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        if migrate {
            command.arg("--migrate");
        }
        let mut server = Server {
            child: command.spawn().unwrap(),
            api,
            ingest,
            grpc,
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
    /// The key pair as Basic auth.
    pair: String,
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

    /// One `tools/call` at `/mcp` in the stateless protocol era, which answers with plain JSON: the tool result.
    fn mcp(&self, tool: &str, arguments: Value) -> Value {
        const VERSION: &str = "2026-07-28";
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": VERSION,
            "io.modelcontextprotocol/clientInfo": { "name": "crucible-e2e", "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let params = json!({ "name": tool, "arguments": arguments, "_meta": meta });
        let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
        let headers = [
            ("authorization", self.bearer.as_str()),
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", VERSION),
            ("mcp-method", "tools/call"),
            ("mcp-name", tool),
        ];
        let (status, body) = http(
            self.addr,
            "POST",
            "/mcp",
            &headers,
            call.to_string().as_bytes(),
        )
        .unwrap_or_else(|e| panic!("POST /mcp {tool}: {e}"));
        let reply: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        assert_eq!(status, 200, "{tool}: {reply}");
        reply["result"].clone()
    }
}

/// How a trace is sent.
#[derive(Clone, Copy, Debug)]
enum Via {
    /// OTLP/HTTP protobuf on the ingest listener.
    Protobuf,
    /// OTLP/HTTP JSON on the main listener.
    Json,
    /// OTLP/gRPC, gzip-compressed, on the gRPC listener.
    Grpc,
    /// The first compat ingest route that names its SDK in headers: gzip-compressed protobuf with the key pair
    /// as Basic auth, as that route's SDKs send it, on the ingest listener.
    Compat,
}

/// Sends the trace with the API's key and returns how long after the acknowledgement the API listed it, and
/// how long after it ClickHouse held both spans.
async fn send_and_time(
    server: &Server,
    api: &Api,
    stored: &Stored,
    trace: &Trace,
    via: Via,
) -> (Duration, Duration) {
    let json = trace.json();
    let protobuf = crucible_otlp::transcode_json(json.as_bytes()).unwrap();
    let acked = match via {
        Via::Grpc => {
            let channel =
                tonic::transport::Endpoint::from_shared(format!("http://{}", server.grpc))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
            let mut client =
                TraceServiceClient::new(channel).send_compressed(CompressionEncoding::Gzip);
            let message = ExportTraceServiceRequest::decode(protobuf.as_slice()).unwrap();
            let mut request = tonic::Request::new(message);
            request
                .metadata_mut()
                .insert("authorization", api.bearer.parse().unwrap());
            let response = client.export(request).await.unwrap().into_inner();
            assert_eq!(response.partial_success, None);
            Instant::now()
        }
        Via::Protobuf | Via::Json | Via::Compat => {
            let route = crucible_compat::routes::INGEST_ROUTES
                .iter()
                .find(|route| route.sdk_headers.is_some())
                .unwrap();
            let (addr, path, content_type, body) = match via {
                Via::Json => (
                    server.api,
                    "/v1/traces",
                    "application/json",
                    json.into_bytes(),
                ),
                Via::Compat => (
                    server.ingest,
                    route.path,
                    "application/x-protobuf",
                    gzip(&protobuf),
                ),
                _ => (
                    server.ingest,
                    "/v1/traces",
                    "application/x-protobuf",
                    protobuf,
                ),
            };
            let mut headers = vec![
                ("authorization", api.bearer.as_str()),
                ("content-type", content_type),
            ];
            if let (Via::Compat, Some(sdk)) = (via, route.sdk_headers) {
                headers[0].1 = api.pair.as_str();
                headers.extend([
                    ("content-encoding", "gzip"),
                    (sdk.name, "e2e-sdk"),
                    (sdk.version, "1.0.0"),
                ]);
            }
            let (status, response) = http(addr, "POST", path, &headers, &body).unwrap();
            let acked = Instant::now();
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&response));
            // A full success rejects nothing: an empty protobuf message, or an empty JSON object.
            assert!(response.is_empty() || response == b"{}", "{response:?}");
            acked
        }
    };

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

/// The number a log line gives for ` field=`, from its first occurrence.
fn log_number(log: &str, field: &str) -> Option<u64> {
    let rest = log.split(&format!(" {field}=")).nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
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
        gzip_threads: 1,
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
        traces_per_request: 1,
        rootless: false,
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
fn ui_check(chrome: &str, rig: &Rig, api: &Api, traces: [&Trace; 4]) -> Value {
    let (status, _) = http(api.addr, "GET", "/traces/", &[], b"").unwrap();
    assert_eq!(
        status, 200,
        "the binary serves no UI; build it with --features embed-ui for the UI check"
    );
    let screenshot = rig.out.join("traces-table.png");
    let node = std::env::var("CRUCIBLE_E2E_NODE").unwrap_or_else(|_| "node".to_owned());
    let mut command = Command::new(node);
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/e2e/ui-traces.mjs"))
        .args(["--chrome", chrome])
        .args(["--url", &format!("http://{}", api.addr)])
        .args(["--project", &api.project])
        .args(["--key", api.bearer.trim_start_matches("Bearer ")]);
    for trace in traces {
        command.args(["--expect", &trace.name]);
    }
    let output = command
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
    assert_eq!(rows.len(), traces.len(), "{seen}");
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
            "[[project_key]]\nproject_id = \"{traces_project}\"\npublic_key = \"{}\"\nsecret = \"{}\"\n\n\
             [[project_key]]\nproject_id = \"{load_project}\"\npublic_key = \"{}\"\nsecret = \"{}\"\n",
            public('a'),
            secret('a'),
            public('b'),
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
                idle_timeout: None,
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
    };
    let load_bearer = format!("Bearer {}", secret('b'));

    // 1. Start with migrations; the startup checks of contract 5.8 pass before `/readyz` answers 200.
    let boot = Instant::now();
    let mut server = rig.start(true);
    let ready_after = boot.elapsed();
    // The memory-derived defaults follow the memory the server may use: in a container limited to 2 GiB the
    // recent-span index is bounded near 200 MiB, and the memory budgets fit without a warning.
    let boot_log = plain(&std::fs::read_to_string(&server.log).unwrap());
    let recent_max_bytes =
        log_number(&boot_log, "recent_max_bytes").expect("the serving line gives the recent bound");
    let memory_limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok());
    const TWO_GIB: u64 = 2 << 30;
    match memory_limit {
        Some(TWO_GIB) => assert!(
            (190 << 20..=210 << 20).contains(&recent_max_bytes),
            "in a container limited to 2 GiB the recent bound is {recent_max_bytes} bytes"
        ),
        Some(limit) => assert!(recent_max_bytes <= limit / 10, "{recent_max_bytes}"),
        None => assert!(recent_max_bytes <= 1 << 30, "{recent_max_bytes}"),
    }
    let budget_warning = boot_log.contains("the memory budgets add up to");
    assert!(!budget_warning, "{boot_log}");
    let api = |server: &Server| Api {
        addr: server.api,
        project: traces_project.clone(),
        bearer: format!("Bearer {}", secret('a')),
        pair: basic('a'),
    };

    // 2. One trace each as protobuf on the ingest listener, as JSON on the main listener, over gRPC, and on a
    // compat ingest route with the key pair.
    let protobuf = Trace::new(env, "e2e protobuf trace");
    let json_trace = Trace::new(env, "e2e json trace");
    let grpc_trace = Trace::new(env, "e2e grpc trace");
    let compat_trace = Trace::new(env, "e2e compat route trace");
    let traces = [&protobuf, &json_trace, &grpc_trace, &compat_trace];
    let mut freshness = Vec::new();
    for (trace, via) in traces
        .into_iter()
        .zip([Via::Protobuf, Via::Json, Via::Grpc, Via::Compat])
    {
        let (listed, stored) = send_and_time(&server, &api(&server), &trace_rows, trace, via).await;
        freshness.push((
            format!("{via:?}").to_lowercase(),
            json!({"ack_to_api_list": millis(listed), "ack_to_clickhouse": millis(stored)}),
        ));
    }
    for trace in traces {
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
    for trace in traces {
        let api = api(&server);
        let (list, detail) = (api.list(), api.detail(trace));
        trace.assert_round_trip(&list, &detail, true, "from the store");
        for meta in [&list["meta"], &detail["meta"]] {
            assert_eq!(meta["partial"], false, "{meta}");
            assert_eq!(meta["consistency"], "strong", "{meta}");
        }
    }
    // The MCP server reads the same trace through `tools/call`, with the previews of the store, for its own
    // project only.
    let trace_id = json!({ "trace_id": protobuf.trace_id });
    let result = api(&server).mcp("get_trace", trace_id.clone());
    assert_eq!(result["isError"], false, "{result}");
    let spans = &result["structuredContent"]["spans"];
    assert_eq!(spans[0]["span_id"], protobuf.root_id.as_str(), "{result}");
    assert_eq!(spans[1]["status_code"], "error", "{result}");
    assert!(spans[0]["input_preview"].is_string(), "{result}");
    // Not found, or an empty trace that says it may lag, like the REST read of step 2.
    let foreign = other.mcp("get_trace", trace_id);
    let text = foreign["content"][0]["text"].as_str().unwrap_or_default();
    let hidden = text.contains("\"code\":\"not_found\"")
        || foreign["structuredContent"]["spans"] == json!([]);
    assert!(hidden, "{foreign}");

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
    for trace in traces {
        let api = api(&server);
        let (list, detail) = (api.list(), api.detail(trace));
        trace.assert_round_trip(&list, &detail, true, "after the restarts");
        assert_eq!(detail["meta"]["partial"], false, "{detail}");
    }

    // 5. The UI, when a Chrome binary is given.
    let ui = std::env::var("CRUCIBLE_E2E_CHROME")
        .ok()
        .map(|chrome| ui_check(&chrome, &rig, &api(&server), traces));

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
        "memory": {
            "container_limit_bytes": memory_limit,
            "recent_max_bytes": recent_max_bytes,
            "budget_warning": budget_warning,
        },
        "freshness_ms": freshness.into_iter().collect::<serde_json::Map<_, _>>(),
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
