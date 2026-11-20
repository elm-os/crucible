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
//! | `CRUCIBLE_E2E_POSTGRES_URL` | A Postgres 15 or newer, for example `postgres://crucible:e2e@localhost:5432/crucible`. When set, the control-plane path runs: the first owner signs in, creates a project and an API key, ingests with the key, reads the trace back with the session and builds a dashboard over it |
//! | `CRUCIBLE_E2E_S3_ENDPOINT` | An S3-compatible endpoint that the server and the test both reach, for example `http://localhost:7070`, with `CRUCIBLE_E2E_S3_BUCKET` (default `media`, which must exist), `CRUCIBLE_E2E_S3_ACCESS_KEY` and `CRUCIBLE_E2E_S3_SECRET_KEY`. When set, the control-plane path also uploads media through the compat media endpoint and reads them back; when unset, it checks that the endpoint answers `media_not_supported` |
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
use crucible_records::{SpanId, Timestamp, TraceId, Uuid};
use crucible_schema::ServiceIdentity;
use crucible_store_clickhouse::render::Statement;
use crucible_store_clickhouse::{ClickHouseStore, Credentials, StoreConfig};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use prost::Message as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
    /// The control plane, when the run has one.
    postgres_url: Option<String>,
    /// The object store for media, when the run has one.
    media: Option<Media>,
}

/// The object store of a run: an S3-compatible endpoint that both the server and the test reach, and a bucket
/// that exists.
#[derive(Clone)]
struct Media {
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
}

impl Media {
    /// The store the `CRUCIBLE_E2E_S3_*` variables name, if `CRUCIBLE_E2E_S3_ENDPOINT` is set.
    fn from_env() -> Option<Self> {
        let endpoint = std::env::var("CRUCIBLE_E2E_S3_ENDPOINT").ok()?;
        let var =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
        Some(Self {
            endpoint,
            bucket: var("CRUCIBLE_E2E_S3_BUCKET", "media"),
            access_key: var("CRUCIBLE_E2E_S3_ACCESS_KEY", "e2e"),
            secret_key: var("CRUCIBLE_E2E_S3_SECRET_KEY", "e2e-secret"),
        })
    }
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
        // With Postgres the keys live there; the keys file is the mode without it.
        match &self.postgres_url {
            Some(url) => command.args(["--postgres-url", url]),
            None => command.arg("--keys-file").arg(self.dir.join("keys.toml")),
        };
        if let Some(media) = &self.media {
            command
                .env("CRUCIBLE_OBJECT_STORE_ENDPOINT", &media.endpoint)
                .env("CRUCIBLE_OBJECT_STORE_BUCKET", &media.bucket)
                .env("CRUCIBLE_OBJECT_STORE_ACCESS_KEY", &media.access_key)
                .env("CRUCIBLE_OBJECT_STORE_SECRET_KEY", &media.secret_key);
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
        spread: Duration::ZERO,
        backdate: Duration::ZERO,
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
        postgres_url: None,
        media: None,
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

/// One HTTP/1.1 exchange with an absolute `http://` URL, such as a presigned one, whose host it sends: the
/// signature covers it.
fn http_url(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Vec<u8>) {
    use std::net::ToSocketAddrs as _;
    let rest = url.strip_prefix("http://").expect("an http URL");
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let mut stream = host
        .to_socket_addrs()
        .unwrap()
        .find_map(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok())
        .expect("the URL's host answers");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = String::from_utf8_lossy(&response[..end])
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    // A chunked answer is not expected from these servers for these small bodies.
    (status, response[end + 4..].to_vec())
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

/// The salt of the project with this slug in the control plane, opened with the seal subkey of [`MASTER_KEY`]
/// as the server opens it (contract 9.3); `None` when the project has no salt row.
async fn project_salt(env: &Env, postgres_url: &str, slug: &str) -> Option<[u8; 32]> {
    use crucible_control::Repos as _;
    let store = crucible_control_pg::PgControlStore::new(
        env,
        &crucible_control_pg::PgConfig::new(postgres_url),
    )
    .unwrap();
    let project = store
        .projects()
        .get_by_slug(slug)
        .await
        .unwrap()
        .expect("the project");
    let row = store.project_salts().get(project.id).await.unwrap();
    store.close();
    let master: Vec<u8> = MASTER_KEY
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let mut seal_key = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(None, &master)
        .expand(b"crucible/seal/v1", &mut seal_key)
        .unwrap();
    let sealer = crucible_auth::Sealer::new("master-0", seal_key);
    row.map(|row| {
        sealer
            .open_project_salt(&row)
            .expect("the salt opens with the server's master key")
    })
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
        media: Media::from_env(),
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
    // The default project has its salt from the start (contract 9.3).
    assert!(
        project_salt(env, &postgres_url, "default").await.is_some(),
        "admin init made the default project without its salt"
    );

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
    // The new project has its salt at once, sealed under the master key, before any frame needs it.
    let salt = project_salt(env, &postgres_url, &slug).await;
    assert!(salt.is_some(), "the project was created without its salt");
    assert_ne!(
        salt,
        project_salt(env, &postgres_url, "default").await,
        "two projects share a salt"
    );
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

    // 6. A key from the command line, as the Compose smoke test makes one, for the first project `default`.
    let made = Command::new(&binary)
        .args([
            "admin",
            "create-key",
            "--project",
            "default",
            "--scopes",
            "ingest,read",
        ])
        .args(["--postgres-url", &postgres_url])
        .arg("--master-key-file")
        .arg(dir.join("master-key"))
        .env_remove("CRUCIBLE_POSTGRES_URL")
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let printed = String::from_utf8(made.stdout).unwrap();
    let field = |name: &str| {
        printed
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = \"")))
            .map(|value| value.trim_end_matches('"').to_owned())
            .unwrap_or_else(|| panic!("no {name} in {printed}"))
    };
    let cli_bearer = format!("Bearer {}", field("secret_key"));
    let cli_trace = Trace::new(env, "e2e command-line key trace");
    // A key made outside the server reaches it through the key feed, within 5 s.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, body) = http(
            server.ingest,
            "POST",
            "/v1/traces",
            &[
                ("content-type", "application/json"),
                ("authorization", &cli_bearer),
            ],
            cli_trace.json().as_bytes(),
        )
        .unwrap();
        if status == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ingest answered {status} for 20 s with the command-line key: {}",
            String::from_utf8_lossy(&body)
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let cli_api = Api {
        addr: server.api,
        project: field("project_id"),
        bearer: cli_bearer.clone(),
        pair: String::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cli_api.lists(&cli_trace) {
        assert!(
            Instant::now() < deadline,
            "the command-line key's trace was not listed"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // The MCP server's tools read: a read-only key reads the trace of step 5 through `/mcp`, and an
    // ingest-only key is refused there.
    let scoped = |scopes: Value| {
        let (status, _, made) = as_owner("POST", &keys, json!({"note": "mcp", "scopes": scopes}));
        assert_eq!(status, 201, "{made}");
        format!("Bearer {}", made["secret_key"].as_str().unwrap())
    };
    let reader = Api {
        addr: server.api,
        project: project_id.clone(),
        bearer: scoped(json!(["read"])),
        pair: String::new(),
    };
    let result = reader.mcp("get_trace", json!({ "trace_id": trace.trace_id }));
    assert_eq!(result["isError"], false, "{result}");
    assert!(
        result["structuredContent"]["spans"]
            .as_array()
            .is_some_and(|spans| !spans.is_empty()),
        "{result}"
    );
    let ingest_only = scoped(json!(["ingest"]));
    let (status, body) = http(
        server.api,
        "POST",
        "/mcp",
        &[
            ("authorization", &ingest_only),
            ("content-type", "application/json"),
        ],
        b"{}",
    )
    .unwrap();
    assert_eq!(status, 401, "{}", String::from_utf8_lossy(&body));

    // 7. An export of the trace of step 5: created once under its Idempotency-Key, written by the export job,
    //    downloaded whole with its checksum, followed through the MCP server, and written again by
    //    `crucible export` straight from the stores.
    let exports = format!("/api/v1/projects/{project_id}/exports");
    let request = json!({
        "format": "crucible-jsonl",
        "from": from.to_string(),
        "to": to.to_string(),
        "trace_ids": [trace.trace_id],
    })
    .to_string();
    let create = || {
        http_with_cookies(
            server.api,
            "POST",
            &exports,
            &[
                ("cookie", cookie.as_str()),
                json_headers[0],
                json_headers[1],
                ("x-crucible-csrf", csrf.as_str()),
                ("idempotency-key", "e2e-export-1"),
            ],
            request.as_bytes(),
        )
    };
    let (status, _, created_export) = create();
    assert_eq!(status, 202, "{created_export}");
    let export_id = created_export["id"].as_str().unwrap().to_owned();
    let (status, _, replayed) = create();
    assert_eq!((status, &replayed["id"]), (202, &created_export["id"]));
    let deadline = Instant::now() + Duration::from_secs(120);
    let ready = loop {
        let (status, _, read) = as_owner("GET", &format!("{exports}/{export_id}"), Value::Null);
        assert_eq!(status, 200, "{read}");
        if read["state"] == "ready" {
            break read;
        }
        assert_ne!(read["state"], "failed", "{read}");
        assert!(
            Instant::now() < deadline,
            "the export did not finish: {read}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    let (status, file) = http(
        server.api,
        "GET",
        ready["download_path"].as_str().unwrap(),
        &[("cookie", cookie.as_str())],
        b"",
    )
    .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&file));
    let checksum: String = Sha256::digest(&file)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(ready["checksum"], checksum.as_str());
    assert_eq!(ready["bytes"], file.len());
    let lines: Vec<Value> = String::from_utf8(file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "the root and its child");
    assert_eq!(ready["spans"], 2);
    for line in &lines {
        assert_eq!(line["schema"], "crucible.span/1");
        assert_eq!(line["span"]["trace_id"], trace.trace_id.as_str());
        assert_eq!(line["span"]["project_id"], project_id.as_str());
    }
    let followed = reader.mcp("get_export", json!({ "id": export_id }));
    assert_eq!(
        followed["structuredContent"]["state"], "ready",
        "{followed}"
    );
    let written = Command::new(&binary)
        .args(["export", "--format", "otlp-json", "--project", &project_id])
        .args(["--from", &from.to_string(), "--to", &to.to_string()])
        .args(["--trace-id", &trace.trace_id])
        .args(["--postgres-url", &postgres_url])
        .args(["--clickhouse-url", &rig.clickhouse_url])
        .args(["--clickhouse-user", &rig.admin.user])
        .arg("--clickhouse-password-file")
        .arg(dir.join("clickhouse-password"))
        .arg("--master-key-file")
        .arg(dir.join("master-key"))
        .env_remove("CRUCIBLE_POSTGRES_URL")
        .output()
        .unwrap();
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    let documents: Vec<Value> = String::from_utf8(written.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(documents.len(), 2);
    for document in &documents {
        let spans = &document["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(spans.as_array().map(Vec::len), Some(1));
        assert_eq!(spans[0]["traceId"], trace.trace_id.as_str());
    }

    // 8. A dashboard: created with a widget, changed under its version, viewed by running the query its widget
    //    holds over a window, and a built-in dashboard duplicated into rows.
    let dashboards = format!("/api/v1/projects/{project_id}/dashboards");
    let spans_query =
        json!({"source": "spans", "measures": [{"measure": "count", "aggregation": "sum"}]});
    let (status, _, made) = as_owner(
        "POST",
        &dashboards,
        json!({"name": "E2E", "widgets": [{
            "query": spans_query,
            "display": {"kind": "stat", "title": "Spans"},
            "layout": {"x": 0, "y": 0, "w": 3, "h": 2},
        }]}),
    );
    assert_eq!(status, 201, "{made}");
    let one = format!("{dashboards}/{}", made["id"].as_str().unwrap());
    let versioned = |method: &str, path: &str, body: &Value, version: &str| {
        let body = body.to_string();
        let headers = [
            ("cookie", cookie.as_str()),
            json_headers[0],
            json_headers[1],
            ("x-crucible-csrf", csrf.as_str()),
            ("if-match", version),
        ];
        http_with_cookies(server.api, method, path, &headers, body.as_bytes())
    };
    let change = json!({"name": "E2E health", "widgets": [
        {
            "id": made["widgets"][0]["id"],
            "query": spans_query,
            "display": {"kind": "stat", "title": "Spans"},
            "layout": {"x": 3, "y": 1, "w": 4, "h": 2},
        },
        {
            "display": {"kind": "markdown", "title": "Notes", "text": "Read **me**"},
            "layout": {"x": 0, "y": 0, "w": 3, "h": 1},
        },
    ]});
    let (status, _, stale) = versioned("PATCH", &one, &change, "\"9\"");
    assert_eq!(status, 412, "{stale}");
    let (status, _, changed) = versioned("PATCH", &one, &change, "\"1\"");
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["version"], 2);
    let (status, _, viewed) = as_owner("GET", &one, Value::Null);
    assert_eq!(status, 200, "{viewed}");
    assert_eq!(viewed["name"], "E2E health");
    // Reading order: the note on row 0, then the stat that moved to row 1.
    assert_eq!(viewed["widgets"][0]["display"]["kind"], "markdown");
    let stat = &viewed["widgets"][1];
    assert_eq!(stat["layout"], json!({"x": 3, "y": 1, "w": 4, "h": 2}));
    let mut query = stat["query"].clone();
    query["from"] = json!(from.to_string());
    query["to"] = json!(to.to_string());
    let metrics = format!("/api/v1/projects/{project_id}/metrics/query");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, _, answer) = as_owner("POST", &metrics, query.clone());
        assert_eq!(status, 200, "{answer}");
        let spans: f64 = answer["series"][0]["points"]
            .as_array()
            .map(|points| points.iter().filter_map(|p| p["value"].as_f64()).sum())
            .unwrap_or_default();
        if spans >= 2.0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the widget's query did not count the trace of step 5: {answer}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let (status, _, copy) = as_owner(
        "POST",
        &format!("{dashboards}/overview/duplicate"),
        json!({}),
    );
    assert_eq!(status, 201, "{copy}");
    assert_eq!(copy["name"], "Agent overview (copy)");
    let (status, _, listed) = as_owner("GET", &dashboards, Value::Null);
    assert_eq!(status, 200, "{listed}");
    let builtin: Vec<&Value> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d.get("builtin_key").is_some())
        .collect();
    assert_eq!(listed["data"].as_array().unwrap().len(), builtin.len() + 2);

    // 9. Media through the compat media endpoint on the ingest listener, with the key pair, as the dialect's SDK
    //    sends them. With an object store (`CRUCIBLE_E2E_S3_ENDPOINT`), the upload is asked for, put to the
    //    presigned URL with its signed headers, reported, read back with a presigned download, and listed with its
    //    use; without one, the endpoint refuses with the setting that turns media on.
    {
        use base64::Engine as _;
        use crucible_compat::media::{
            MEDIA_ROUTES, MediaReply, UploadReply, UploadRequest, media_id, report_request,
            upload_request,
        };
        let standard = base64::engine::general_purpose::STANDARD;
        let pair = standard.encode(format!(
            "{}:{}",
            created["key"]["public_key"].as_str().unwrap(),
            created["secret_key"].as_str().unwrap()
        ));
        let pair = format!("Basic {pair}");
        let collection = MEDIA_ROUTES[0].collection;
        let bytes = format!("e2e media of {}", trace.trace_id).into_bytes();
        let sha256: [u8; 32] = Sha256::digest(&bytes).into();
        let ask = upload_request(&UploadRequest {
            trace_id: TraceId::parse(&trace.trace_id).unwrap(),
            span_id: Some(SpanId::parse(&trace.root_id).unwrap()),
            content_type: "image/png".to_owned(),
            len: u64::try_from(bytes.len()).unwrap(),
            sha256,
            field: "input".to_owned(),
        });
        let json_pair = [
            ("content-type", "application/json"),
            ("authorization", pair.as_str()),
        ];
        let (status, answer) = http(server.ingest, "POST", collection, &json_pair, &ask).unwrap();
        let text = String::from_utf8_lossy(&answer).into_owned();
        match &rig.media {
            None => {
                let problem: Value = serde_json::from_slice(&answer).unwrap();
                assert_eq!(status, 501, "{text}");
                assert_eq!(problem["code"], "media_not_supported", "{text}");
                assert!(text.contains("object_store.endpoint"), "{text}");
            }
            Some(_) => {
                assert_eq!(status, 201, "{text}");
                let reply: UploadReply = serde_json::from_slice(&answer).unwrap();
                assert_eq!(reply.media_id, media_id(&sha256));
                let checksum = standard.encode(sha256);
                let (status, body) = http_url(
                    "PUT",
                    reply.upload_url.as_deref().unwrap(),
                    &[
                        ("content-type", "image/png"),
                        ("x-amz-checksum-sha256", checksum.as_str()),
                    ],
                    &bytes,
                );
                assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
                let one = format!("{collection}/{}", reply.media_id);
                let (status, body) = http(
                    server.ingest,
                    "PATCH",
                    &one,
                    &json_pair,
                    &report_request(200),
                )
                .unwrap();
                assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
                let (status, read) = http(server.api, "GET", &one, &json_pair[1..], b"").unwrap();
                assert_eq!(status, 200, "{}", String::from_utf8_lossy(&read));
                let read: MediaReply = serde_json::from_slice(&read).unwrap();
                assert_eq!(
                    (read.content_type.as_str(), read.len),
                    ("image/png", u64::try_from(bytes.len()).unwrap())
                );
                assert!(read.uploaded_at.is_some(), "{read:?}");
                let (status, downloaded) = http_url("GET", &read.url, &[], b"");
                assert_eq!((status, downloaded), (200, bytes.clone()));
                // The same bytes again need no upload.
                let (status, again) =
                    http(server.ingest, "POST", collection, &json_pair, &ask).unwrap();
                assert_eq!(status, 201, "{}", String::from_utf8_lossy(&again));
                let again: UploadReply = serde_json::from_slice(&again).unwrap();
                assert_eq!((again.media_id, again.upload_url), (reply.media_id, None));
                // The session reads where the media appear, and that media are on.
                let uses = format!(
                    "/api/v1/projects/{project_id}/media?trace_id={}&span_id={}",
                    trace.trace_id, trace.root_id
                );
                let (status, _, listed) = as_owner("GET", &uses, Value::Null);
                assert_eq!(status, 200, "{listed}");
                assert_eq!(listed["data"][0]["field"], "input", "{listed}");
                assert_eq!(listed["data"][0]["media"]["state"], "uploaded", "{listed}");
                let (status, _, info) = as_owner("GET", "/api/v1/auth/session", Value::Null);
                assert_eq!(status, 200, "{info}");
                assert_eq!(info["features"]["media"]["enabled"], true, "{info}");
            }
        }
    }

    // 10. Revoking the key stops it at once; signing out ends the session.
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
