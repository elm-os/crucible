//! The read generator (`crucible-loadgen read`) and the ack-to-API probe (`crucible-loadgen fresh`).
//!
//! `read` sends API reads open loop, like [`crate::run`]: request `i` is due at `start + t_i`, fixed before the
//! first request leaves, and its latency counts from that intended time, so a stalled server cannot hide behind
//! a stalled client. The gaps between due times are the schedule's think times: all equal to `1 / rate`
//! ([`Think::Fixed`]), or drawn from the seed from an exponential distribution of that mean
//! ([`Think::Exponential`], a Poisson arrival process). Request `i` uses class `i % classes`. A class is a
//! method, a path with its query and an optional JSON body, whose placeholders name the dataset that
//! [`crate::run`] loaded through ingest: the reports of those runs list every acknowledged request, and
//! [`Identity`] gives the ids and start times of its traces and spans, so no read is spent finding them.
//!
//! | Placeholder | Value |
//! |---|---|
//! | `{project}` | the project id |
//! | `{trace_id}` | a trace of the dataset, drawn per request |
//! | `{root_span_id}` | that trace's first span |
//! | `{span_id}` | a span of that trace, drawn per request |
//! | `{span_start}`, `{trace_start}` | the start time of that span and of the trace's first span, RFC 3339 |
//! | `{from}`, `{to}` | the read window: `to` is the request's due time on the wall clock, `from` the window before |
//!
//! `fresh` measures how soon an acknowledged trace shows in the API. It sends one small trace at a time to
//! ingest and, from the acknowledgement on, lists the project's traces every `poll` until the list names it:
//! the method of milestone 1's end-to-end measurement.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{ChaChaRng, Env, Instant, Rng};
use crucible_records::{Timestamp, Uuid};
use hdrhistogram::Histogram;
use http::header::{CONTENT_TYPE, HOST};
use http::{HeaderName, HeaderValue, Method, Request, Uri};
use http_body_util::Full;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::pool::{Identity, Pool, PoolConfig, SpanSize, span_offset_nanos};
use crate::run::{
    Connection, ErrorCounts, HISTOGRAM_MAX_US, RunError, SPIN_WINDOW, SpanTimes, Summary,
    TRACES_PATH,
};

/// The `schema` value of [`ReadReport`].
pub const READ_SCHEMA: &str = "crucible-loadgen.read.v1";
/// The `schema` value of [`FreshReport`].
pub const FRESH_SCHEMA: &str = "crucible-loadgen.fresh.v1";
/// The largest read schedule, in requests: the due times are computed before the first request.
pub const MAX_READS: u64 = 1 << 24;

/// One kind of read.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadClass {
    /// The class's name in the report.
    pub name: String,
    /// `GET` or `POST`.
    pub method: String,
    /// The path and query, with placeholders.
    pub path: String,
    /// A JSON body, with placeholders.
    #[serde(default)]
    pub body: Option<String>,
    /// A JSON pointer that must name a non-empty array, object or string in every 2xx answer; otherwise the
    /// answer counts as empty, not as a success.
    #[serde(default)]
    pub nonempty: Option<String>,
}

/// How the gaps between due times are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Think {
    /// Every gap is `1 / rate`.
    Fixed,
    /// Gaps drawn from an exponential distribution of mean `1 / rate`: a Poisson arrival process.
    Exponential,
}

/// What to read, where and how fast.
#[derive(Clone, Debug)]
pub struct ReadConfig {
    /// The API listener.
    pub target: SocketAddr,
    /// The `Host` header.
    pub authority: String,
    /// Extra headers on every request, such as `Authorization`.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// The project that `{project}` names.
    pub project: String,
    /// The classes, used in turn.
    pub classes: Vec<ReadClass>,
    /// Requests per second, on average.
    pub rate: f64,
    /// The schedule's length.
    pub duration: Duration,
    /// How the gaps are drawn.
    pub think: Think,
    /// Connections, and so the most requests in flight.
    pub concurrency: usize,
    /// Deadline for connecting and for each request.
    pub timeout: Duration,
    /// The read window that `{from}` and `{to}` span.
    pub window: Duration,
    /// Fixes the think times and the traces drawn.
    pub seed: u64,
}

/// The fields of a `crucible-loadgen.run.v1` report that name a loaded run.
#[derive(Clone, Debug, Deserialize)]
pub struct LoadedRun {
    run_id: String,
    started_at: String,
    lane: LoadedLane,
    schedule: LoadedSchedule,
    not_acked_requests: Vec<[u64; 2]>,
}

#[derive(Clone, Debug, Deserialize)]
struct LoadedLane {
    spans_per_request: u32,
    #[serde(default = "one")]
    traces_per_request: u32,
    #[serde(default)]
    spread_s: f64,
    #[serde(default)]
    backdate_s: f64,
}

#[derive(Clone, Debug, Deserialize)]
struct LoadedSchedule {
    target_rps: u64,
    requests: u64,
}

const fn one() -> u32 {
    1
}

/// Why a dataset or a read schedule cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A run report did not parse.
    #[error("a run report does not parse: {0}")]
    Report(#[from] serde_json::Error),
    /// A run report's run id is not a UUID.
    #[error("a run report's run_id is not a UUID")]
    RunId,
    /// A run report's start time is not RFC 3339.
    #[error("a run report's started_at is not an RFC 3339 time")]
    StartedAt,
    /// The dataset holds no acknowledged request.
    #[error("the dataset holds no acknowledged request")]
    Empty,
    /// A class cannot be sent.
    #[error("class `{0}`: {1}")]
    Class(String, &'static str),
    /// The schedule cannot run.
    #[error(transparent)]
    Run(#[from] RunError),
}

/// One loaded run, ready to draw from.
#[derive(Clone, Debug)]
struct Loaded {
    identity: Identity,
    traces_per_request: u32,
    times: SpanTimes,
    /// Acknowledged request ranges `[start, end)`, ascending, and the acknowledged requests before each.
    acked: Vec<([u64; 2], u64)>,
    acked_count: u64,
}

/// The traces and spans a read can name: the acknowledged requests of the runs that loaded the dataset.
#[derive(Clone, Debug)]
pub struct Dataset {
    runs: Vec<Loaded>,
    acked: u64,
}

/// A span a read names, with its trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drawn {
    /// The trace id, lowercase hex.
    pub trace_id: String,
    /// The trace's first span, lowercase hex.
    pub root_span_id: String,
    /// The drawn span, lowercase hex.
    pub span_id: String,
    /// The drawn span's start time.
    pub span_start: Timestamp,
    /// The trace's first span's start time.
    pub trace_start: Timestamp,
}

impl Dataset {
    /// The dataset of these `crucible-loadgen.run.v1` reports.
    pub fn from_reports<'a>(reports: impl IntoIterator<Item = &'a str>) -> Result<Self, ReadError> {
        let mut runs = Vec::new();
        let mut acked = 0;
        for report in reports {
            let run: LoadedRun = serde_json::from_str(report)?;
            let run_id: Uuid = run.run_id.parse().map_err(|_| ReadError::RunId)?;
            let started =
                Timestamp::parse_rfc3339(&run.started_at).map_err(|_| ReadError::StartedAt)?;
            let requests = run.schedule.requests;
            let mut ranges = Vec::new();
            let mut count = 0;
            let mut next = 0;
            let mut not_acked = run.not_acked_requests.clone();
            not_acked.sort_unstable();
            for [from, to] in not_acked.into_iter().chain([[requests, requests]]) {
                if from > next {
                    ranges.push(([next, from.min(requests)], count));
                    count += from.min(requests) - next;
                }
                next = next.max(to);
            }
            if count == 0 {
                continue;
            }
            runs.push(Loaded {
                identity: Identity::new(
                    run_id,
                    run.lane.spans_per_request,
                    run.lane.traces_per_request,
                ),
                traces_per_request: run.lane.traces_per_request.max(1),
                times: SpanTimes {
                    wall_start_nanos: u64::try_from(started.micros())
                        .unwrap_or(0)
                        .saturating_mul(1_000),
                    rate: run.schedule.target_rps,
                    requests,
                    spread_nanos: (run.lane.spread_s.max(0.0) * 1e9) as u64,
                    backdate_nanos: (run.lane.backdate_s.max(0.0) * 1e9) as u64,
                },
                acked: ranges,
                acked_count: count,
            });
            acked += count;
        }
        if acked == 0 {
            return Err(ReadError::Empty);
        }
        Ok(Self { runs, acked })
    }

    /// The acknowledged requests of the dataset.
    pub fn acked_requests(&self) -> u64 {
        self.acked
    }

    /// The loaded runs that acknowledged at least one request.
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// A span drawn with `rng`: an acknowledged request uniformly, then one of its traces, then one of its spans.
    pub fn draw(&self, rng: &dyn Rng) -> Drawn {
        let mut pick = rng.next_u64() % self.acked;
        let run = self
            .runs
            .iter()
            .find(|run| {
                let inside = pick < run.acked_count;
                if !inside {
                    pick -= run.acked_count;
                }
                inside
            })
            .unwrap_or(&self.runs[0]);
        let at = run.acked.partition_point(|(_, before)| *before <= pick);
        let ([from, _], before) = run.acked[at.saturating_sub(1)];
        let index = from + (pick - before);
        let trace = (rng.next_u64() % u64::from(run.traces_per_request)) as u32;
        let positions = run.identity.positions(trace);
        let width = (positions.end - positions.start).max(1) as u64;
        let position = positions.start + (rng.next_u64() % width) as usize;
        let base = run.times.base(index);
        let start = |position: usize| {
            Timestamp::from_unix_nanos(base.saturating_add(span_offset_nanos(position)))
        };
        let span = |position: usize| Pool::span_id(run.identity.seq(index, position)).to_string();
        Drawn {
            trace_id: run.identity.trace_id(index, trace).to_string(),
            root_span_id: span(positions.start),
            span_id: span(position),
            span_start: start(position),
            trace_start: start(positions.start),
        }
    }
}

/// `text` with every placeholder filled.
pub fn fill(text: &str, project: &str, drawn: &Drawn, from: Timestamp, to: Timestamp) -> String {
    text.replace("{project}", project)
        .replace("{trace_id}", &drawn.trace_id)
        .replace("{root_span_id}", &drawn.root_span_id)
        .replace("{span_id}", &drawn.span_id)
        .replace("{span_start}", &drawn.span_start.to_string())
        .replace("{trace_start}", &drawn.trace_start.to_string())
        .replace("{from}", &from.to_string())
        .replace("{to}", &to.to_string())
}

/// Whether the value at `pointer` in a JSON body is a non-empty array, object or string.
pub fn nonempty(body: &[u8], pointer: &str) -> Option<bool> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    Some(match value.pointer(pointer) {
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(serde_json::Value::Object(fields)) => !fields.is_empty(),
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        _ => false,
    })
}

/// The due offsets of a schedule of `requests` requests, in nanoseconds from its start: see the module
/// documentation.
pub fn due_offsets(requests: u64, rate: f64, think: Think, seed: u64) -> Vec<u64> {
    let mean = 1e9 / rate.max(f64::MIN_POSITIVE);
    let rng = ChaChaRng::from_u64(seed).fork("think");
    let mut at = 0.0f64;
    (0..requests)
        .map(|index| {
            let due = match think {
                Think::Fixed => index as f64 * mean,
                Think::Exponential => {
                    let due = at;
                    // A uniform draw in (0, 1]: 53 random bits, never 0, so the logarithm is finite.
                    let uniform = ((rng.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64;
                    at += -uniform.ln() * mean;
                    due
                }
            };
            due as u64
        })
        .collect()
}

/// What one class did.
#[derive(Clone, Debug, Serialize)]
pub struct ClassReport {
    /// The class's name.
    pub name: String,
    /// Its method.
    pub method: String,
    /// Its path template.
    pub path: String,
    /// Requests written to a connection.
    pub sent: u64,
    /// 2xx answers, non-empty where the class says what must not be empty: the answers `latency_us` covers.
    pub ok: u64,
    /// 2xx answers whose `nonempty` pointer named nothing, or an empty value.
    pub empty: u64,
    /// Answers by HTTP status code.
    pub status_counts: BTreeMap<u16, u64>,
    /// Requests without an answer.
    pub errors: ErrorCounts,
    /// Intended send time to complete answer, over `ok` answers.
    pub latency_us: Summary,
    /// `latency_us` over the reads due in the schedule's first half, and over those due in its second: halves
    /// that differ show a server that was still settling or a disturbance, not a steady state.
    pub latency_halves_us: [Summary; 2],
    /// Actual send time to complete answer, over `ok` answers.
    pub service_time_us: Summary,
}

/// The JSON report of a read run (schema [`READ_SCHEMA`], documented in `docs/testing/perf.md`).
#[derive(Clone, Debug, Serialize)]
pub struct ReadReport {
    /// Always [`READ_SCHEMA`].
    pub schema: &'static str,
    /// The target as given.
    pub target: String,
    /// The seed.
    pub seed: u64,
    /// The wall-clock time of the schedule's start, RFC 3339.
    pub started_at: String,
    /// What was asked for.
    pub schedule: ReadSchedule,
    /// The loaded runs that acknowledged a request, and their acknowledged requests.
    pub dataset_runs: usize,
    /// See `dataset_runs`.
    pub dataset_requests: u64,
    /// One entry per class.
    pub classes: Vec<ClassReport>,
    /// Intended to actual send time, over every sent request.
    pub send_lag_us: Summary,
    /// From the schedule's start to the last answer or failure.
    pub elapsed_s: f64,
}

/// The requested read schedule.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ReadSchedule {
    /// Requests per second, on average.
    pub rate: f64,
    /// Schedule length in seconds.
    pub duration_s: f64,
    /// How the gaps were drawn.
    pub think: Think,
    /// Requests in the schedule.
    pub requests: u64,
    /// Connections.
    pub concurrency: usize,
    /// Per-request deadline.
    pub timeout_ms: u64,
    /// The read window of `{from}` and `{to}`, in seconds.
    pub window_s: f64,
}

#[derive(Debug)]
struct ClassStats {
    latency: Histogram<u64>,
    halves: [Histogram<u64>; 2],
    service: Histogram<u64>,
    statuses: BTreeMap<u16, u64>,
    errors: ErrorCounts,
    sent: u64,
    ok: u64,
    empty: u64,
}

impl ClassStats {
    fn new() -> Result<Self, RunError> {
        let histogram = || Histogram::new_with_bounds(1, HISTOGRAM_MAX_US, 3);
        Ok(Self {
            latency: histogram()?,
            halves: [histogram()?, histogram()?],
            service: histogram()?,
            statuses: BTreeMap::new(),
            errors: ErrorCounts::default(),
            sent: 0,
            ok: 0,
            empty: 0,
        })
    }
}

#[derive(Debug)]
struct ReadStats {
    classes: Vec<ClassStats>,
    lag: Histogram<u64>,
    last_done: Option<Instant>,
}

#[derive(Debug)]
struct ReadShared {
    next: AtomicU64,
    released: AtomicU64,
    slots: Vec<Notify>,
    stats: Mutex<ReadStats>,
}

impl ReadShared {
    async fn wait_for_release(&self, index: u64) {
        let slot = &self.slots[(index % self.slots.len() as u64) as usize];
        while self.released.load(Ordering::Acquire) <= index {
            slot.notified().await;
        }
    }
}

#[derive(Debug)]
struct ReadPlan {
    config: ReadConfig,
    authority: HeaderValue,
    dataset: Dataset,
    offsets: Vec<u64>,
    start: Instant,
    wall_start_nanos: u64,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Runs the read schedule against `config.target` over `dataset` and reports what happened per class.
pub async fn read(
    env: &Env,
    dataset: Dataset,
    config: &ReadConfig,
) -> Result<ReadReport, ReadError> {
    let requests = (config.rate * config.duration.as_secs_f64()) as u64;
    if requests == 0 || config.rate <= 0.0 {
        return Err(RunError::Config("rate times duration schedules no request").into());
    }
    if requests > MAX_READS {
        return Err(RunError::Config("the read schedule holds more than 2^24 requests").into());
    }
    if config.concurrency == 0 {
        return Err(RunError::Config("concurrency must be at least 1").into());
    }
    if config.classes.is_empty() {
        return Err(RunError::Config("a read run needs at least one class").into());
    }
    for class in &config.classes {
        if class.method != "GET" && class.method != "POST" {
            return Err(ReadError::Class(
                class.name.clone(),
                "the method must be GET or POST",
            ));
        }
        if !class.path.starts_with('/') {
            return Err(ReadError::Class(
                class.name.clone(),
                "the path must start with /",
            ));
        }
    }
    let authority = HeaderValue::from_str(&config.authority)
        .map_err(|_| RunError::Config("the target is not a valid Host header"))?;
    let stats = ReadStats {
        classes: config
            .classes
            .iter()
            .map(|_| ClassStats::new())
            .collect::<Result<_, _>>()?,
        lag: Histogram::new_with_bounds(1, HISTOGRAM_MAX_US, 3).map_err(RunError::from)?,
        last_done: None,
    };
    let mut connections = Vec::with_capacity(config.concurrency);
    for _ in 0..config.concurrency {
        connections.push(
            Connection::open(env, config.target, config.timeout)
                .await
                .map_err(RunError::from)?,
        );
    }
    let wall_start = env.clock.wall_now();
    let plan = Arc::new(ReadPlan {
        config: config.clone(),
        authority,
        dataset,
        offsets: due_offsets(requests, config.rate, config.think, config.seed),
        start: env.clock.now(),
        wall_start_nanos: u64::try_from(wall_start.micros())
            .unwrap_or(0)
            .saturating_mul(1_000),
    });
    let shared = Arc::new(ReadShared {
        next: AtomicU64::new(0),
        released: AtomicU64::new(0),
        slots: (0..config.concurrency).map(|_| Notify::new()).collect(),
        stats: Mutex::new(stats),
    });
    let mut pacer = env.spawn(
        "loadgen-read-pacer",
        pace(env.clone(), Arc::clone(&plan), Arc::clone(&shared)),
    );
    let mut workers: Vec<_> = connections
        .into_iter()
        .map(|connection| {
            let work = read_worker(
                env.clone(),
                Arc::clone(&plan),
                Arc::clone(&shared),
                connection,
            );
            env.spawn("loadgen-read-worker", work)
        })
        .collect();
    for worker in &mut workers {
        worker.join().await.map_err(|_| RunError::Cancelled)?;
    }
    pacer.join().await.map_err(|_| RunError::Cancelled)?;

    let stats = shared.stats.lock().unwrap_or_else(PoisonError::into_inner);
    let elapsed = stats.last_done.map_or(Duration::ZERO, |done| {
        done.saturating_duration_since(plan.start)
    });
    Ok(ReadReport {
        schema: READ_SCHEMA,
        target: config.authority.clone(),
        seed: config.seed,
        started_at: wall_start.to_string(),
        schedule: ReadSchedule {
            rate: config.rate,
            duration_s: config.duration.as_secs_f64(),
            think: config.think,
            requests,
            concurrency: config.concurrency,
            timeout_ms: u64::try_from(config.timeout.as_millis()).unwrap_or(u64::MAX),
            window_s: config.window.as_secs_f64(),
        },
        dataset_runs: plan.dataset.runs(),
        dataset_requests: plan.dataset.acked_requests(),
        classes: config
            .classes
            .iter()
            .zip(&stats.classes)
            .map(|(class, stats)| ClassReport {
                name: class.name.clone(),
                method: class.method.clone(),
                path: class.path.clone(),
                sent: stats.sent,
                ok: stats.ok,
                empty: stats.empty,
                status_counts: stats.statuses.clone(),
                errors: stats.errors,
                latency_us: Summary::of(&stats.latency),
                latency_halves_us: [Summary::of(&stats.halves[0]), Summary::of(&stats.halves[1])],
                service_time_us: Summary::of(&stats.service),
            })
            .collect(),
        send_lag_us: Summary::of(&stats.lag),
        elapsed_s: elapsed.as_secs_f64(),
    })
}

/// Releases every read at its due time, in order: the pacer of [`crate::run`], over precomputed offsets.
async fn pace(env: Env, plan: Arc<ReadPlan>, shared: Arc<ReadShared>) {
    for (index, offset) in (0u64..).zip(&plan.offsets) {
        let due = plan.start.saturating_add(Duration::from_nanos(*offset));
        loop {
            let now = env.clock.now();
            if now >= due {
                break;
            }
            if due.saturating_duration_since(now) > SPIN_WINDOW {
                let window = u64::try_from(SPIN_WINDOW.as_nanos()).unwrap_or(u64::MAX);
                env.clock
                    .sleep_until(Instant::from_nanos(due.nanos().saturating_sub(window)))
                    .await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        shared.released.store(index + 1, Ordering::Release);
        shared.slots[(index % shared.slots.len() as u64) as usize].notify_one();
    }
}

async fn read_worker(
    env: Env,
    plan: Arc<ReadPlan>,
    shared: Arc<ReadShared>,
    connection: Connection,
) {
    let config = &plan.config;
    let rng = ChaChaRng::from_u64(config.seed).fork("draws");
    let mut connection = Some(connection);
    loop {
        let index = shared.next.fetch_add(1, Ordering::Relaxed);
        let Some(offset) = plan.offsets.get(index as usize).copied() else {
            return;
        };
        let class_index = (index % config.classes.len() as u64) as usize;
        let class = &config.classes[class_index];
        // Each request's draw is a function of the seed and its index alone.
        let drawn = plan.dataset.draw(rng.fork(&index.to_string()).as_ref());
        let to_nanos = plan.wall_start_nanos.saturating_add(offset);
        let to = Timestamp::from_unix_nanos(to_nanos);
        let window = u64::try_from(config.window.as_nanos()).unwrap_or(u64::MAX);
        let from = Timestamp::from_unix_nanos(to_nanos.saturating_sub(window));
        let path = fill(&class.path, &config.project, &drawn, from, to);
        let body = class
            .body
            .as_deref()
            .map(|body| fill(body, &config.project, &drawn, from, to));
        let intended = plan.start.saturating_add(Duration::from_nanos(offset));
        shared.wait_for_release(index).await;
        let outcome = exchange(&env, &plan, &mut connection, class, &path, body).await;
        let mut stats = shared.stats.lock().unwrap_or_else(PoisonError::into_inner);
        let stats = &mut *stats;
        let class_stats = &mut stats.classes[class_index];
        match outcome {
            Exchange::NotSent { done } => {
                class_stats.errors.connect += 1;
                stats.last_done = stats.last_done.max(Some(done));
            }
            Exchange::Failed {
                sent_at,
                done,
                timed_out,
            } => {
                class_stats.sent += 1;
                if timed_out {
                    class_stats.errors.timeout += 1;
                } else {
                    class_stats.errors.transport += 1;
                }
                stats
                    .lag
                    .saturating_record(micros(sent_at.saturating_duration_since(intended)));
                stats.last_done = stats.last_done.max(Some(done));
            }
            Exchange::Answered {
                sent_at,
                done,
                status,
                ok,
            } => {
                class_stats.sent += 1;
                *class_stats.statuses.entry(status).or_default() += 1;
                stats
                    .lag
                    .saturating_record(micros(sent_at.saturating_duration_since(intended)));
                match ok {
                    Some(true) => {
                        class_stats.ok += 1;
                        let latency = micros(done.saturating_duration_since(intended));
                        class_stats.latency.saturating_record(latency);
                        let half = config.duration.as_nanos() / 2;
                        class_stats.halves[usize::from(u128::from(offset) >= half)]
                            .saturating_record(latency);
                        class_stats
                            .service
                            .saturating_record(micros(done.saturating_duration_since(sent_at)));
                    }
                    Some(false) => class_stats.empty += 1,
                    None => {}
                }
                stats.last_done = stats.last_done.max(Some(done));
            }
        }
    }
}

enum Exchange {
    NotSent {
        done: Instant,
    },
    Failed {
        sent_at: Instant,
        done: Instant,
        timed_out: bool,
    },
    Answered {
        sent_at: Instant,
        done: Instant,
        status: u16,
        /// `None` for a non-2xx answer; for a 2xx answer whether it was non-empty as the class requires.
        ok: Option<bool>,
    },
}

fn request(
    authority: &HeaderValue,
    headers: &[(HeaderName, HeaderValue)],
    method: Method,
    path: &str,
    body: Option<String>,
) -> Option<Request<Full<Bytes>>> {
    let json = body.is_some();
    let mut request = Request::new(Full::new(Bytes::from(body.unwrap_or_default())));
    *request.method_mut() = method;
    *request.uri_mut() = Uri::try_from(path).ok()?;
    let fields = request.headers_mut();
    fields.insert(HOST, authority.clone());
    if json {
        fields.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    for (name, value) in headers {
        fields.append(name.clone(), value.clone());
    }
    Some(request)
}

async fn exchange(
    env: &Env,
    plan: &ReadPlan,
    connection: &mut Option<Connection>,
    class: &ReadClass,
    path: &str,
    body: Option<String>,
) -> Exchange {
    let config = &plan.config;
    let method = if class.method == "POST" {
        Method::POST
    } else {
        Method::GET
    };
    let Some(request) = request(&plan.authority, &config.headers, method, path, body) else {
        return Exchange::NotSent {
            done: env.clock.now(),
        };
    };
    match send(env, config.target, config.timeout, connection, request).await {
        Delivery::NotSent { done } => Exchange::NotSent { done },
        Delivery::Failed {
            sent_at,
            done,
            timed_out,
        } => Exchange::Failed {
            sent_at,
            done,
            timed_out,
        },
        Delivery::Answered {
            sent_at,
            done,
            status,
            body,
        } => {
            let ok = (200..300).contains(&status).then(|| {
                class
                    .nonempty
                    .as_deref()
                    .is_none_or(|pointer| nonempty(&body, pointer).unwrap_or(false))
            });
            Exchange::Answered {
                sent_at,
                done,
                status,
                ok,
            }
        }
    }
}

enum Delivery {
    NotSent {
        done: Instant,
    },
    Failed {
        sent_at: Instant,
        done: Instant,
        timed_out: bool,
    },
    Answered {
        sent_at: Instant,
        done: Instant,
        status: u16,
        body: Bytes,
    },
}

/// Sends `request` on `connection`, opening a new one when it is closed; a failed exchange drops the connection.
async fn send(
    env: &Env,
    target: SocketAddr,
    timeout: Duration,
    connection: &mut Option<Connection>,
    request: Request<Full<Bytes>>,
) -> Delivery {
    if let Some(idle) = connection.as_mut()
        && !matches!(env.timeout(timeout, idle.ready()).await, Ok(Ok(())))
    {
        *connection = None;
    }
    let open = match connection {
        Some(open) => open,
        None => match Connection::open(env, target, timeout).await {
            Ok(open) => connection.insert(open),
            Err(_) => {
                return Delivery::NotSent {
                    done: env.clock.now(),
                };
            }
        },
    };
    let sent_at = env.clock.now();
    match env.timeout(timeout, open.exchange(request)).await {
        Ok(Ok((status, body))) => Delivery::Answered {
            sent_at,
            done: env.clock.now(),
            status: status.as_u16(),
            body,
        },
        failed => {
            *connection = None;
            Delivery::Failed {
                sent_at,
                done: env.clock.now(),
                timed_out: failed.is_err(),
            }
        }
    }
}

/// What the ack-to-API probe sends and where.
#[derive(Clone, Debug)]
pub struct FreshConfig {
    /// The OTLP/HTTP ingest listener and its `Host` header.
    pub ingest: SocketAddr,
    /// See `ingest`.
    pub ingest_authority: String,
    /// The API listener and its `Host` header.
    pub api: SocketAddr,
    /// See `api`.
    pub api_authority: String,
    /// Extra headers on every request: an `Authorization` header with the `ingest` and `read` scopes.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// The project of the key.
    pub project: String,
    /// Traces to send, one at a time.
    pub probes: u32,
    /// From one probe's acknowledgement to the next probe.
    pub interval: Duration,
    /// Between two lists of the traces.
    pub poll: Duration,
    /// The most a probe waits for its trace to be listed, and the deadline of each request.
    pub timeout: Duration,
    /// The content seed of the probes' spans.
    pub seed: u64,
}

/// The JSON report of the ack-to-API probe (schema [`FRESH_SCHEMA`]).
#[derive(Clone, Debug, Serialize)]
pub struct FreshReport {
    /// Always [`FRESH_SCHEMA`].
    pub schema: &'static str,
    /// The run id the probes' spans carry.
    pub run_id: String,
    /// The wall-clock time of the first probe, RFC 3339.
    pub started_at: String,
    /// Probes sent, and probes whose trace the list named within the timeout.
    pub probes: u32,
    /// See `probes`.
    pub listed: u32,
    /// From sending a probe to its acknowledgement.
    pub ack_us: Summary,
    /// From a probe's acknowledgement to the first list that names its trace: what milestone 1 measured.
    pub ack_to_list_us: Summary,
    /// Lists of the traces per listed probe.
    pub lists_per_probe: Summary,
    /// Each list's round trip, from sending it to its complete answer, over the lists answered 200.
    pub list_us: Summary,
    /// Probes whose ingest request failed or was not answered 200.
    pub ingest_errors: u64,
    /// Lists that failed or were not answered 200.
    pub list_errors: u64,
    /// Probes acknowledged but not listed within the timeout.
    pub missing: u64,
}

/// Sends `config.probes` traces one at a time and measures how soon each is listed after its acknowledgement.
pub async fn fresh(env: &Env, config: &FreshConfig) -> Result<FreshReport, ReadError> {
    if config.probes == 0 {
        return Err(RunError::Config("the probe needs at least one trace").into());
    }
    let run_id = env.new_id();
    let pool = Pool::build(
        PoolConfig {
            span_size: SpanSize::Kib1,
            spans_per_request: 2,
            traces_per_request: 1,
            rootless: false,
            templates: 1,
            seed: config.seed,
        },
        run_id,
    )
    .map_err(|_| RunError::Config("the probe's pool"))?;
    let header = |authority: &str| {
        HeaderValue::from_str(authority)
            .map_err(|_| ReadError::from(RunError::Config("a target is not a valid Host header")))
    };
    let (ingest_host, api_host) = (
        header(&config.ingest_authority)?,
        header(&config.api_authority)?,
    );
    let histogram =
        || Histogram::<u64>::new_with_bounds(1, HISTOGRAM_MAX_US, 3).map_err(RunError::from);
    let (mut ack, mut listed_after, mut lists, mut round_trips) =
        (histogram()?, histogram()?, histogram()?, histogram()?);
    let (mut ingest_conn, mut api_conn) = (None, None);
    let (mut listed, mut ingest_errors, mut list_errors, mut missing) = (0, 0, 0, 0);
    let started_at = env.clock.wall_now();
    for probe in 0..config.probes {
        let wall = env.clock.wall_now();
        let now_nanos = u64::try_from(wall.micros())
            .unwrap_or(0)
            .saturating_mul(1_000);
        let body = pool.request(u64::from(probe), now_nanos);
        let trace = pool.trace_id(u64::from(probe), 0).to_string();
        let mut post = request(
            &ingest_host,
            &config.headers,
            Method::POST,
            TRACES_PATH,
            None,
        )
        .ok_or(RunError::Config("the ingest request"))?;
        *post.body_mut() = Full::new(Bytes::from(body));
        post.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-protobuf"),
        );
        let sent = env.clock.now();
        let acked = match send(env, config.ingest, config.timeout, &mut ingest_conn, post).await {
            Delivery::Answered {
                status: 200, done, ..
            } => done,
            _ => {
                ingest_errors += 1;
                env.clock.sleep(config.interval).await;
                continue;
            }
        };
        ack.saturating_record(micros(acked.saturating_duration_since(sent)));
        let margin = 600_000_000;
        let from = Timestamp::from_micros(wall.micros().saturating_sub(margin));
        let to = Timestamp::from_micros(wall.micros().saturating_add(margin));
        let path = format!(
            "/api/v1/projects/{}/traces?from={from}&to={to}&limit=200",
            config.project
        );
        let mut polls = 0;
        let found = loop {
            let get = request(&api_host, &config.headers, Method::GET, &path, None)
                .ok_or(RunError::Config("the list request"))?;
            polls += 1;
            match send(env, config.api, config.timeout, &mut api_conn, get).await {
                Delivery::Answered {
                    status: 200,
                    sent_at,
                    done,
                    body,
                } => {
                    round_trips.saturating_record(micros(done.saturating_duration_since(sent_at)));
                    if contains(&body, trace.as_bytes()) {
                        break Some(done);
                    }
                }
                _ => list_errors += 1,
            }
            if env.clock.now().saturating_duration_since(acked) > config.timeout {
                break None;
            }
            env.clock.sleep(config.poll).await;
        };
        match found {
            Some(done) => {
                listed += 1;
                listed_after.saturating_record(micros(done.saturating_duration_since(acked)));
                lists.saturating_record(polls);
            }
            None => missing += 1,
        }
        env.clock.sleep(config.interval).await;
    }
    Ok(FreshReport {
        schema: FRESH_SCHEMA,
        run_id: run_id.to_string(),
        started_at: started_at.to_string(),
        probes: config.probes,
        listed,
        ack_us: Summary::of(&ack),
        ack_to_list_us: Summary::of(&listed_after),
        lists_per_probe: Summary::of(&lists),
        list_us: Summary::of(&round_trips),
        ingest_errors,
        list_errors,
        missing,
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const RUN: &str = "019a6b2e-7c3d-7e4f-8a5b-6c7d8e9f0a1b";

    fn report(requests: u64, not_acked: &str, spread: f64) -> String {
        format!(
            r#"{{"schema":"crucible-loadgen.run.v1","run_id":"{RUN}","started_at":"2026-11-18T21:30:00.123456Z",
            "lane":{{"spans_per_request":10,"traces_per_request":2,"spread_s":{spread}}},
            "schedule":{{"target_rps":100,"requests":{requests}}},"not_acked_requests":{not_acked}}}"#
        )
    }

    #[test]
    fn datasets_hold_only_acknowledged_requests() {
        let dataset = Dataset::from_reports([report(10, "[[2,4],[9,10]]", 0.0).as_str()]).unwrap();
        assert_eq!((dataset.runs(), dataset.acked_requests()), (1, 7));
        assert!(matches!(
            Dataset::from_reports([report(3, "[[0,3]]", 0.0).as_str()]),
            Err(ReadError::Empty)
        ));
        assert!(Dataset::from_reports(["{}"]).is_err());
        // Every draw names a span of an acknowledged request, in the trace it belongs to.
        let identity = Identity::new(RUN.parse().unwrap(), 10, 2);
        let rng = ChaChaRng::from_u64(7);
        let acked: Vec<u64> = vec![0, 1, 4, 5, 6, 7, 8];
        for _ in 0..500 {
            let drawn = dataset.draw(&rng);
            let found = acked.iter().any(|&index| {
                (0..2).any(|trace| {
                    identity.trace_id(index, trace).to_string() == drawn.trace_id
                        && identity.positions(trace).any(|p| {
                            Pool::span_id(identity.seq(index, p)).to_string() == drawn.span_id
                        })
                        && Pool::span_id(identity.seq(index, identity.positions(trace).start))
                            .to_string()
                            == drawn.root_span_id
                })
            });
            assert!(found, "{drawn:?}");
            assert!(drawn.span_start >= drawn.trace_start);
        }
    }

    #[test]
    fn start_times_follow_the_run_its_spread_and_its_backdate() {
        // The one trace of request 0 starts at the run's start, the backdate before it, or with a spread, the
        // spread before that.
        let start = Timestamp::parse_rfc3339("2026-11-18T21:30:00.123456Z").unwrap();
        for (spread, backdate, expected) in [
            (0, 0, 0),
            (0, 60, -60),
            (3_600, 0, -3_600),
            (3_600, 86_400, -90_000),
        ] {
            let report = format!(
                r#"{{"run_id":"{RUN}","started_at":"2026-11-18T21:30:00.123456Z","lane":{{"spans_per_request":4,
                "spread_s":{spread},"backdate_s":{backdate}}},"schedule":{{"target_rps":10,"requests":1}},
                "not_acked_requests":[]}}"#
            );
            let dataset = Dataset::from_reports([report.as_str()]).unwrap();
            let drawn = dataset.draw(&ChaChaRng::from_u64(1));
            assert_eq!(
                drawn.trace_start.micros() - start.micros(),
                expected * 1_000_000
            );
        }
    }

    #[test]
    fn placeholders_are_filled() {
        let drawn = Drawn {
            trace_id: "ab".repeat(16),
            root_span_id: "01".repeat(8),
            span_id: "02".repeat(8),
            span_start: Timestamp::from_micros(1_000_000),
            trace_start: Timestamp::from_micros(0),
        };
        let filled = fill(
            "/p/{project}/t/{trace_id}?r={root_span_id}&s={span_id}&at={span_start}&h={trace_start}&from={from}&to={to}",
            "proj",
            &drawn,
            Timestamp::from_micros(0),
            Timestamp::from_micros(2_000_000),
        );
        assert_eq!(
            filled,
            format!(
                "/p/proj/t/{}?r={}&s={}&at=1970-01-01T00:00:01.000000Z&h=1970-01-01T00:00:00.000000Z\
                 &from=1970-01-01T00:00:00.000000Z&to=1970-01-01T00:00:02.000000Z",
                "ab".repeat(16),
                "01".repeat(8),
                "02".repeat(8)
            )
        );
        assert_eq!(nonempty(br#"{"data":[1]}"#, "/data"), Some(true));
        assert_eq!(nonempty(br#"{"data":[]}"#, "/data"), Some(false));
        assert_eq!(nonempty(br#"{"spans":{}}"#, "/data"), Some(false));
        assert_eq!(nonempty(b"not json", "/data"), None);
    }

    proptest! {
        /// Due offsets never go backwards; fixed think times are exact and exponential ones keep their mean.
        #[test]
        fn think_times_keep_their_rate(rate in 1.0f64..500.0, seed: u64) {
            let fixed = due_offsets(200, rate, Think::Fixed, seed);
            prop_assert_eq!(fixed[100], (100.0 * 1e9 / rate) as u64);
            let poisson = due_offsets(2_000, rate, Think::Exponential, seed);
            prop_assert!(poisson.windows(2).all(|w| w[0] <= w[1]));
            prop_assert_eq!(poisson[0], 0);
            // 2,000 exponential gaps: the mean is within 10% of 1/rate with overwhelming probability.
            let mean = poisson[1_999] as f64 / 1_999.0;
            prop_assert!((mean * rate / 1e9 - 1.0).abs() < 0.1, "{}", mean * rate / 1e9);
            prop_assert_eq!(&poisson, &due_offsets(2_000, rate, Think::Exponential, seed));
        }
    }
}
