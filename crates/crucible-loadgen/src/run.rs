//! The open-loop runner.
//!
//! Request `i` is due at `start + i * 1e9 / rate` nanoseconds, computed from `i` alone: the schedule never
//! looks at responses. Each of `concurrency` workers owns one HTTP/1.1 connection, which bounds the requests
//! in flight. A free worker takes the lowest request index nobody has taken, prepares its body (a pool copy,
//! gzipped in the gzip lane), waits until the request is released and sends it. When every worker is busy,
//! released requests wait for a free one, and that wait shows up in their latency, because latency is
//! measured from the intended send time, not from the moment the request left. Retries are off: a failed
//! request is counted and never resent, and its connection is replaced.
//!
//! One pacer task releases the requests in order at their due times. The production timer behind the `Env`
//! clock has millisecond resolution, which would let requests leave up to two milliseconds late, so the
//! pacer sleeps on the timer only until [`SPIN_WINDOW`] before a due time and then yields in a loop,
//! reading the clock, until the time comes. It keeps about one core busy while the schedule runs. A worker
//! waiting for request `i` waits on slot `i % concurrency`; no two waiting requests share a slot, because at
//! most `concurrency` requests are taken but not yet released. The report keeps the remaining error visible:
//! `send_lag_us` is the delay from intended to actual send, `service_time_us` runs from actual send to
//! response, and `latency_us` (from the intended send time) contains both.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Env, Instant, JoinHandle, NetError};
use flate2::write::DeflateEncoder;
use flate2::{Compression, Crc};
use hdrhistogram::{CreationError, Histogram};
use http::header::{CONTENT_ENCODING, CONTENT_TYPE, HOST};
use http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::{self, SendRequest};
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceResponse;
use prost::Message;
use serde::Serialize;
use tokio::sync::Notify;

use crate::ledger::AckLedger;
use crate::pool::Pool;

/// The OTLP/HTTP traces path (contract 12.1).
pub const TRACES_PATH: &str = "/v1/traces";
/// The `schema` value of [`RunReport`].
pub const RUN_SCHEMA: &str = "crucible-loadgen.run.v1";
/// The largest schedule a run accepts, in requests.
pub const MAX_REQUESTS: u64 = 1 << 32;
/// The pacer sleeps on the timer until this long before a due time and spins from there. It exceeds the
/// production timer's lateness (up to about 2.5 ms in the accuracy baseline of `docs/testing/perf.md`).
pub const SPIN_WINDOW: Duration = Duration::from_millis(3);
/// Histograms track values up to one hour, in microseconds; larger values are recorded as one hour.
const HISTOGRAM_MAX_US: u64 = 3_600_000_000;

/// What to send, where and how fast.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// The OTLP/HTTP listener.
    pub target: SocketAddr,
    /// The `Host` header, normally the target as the user wrote it.
    pub authority: String,
    /// Requests per second.
    pub rate: u64,
    /// The schedule holds `rate * duration` requests.
    pub duration: Duration,
    /// Connections, and so the most requests in flight.
    pub concurrency: usize,
    /// Send `Content-Encoding: gzip` bodies.
    pub gzip: bool,
    /// Deadline for connecting and for each request.
    pub timeout: Duration,
    /// Extra headers on every request, such as `Authorization`.
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

/// Why a run could not start or finish.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The configuration cannot be run.
    #[error("invalid run configuration: {0}")]
    Config(&'static str),
    /// A connection could not be opened before the schedule started.
    #[error("connecting to the target: {0}")]
    Connect(#[from] ConnectError),
    /// A histogram could not be created.
    #[error("histogram: {0}")]
    Histogram(#[from] CreationError),
    /// Compressing a body failed.
    #[error("gzip: {0}")]
    Gzip(#[from] std::io::Error),
    /// A worker task ended without a result.
    #[error("a worker task was cancelled")]
    Cancelled,
}

/// Why a connection could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The TCP connection failed.
    #[error(transparent)]
    Net(#[from] NetError),
    /// The HTTP/1.1 handshake failed.
    #[error("http handshake: {0}")]
    Handshake(#[from] hyper::Error),
}

/// The result of a run: the report and, for a later zero-loss check, the ledger of acknowledged requests.
#[derive(Debug)]
pub struct RunOutcome {
    /// The JSON report.
    pub report: RunReport,
    /// Which requests were acknowledged.
    pub ledger: AckLedger,
}

/// The JSON report of a run (schema [`RUN_SCHEMA`], documented in `docs/testing/perf.md`).
#[derive(Clone, Debug, Serialize)]
pub struct RunReport {
    /// Always [`RUN_SCHEMA`].
    pub schema: &'static str,
    /// The run id carried by every span.
    pub run_id: String,
    /// The content seed.
    pub seed: u64,
    /// The target as given.
    pub target: String,
    /// What each request looked like.
    pub lane: Lane,
    /// What was asked for.
    pub schedule: Schedule,
    /// What happened.
    pub outcome: Outcome,
    /// Intended send time to complete response, over 2xx responses.
    pub latency_us: Summary,
    /// Actual send time to complete response, over 2xx responses.
    pub service_time_us: Summary,
    /// Intended to actual send time, over every sent request.
    pub send_lag_us: Summary,
    /// Requests not acknowledged, as half-open `[start, end)` index ranges. Span `seq` belongs to request
    /// `seq / spans_per_request`; every span of every other request must be readable afterwards.
    pub not_acked_requests: Vec<[u64; 2]>,
    /// The generator's own CPU time during the run, when the binary could measure it.
    pub generator_cpu: Option<GeneratorCpu>,
}

/// The shape of the requests.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Lane {
    /// Encoded bytes per span.
    pub span_bytes: usize,
    /// Spans per request.
    pub spans_per_request: u32,
    /// Whether bodies were gzipped.
    pub gzip: bool,
    /// Distinct pre-encoded templates.
    pub templates: usize,
    /// Mean protobuf body size of sent requests, before compression.
    pub request_bytes_mean: f64,
    /// Mean body size on the wire.
    pub wire_bytes_mean: f64,
}

/// The requested schedule.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Schedule {
    /// Requests per second.
    pub target_rps: u64,
    /// Schedule length in seconds.
    pub duration_s: f64,
    /// Requests in the schedule.
    pub requests: u64,
    /// Connections (the in-flight bound).
    pub concurrency: usize,
    /// Per-request deadline.
    pub timeout_ms: u64,
}

/// What the run achieved.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    /// Requests written to a connection.
    pub sent: u64,
    /// Requests that got an HTTP response.
    pub responses: u64,
    /// Requests acknowledged without rejected spans.
    pub acked: u64,
    /// Spans of acknowledged requests.
    pub acked_spans: u64,
    /// 2xx responses that reported rejected spans.
    pub partial_success_requests: u64,
    /// Spans those responses rejected.
    pub rejected_spans: u64,
    /// Responses by HTTP status code.
    pub status_counts: BTreeMap<u16, u64>,
    /// Requests that got no usable response.
    pub errors: ErrorCounts,
    /// Sent requests per second over the send window.
    pub achieved_rps: f64,
    /// Acknowledged requests per second over the elapsed time.
    pub acked_rps: f64,
    /// Acknowledged spans per second over the elapsed time.
    pub acked_spans_per_s: f64,
    /// From the schedule start to the last send, plus one send interval, so a generator exactly on schedule
    /// reports the target rate.
    pub send_window_s: f64,
    /// From the schedule start to the last response or failure.
    pub elapsed_s: f64,
}

/// Failed requests by cause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ErrorCounts {
    /// No connection could be opened, so the request was not sent.
    pub connect: u64,
    /// No complete response within the timeout.
    pub timeout: u64,
    /// The connection failed before a complete response.
    pub transport: u64,
    /// A 2xx response whose body is not an `ExportTraceServiceResponse`.
    pub bad_response: u64,
}

/// Percentiles of a histogram, in its unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Recorded values.
    pub count: u64,
    /// Median.
    pub p50: u64,
    /// 90th percentile.
    pub p90: u64,
    /// 99th percentile.
    pub p99: u64,
    /// 99.9th percentile.
    pub p99_9: u64,
    /// Largest value.
    pub max: u64,
}

impl Summary {
    fn of(histogram: &Histogram<u64>) -> Self {
        if histogram.is_empty() {
            return Self::default();
        }
        Self {
            count: histogram.len(),
            p50: histogram.value_at_quantile(0.5),
            p90: histogram.value_at_quantile(0.9),
            p99: histogram.value_at_quantile(0.99),
            p99_9: histogram.value_at_quantile(0.999),
            max: histogram.max(),
        }
    }
}

/// The generator's CPU time during the run.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct GeneratorCpu {
    /// User CPU seconds.
    pub user_s: f64,
    /// System CPU seconds.
    pub system_s: f64,
    /// User plus system CPU microseconds per sent request.
    pub us_per_request: f64,
}

/// The number of requests in a schedule of `rate` requests per second for `duration`, rounded down.
pub fn scheduled_requests(rate: u64, duration: Duration) -> u64 {
    let requests = u128::from(rate) * duration.as_nanos() / 1_000_000_000;
    u64::try_from(requests).unwrap_or(u64::MAX)
}

/// The offset of request `index` from the schedule start, in nanoseconds: `index * 1e9 / rate`, rounded
/// down. A zero rate is treated as one request per second.
pub fn offset_nanos(index: u64, rate: u64) -> u64 {
    let nanos = u128::from(index) * 1_000_000_000 / u128::from(rate.max(1));
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// Resolves `host:port` (or a literal socket address) to the address to connect to.
pub async fn resolve(env: &Env, target: &str) -> Result<SocketAddr, ResolveError> {
    if let Ok(addr) = target.parse() {
        return Ok(addr);
    }
    let (host, port) = target.rsplit_once(':').ok_or(ResolveError::Syntax)?;
    let port = port.parse().map_err(|_| ResolveError::Syntax)?;
    let ip = env
        .net
        .resolve(host.trim_start_matches('[').trim_end_matches(']'))
        .await?
        .into_iter()
        .next()
        .ok_or(ResolveError::NoAddress)?;
    Ok(SocketAddr::new(ip, port))
}

/// Why a target could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The target is not `host:port`.
    #[error("the target must be host:port")]
    Syntax,
    /// The host has no address.
    #[error("the host has no address")]
    NoAddress,
    /// The resolver failed.
    #[error(transparent)]
    Net(#[from] NetError),
}

/// Runs the schedule against `config.target` and reports what happened.
pub async fn run(env: &Env, pool: Arc<Pool>, config: &RunConfig) -> Result<RunOutcome, RunError> {
    let requests = scheduled_requests(config.rate, config.duration);
    if requests == 0 {
        return Err(RunError::Config("rate times duration schedules no request"));
    }
    if requests > MAX_REQUESTS {
        return Err(RunError::Config(
            "the schedule holds more than 2^32 requests",
        ));
    }
    if config.concurrency == 0 {
        return Err(RunError::Config("concurrency must be at least 1"));
    }
    let authority = HeaderValue::from_str(&config.authority)
        .map_err(|_| RunError::Config("the target is not a valid Host header"))?;
    let spans_per_request = pool.config().spans_per_request;
    let stats = Stats::new(AckLedger::new(pool.run_id(), spans_per_request, requests))?;

    let mut connections = Vec::with_capacity(config.concurrency);
    for _ in 0..config.concurrency {
        connections.push(Connection::open(env, config.target, config.timeout).await?);
    }

    let plan = Arc::new(Plan {
        target: config.target,
        authority,
        headers: config.headers.clone(),
        rate: config.rate,
        requests,
        gzip: config.gzip,
        timeout: config.timeout,
        start: env.clock.now(),
        wall_start_nanos: u64::try_from(env.clock.wall_now().micros())
            .unwrap_or(0)
            .saturating_mul(1_000),
    });
    let shared = Arc::new(Shared {
        next: AtomicU64::new(0),
        released: AtomicU64::new(0),
        slots: (0..config.concurrency).map(|_| Notify::new()).collect(),
        stats: Mutex::new(stats),
    });
    let mut pacer = env.spawn(
        "loadgen-pacer",
        pace(env.clone(), Arc::clone(&plan), Arc::clone(&shared)),
    );
    let mut workers: Vec<JoinHandle<Result<(), RunError>>> = connections
        .into_iter()
        .map(|connection| {
            let work = worker(
                env.clone(),
                Arc::clone(&plan),
                Arc::clone(&pool),
                Arc::clone(&shared),
                connection,
            );
            env.spawn("loadgen-worker", work)
        })
        .collect();
    // A failed worker leaves its request unsent; the others finish the schedule, then the error is returned.
    let mut finished = Ok(());
    for worker in &mut workers {
        let joined = worker.join().await.unwrap_or(Err(RunError::Cancelled));
        finished = finished.and(joined);
    }
    pacer.join().await.map_err(|_| RunError::Cancelled)?;
    finished?;

    let stats = shared.stats.lock().unwrap_or_else(PoisonError::into_inner);
    let report = stats.report(&plan, &pool, config);
    Ok(RunOutcome {
        report,
        ledger: stats.ledger.clone(),
    })
}

/// The immutable part of a run, shared by the workers.
#[derive(Debug)]
struct Plan {
    target: SocketAddr,
    authority: HeaderValue,
    headers: Vec<(HeaderName, HeaderValue)>,
    rate: u64,
    requests: u64,
    gzip: bool,
    timeout: Duration,
    start: Instant,
    wall_start_nanos: u64,
}

impl Plan {
    fn request(&self, body: Bytes) -> Request<Full<Bytes>> {
        let mut request = Request::new(Full::new(body));
        *request.method_mut() = Method::POST;
        *request.uri_mut() = Uri::from_static(TRACES_PATH);
        let headers = request.headers_mut();
        headers.insert(HOST, self.authority.clone());
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-protobuf"),
        );
        if self.gzip {
            headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        for (name, value) in &self.headers {
            headers.append(name.clone(), value.clone());
        }
        request
    }
}

#[derive(Debug)]
struct Shared {
    /// The lowest request index no worker has taken.
    next: AtomicU64,
    /// Requests below this index are due.
    released: AtomicU64,
    /// Wakes the worker waiting for request `i` through slot `i % slots.len()`.
    slots: Vec<Notify>,
    stats: Mutex<Stats>,
}

impl Shared {
    /// Waits until the pacer has released request `index`.
    async fn wait_for_release(&self, index: u64) {
        let slot = &self.slots[(index % self.slots.len() as u64) as usize];
        // A release that lands before the wait leaves a permit in the slot, so no wakeup is lost; a permit
        // left for an earlier request of the same slot only causes one extra check.
        while self.released.load(Ordering::Acquire) <= index {
            slot.notified().await;
        }
    }
}

/// Releases every request at its due time, in order; see the module documentation.
async fn pace(env: Env, plan: Arc<Plan>, shared: Arc<Shared>) {
    for index in 0..plan.requests {
        let due = plan
            .start
            .saturating_add(Duration::from_nanos(offset_nanos(index, plan.rate)));
        loop {
            let now = env.clock.now();
            if now >= due {
                break;
            }
            if due.saturating_duration_since(now) > SPIN_WINDOW {
                let wake = Instant::from_nanos(due.nanos().saturating_sub(nanos(SPIN_WINDOW)));
                env.clock.sleep_until(wake).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        shared.released.store(index + 1, Ordering::Release);
        shared.slots[(index % shared.slots.len() as u64) as usize].notify_one();
    }
}

/// One HTTP/1.1 connection; dropping it closes the socket.
struct Connection {
    sender: SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl Connection {
    async fn open(env: &Env, target: SocketAddr, timeout: Duration) -> Result<Self, ConnectError> {
        let stream = env
            .net
            .connect(target, env.clock.now().saturating_add(timeout))
            .await?;
        let (sender, connection) = http1::handshake(TokioIo::new(stream)).await?;
        let driver = env.spawn("loadgen-connection", async move {
            // The connection ends when the sender is dropped or the peer closes it.
            let _closed = connection.await;
        });
        Ok(Self { sender, driver })
    }

    async fn exchange(
        &mut self,
        request: Request<Full<Bytes>>,
    ) -> Result<(StatusCode, Bytes), hyper::Error> {
        self.sender.ready().await?;
        let response = self.sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        Ok((status, body))
    }
}

enum Attempt {
    /// No connection could be opened.
    NotSent { done: Instant },
    /// Sent, but no complete response arrived.
    Failed {
        sent_at: Instant,
        done: Instant,
        timed_out: bool,
    },
    /// A complete response arrived.
    Answered {
        sent_at: Instant,
        done: Instant,
        status: StatusCode,
        ack: Ack,
    },
}

/// What a 2xx response body says.
#[derive(Clone, Copy)]
enum Ack {
    /// Not a 2xx response.
    None,
    /// Every span accepted.
    Full,
    /// Some spans rejected.
    Partial(u64),
    /// The body is not an `ExportTraceServiceResponse`.
    Undecodable,
}

impl Ack {
    fn of(status: StatusCode, body: &[u8]) -> Self {
        if !status.is_success() {
            return Self::None;
        }
        match ExportTraceServiceResponse::decode(body) {
            Ok(response) => match response.partial_success.map(|p| p.rejected_spans) {
                Some(rejected) if rejected > 0 => Self::Partial(rejected.unsigned_abs()),
                _ => Self::Full,
            },
            Err(_) => Self::Undecodable,
        }
    }
}

async fn worker(
    env: Env,
    plan: Arc<Plan>,
    pool: Arc<Pool>,
    shared: Arc<Shared>,
    connection: Connection,
) -> Result<(), RunError> {
    let mut connection = Some(connection);
    let mut gzip = plan.gzip.then(Gzip::new);
    loop {
        let index = shared.next.fetch_add(1, Ordering::Relaxed);
        if index >= plan.requests {
            return Ok(());
        }
        let offset = offset_nanos(index, plan.rate);
        let intended = plan.start.saturating_add(Duration::from_nanos(offset));
        let raw = pool.request(index, plan.wall_start_nanos.saturating_add(offset));
        let request_bytes = raw.len();
        let body = match gzip.as_mut() {
            Some(gzip) => gzip.compress(&raw)?,
            None => raw,
        };
        let wire_bytes = body.len();
        shared.wait_for_release(index).await;
        let attempt = send(&env, &plan, &mut connection, Bytes::from(body)).await;
        shared
            .stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(index, intended, &attempt, request_bytes, wire_bytes);
    }
}

async fn send(env: &Env, plan: &Plan, connection: &mut Option<Connection>, body: Bytes) -> Attempt {
    // A connection the peer closed while it sat idle is replaced before anything is written, so the request
    // is neither counted as sent nor retried.
    if let Some(idle) = connection.as_mut()
        && !matches!(
            env.timeout(plan.timeout, idle.sender.ready()).await,
            Ok(Ok(()))
        )
    {
        *connection = None;
    }
    let open = match connection {
        Some(open) => open,
        None => match Connection::open(env, plan.target, plan.timeout).await {
            Ok(open) => connection.insert(open),
            Err(_) => {
                return Attempt::NotSent {
                    done: env.clock.now(),
                };
            }
        },
    };
    let request = plan.request(body);
    let sent_at = env.clock.now();
    match env.timeout(plan.timeout, open.exchange(request)).await {
        Ok(Ok((status, body))) => Attempt::Answered {
            sent_at,
            done: env.clock.now(),
            status,
            ack: Ack::of(status, &body),
        },
        failed => {
            *connection = None;
            Attempt::Failed {
                sent_at,
                done: env.clock.now(),
                timed_out: failed.is_err(),
            }
        }
    }
}

/// A gzip encoder that keeps its deflate state from body to body: a fresh encoder allocates and zeroes
/// several hundred KiB, which per request costs more than the compression itself.
struct Gzip(DeflateEncoder<Vec<u8>>);

impl Gzip {
    /// RFC 1952 member header: deflate, no flags, no modification time, unknown operating system.
    const HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];

    fn new() -> Self {
        Self(DeflateEncoder::new(Vec::new(), Compression::fast()))
    }

    /// One gzip member holding `body`.
    fn compress(&mut self, body: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(body.len() / 2 + 32);
        out.extend_from_slice(&Self::HEADER);
        // Resetting reuses the deflate state; the writer it hands back holds only the previous stream's end.
        self.0.reset(out)?;
        self.0.write_all(body)?;
        self.0.try_finish()?;
        let mut out = std::mem::take(self.0.get_mut());
        let mut crc = Crc::new();
        crc.update(body);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&crc.amount().to_le_bytes());
        Ok(out)
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn ratio(count: u64, seconds: f64) -> f64 {
    if seconds > 0.0 {
        count as f64 / seconds
    } else {
        0.0
    }
}

/// Everything the workers record, behind one lock taken once per request.
#[derive(Debug)]
struct Stats {
    latency: Histogram<u64>,
    service: Histogram<u64>,
    lag: Histogram<u64>,
    statuses: BTreeMap<u16, u64>,
    errors: ErrorCounts,
    sent: u64,
    responses: u64,
    partial_requests: u64,
    rejected_spans: u64,
    request_bytes: u64,
    wire_bytes: u64,
    last_send: Option<Instant>,
    last_done: Option<Instant>,
    ledger: AckLedger,
}

impl Stats {
    fn new(ledger: AckLedger) -> Result<Self, CreationError> {
        let histogram = || Histogram::new_with_bounds(1, HISTOGRAM_MAX_US, 3);
        Ok(Self {
            latency: histogram()?,
            service: histogram()?,
            lag: histogram()?,
            statuses: BTreeMap::new(),
            errors: ErrorCounts::default(),
            sent: 0,
            responses: 0,
            partial_requests: 0,
            rejected_spans: 0,
            request_bytes: 0,
            wire_bytes: 0,
            last_send: None,
            last_done: None,
            ledger,
        })
    }

    fn record(
        &mut self,
        index: u64,
        intended: Instant,
        attempt: &Attempt,
        request_bytes: usize,
        wire_bytes: usize,
    ) {
        let (sent_at, done) = match *attempt {
            Attempt::NotSent { done } => {
                self.errors.connect += 1;
                (None, done)
            }
            Attempt::Failed {
                sent_at,
                done,
                timed_out,
            } => {
                if timed_out {
                    self.errors.timeout += 1;
                } else {
                    self.errors.transport += 1;
                }
                (Some(sent_at), done)
            }
            Attempt::Answered {
                sent_at,
                done,
                status,
                ack,
            } => {
                self.responses += 1;
                *self.statuses.entry(status.as_u16()).or_default() += 1;
                if status.is_success() {
                    self.latency
                        .saturating_record(micros(done.saturating_duration_since(intended)));
                    self.service
                        .saturating_record(micros(done.saturating_duration_since(sent_at)));
                }
                match ack {
                    Ack::Full => self.ledger.ack(index),
                    Ack::Partial(rejected) => {
                        self.partial_requests += 1;
                        self.rejected_spans += rejected;
                    }
                    Ack::Undecodable => self.errors.bad_response += 1,
                    Ack::None => {}
                }
                (Some(sent_at), done)
            }
        };
        if let Some(sent_at) = sent_at {
            self.sent += 1;
            self.request_bytes += request_bytes as u64;
            self.wire_bytes += wire_bytes as u64;
            self.lag
                .saturating_record(micros(sent_at.saturating_duration_since(intended)));
            self.last_send = self.last_send.max(Some(sent_at));
        }
        self.last_done = self.last_done.max(Some(done));
    }

    fn report(&self, plan: &Plan, pool: &Pool, config: &RunConfig) -> RunReport {
        let pool_config = pool.config();
        let spans = u64::from(pool_config.spans_per_request);
        let interval = Duration::from_nanos(offset_nanos(1, plan.rate));
        let send_window = self.last_send.map_or(Duration::ZERO, |last| {
            last.saturating_duration_since(plan.start) + interval
        });
        let elapsed = self.last_done.map_or(Duration::ZERO, |last| {
            last.saturating_duration_since(plan.start)
        });
        let acked = self.ledger.acked_requests();
        let mean = |bytes: u64| ratio(bytes, self.sent as f64);
        RunReport {
            schema: RUN_SCHEMA,
            run_id: pool.run_id().to_string(),
            seed: pool_config.seed,
            target: config.authority.clone(),
            lane: Lane {
                span_bytes: pool_config.span_size.bytes(),
                spans_per_request: pool_config.spans_per_request,
                gzip: config.gzip,
                templates: pool_config.templates,
                request_bytes_mean: mean(self.request_bytes),
                wire_bytes_mean: mean(self.wire_bytes),
            },
            schedule: Schedule {
                target_rps: config.rate,
                duration_s: config.duration.as_secs_f64(),
                requests: plan.requests,
                concurrency: config.concurrency,
                timeout_ms: u64::try_from(config.timeout.as_millis()).unwrap_or(u64::MAX),
            },
            outcome: Outcome {
                sent: self.sent,
                responses: self.responses,
                acked,
                acked_spans: acked * spans,
                partial_success_requests: self.partial_requests,
                rejected_spans: self.rejected_spans,
                status_counts: self.statuses.clone(),
                errors: self.errors,
                achieved_rps: ratio(self.sent, send_window.as_secs_f64()),
                acked_rps: ratio(acked, elapsed.as_secs_f64()),
                acked_spans_per_s: ratio(acked * spans, elapsed.as_secs_f64()),
                send_window_s: send_window.as_secs_f64(),
                elapsed_s: elapsed.as_secs_f64(),
            },
            latency_us: Summary::of(&self.latency),
            service_time_us: Summary::of(&self.service),
            send_lag_us: Summary::of(&self.lag),
            not_acked_requests: self.ledger.not_acked_ranges(),
            generator_cpu: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn an_empty_histogram_summarizes_to_zeros() {
        let histogram = Histogram::<u64>::new_with_bounds(1, HISTOGRAM_MAX_US, 3).unwrap();
        assert_eq!(Summary::of(&histogram), Summary::default());
    }

    #[test]
    fn summaries_report_the_recorded_percentiles() {
        let mut histogram = Histogram::<u64>::new_with_bounds(1, HISTOGRAM_MAX_US, 3).unwrap();
        for value in 1..=1_000 {
            histogram.saturating_record(value);
        }
        let summary = Summary::of(&histogram);
        assert_eq!((summary.count, summary.max), (1_000, 1_000));
        // Quantiles round up to the next recorded value, so allow one unit for floating-point rounding.
        for (value, expected) in [
            (summary.p50, 500),
            (summary.p90, 900),
            (summary.p99, 990),
            (summary.p99_9, 999),
        ] {
            assert!(
                (expected..=expected + 1).contains(&value),
                "{value} vs {expected}"
            );
        }
    }

    #[test]
    fn ack_reads_partial_success() {
        let partial = ExportTraceServiceResponse {
            partial_success: Some(
                opentelemetry_proto::tonic::collector::trace::v1::ExportTracePartialSuccess {
                    rejected_spans: 3,
                    error_message: "too old".to_owned(),
                },
            ),
        };
        assert!(matches!(
            Ack::of(StatusCode::OK, &partial.encode_to_vec()),
            Ack::Partial(3)
        ));
        assert!(matches!(Ack::of(StatusCode::OK, b""), Ack::Full));
        assert!(matches!(
            Ack::of(StatusCode::OK, b"\xff\xff"),
            Ack::Undecodable
        ));
        assert!(matches!(
            Ack::of(StatusCode::TOO_MANY_REQUESTS, b""),
            Ack::None
        ));
    }

    proptest! {
        /// Each body compresses to one complete gzip member that inflates to the body, however many bodies
        /// the encoder handled before.
        #[test]
        fn reused_gzip_encoders_produce_valid_members(
            bodies in proptest::collection::vec(proptest::collection::vec(0u8..8, 0..5_000), 1..6),
        ) {
            use std::io::Read as _;
            let mut gzip = Gzip::new();
            for body in &bodies {
                let member = gzip.compress(body).unwrap();
                let mut decoder = flate2::bufread::GzDecoder::new(member.as_slice());
                let mut inflated = Vec::new();
                decoder.read_to_end(&mut inflated).unwrap();
                prop_assert_eq!(&inflated, body);
                // Nothing may follow the member.
                prop_assert!(decoder.into_inner().is_empty());
            }
        }
        /// The schedule is computed from the index alone: it never drifts and never runs backwards.
        #[test]
        fn the_schedule_neither_drifts_nor_goes_backwards(rate in 1u64..10_000_000, index in 0u64..1 << 40, seconds in 0u64..100_000) {
            prop_assert_eq!(offset_nanos(rate * seconds, rate), seconds * 1_000_000_000);
            prop_assert!(offset_nanos(index, rate) <= offset_nanos(index + 1, rate));
            prop_assert_eq!(scheduled_requests(rate, Duration::from_secs(seconds)), rate * seconds);
        }
    }
}
