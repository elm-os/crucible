//! The telemetry task: [`ProxyCore`] run on its own task, off the forwarding path, with the OTLP exporter.
//!
//! The pumps hold a [`LiveTap`]. Forwarding a chunk takes one short lock to queue the copy and wakes the task;
//! it never waits for analysis or for the collector. The task analyses what is queued, sends batches to the
//! collector as `POST /v1/traces` in protobuf with the project's secret key, and backs off while the collector
//! is down. An outage therefore costs telemetry (dropped oldest first, and counted), never the MCP session.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Clock, ClockExt as _, Env, JoinHandle};
use crucible_records::Direction;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, USER_AGENT};
use http::{Method, Request};
use http_body_util::Full;
use tokio::sync::Notify;

use crate::core::{
    Batch, BodyKind, EndReason, ExportResult, HeaderView, Now, ProxyCore, Stats, StreamId, Tap,
};
use crate::encode::{ResourceInfo, encode};
use crate::net::{RequestError, Target, request_once};

/// The exit flush deadline of the contract.
pub const FLUSH_DEADLINE: Duration = Duration::from_millis(500);

/// Where and how to export.
#[derive(Clone)]
pub struct ExportConfig {
    /// The Crucible endpoint; `/v1/traces` is appended to its path.
    pub endpoint: Target,
    /// The project's secret key, sent as a Bearer token. It needs the `ingest` scope.
    pub secret_key: String,
    /// The longest one export may take.
    pub timeout: Duration,
    /// What every span shares.
    pub resource: ResourceInfo,
}

impl std::fmt::Debug for ExportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never reaches a log.
        f.debug_struct("ExportConfig")
            .field("endpoint", &self.endpoint)
            .field("timeout", &self.timeout)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

/// The forwarding face handed to the pumps. Without telemetry it forwards and does nothing else.
#[derive(Clone, Debug)]
pub struct LiveTap {
    inner: Option<(Tap, Arc<Notify>)>,
    clock: Arc<dyn Clock>,
}

impl LiveTap {
    /// A tap that only forwards: the proxy runs without a collector.
    pub fn disabled(env: &Env) -> Self {
        Self {
            inner: None,
            clock: Arc::clone(&env.clock),
        }
    }

    fn now(&self) -> Now {
        Now::read(self.clock.as_ref())
    }

    /// See [`Tap::forward`]: returns the bytes to write, which are the bytes given.
    pub fn forward(&self, stream: StreamId, direction: Direction, bytes: Bytes) -> Bytes {
        match &self.inner {
            Some((tap, wake)) => {
                let out = tap.forward(stream, direction, bytes, self.now());
                wake.notify_one();
                out
            }
            None => bytes,
        }
    }

    /// See [`Tap::open`].
    pub fn open(&self, stream: StreamId, view: HeaderView) {
        if let Some((tap, wake)) = &self.inner {
            tap.open(stream, view, self.now());
            wake.notify_one();
        }
    }

    /// See [`Tap::head`].
    pub fn head(&self, stream: StreamId, status: u16, body: BodyKind, session_key: Option<String>) {
        if let Some((tap, wake)) = &self.inner {
            tap.head(stream, status, body, session_key, self.now());
            wake.notify_one();
        }
    }

    /// See [`Tap::close`].
    pub fn close(&self, stream: StreamId, reason: EndReason) {
        if let Some((tap, wake)) = &self.inner {
            tap.close(stream, reason, self.now());
            wake.notify_one();
        }
    }

    /// See [`Tap::session_end`].
    pub fn session_end(&self) {
        if let Some((tap, wake)) = &self.inner {
            tap.session_end(self.now());
            wake.notify_one();
        }
    }
}

/// The running telemetry task.
#[derive(Debug)]
pub struct Telemetry {
    tap: LiveTap,
    stop: Arc<Notify>,
    task: Option<JoinHandle<Stats>>,
    clock: Arc<dyn Clock>,
}

impl Telemetry {
    /// Starts the task for `core`, exporting to `export`.
    pub fn start(env: &Env, core: ProxyCore, export: ExportConfig) -> Self {
        let wake = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());
        let tap = LiveTap {
            inner: Some((core.tap(), Arc::clone(&wake))),
            clock: Arc::clone(&env.clock),
        };
        let task = env.spawn(
            "proxy-telemetry",
            run(env.clone(), core, export, wake, Arc::clone(&stop)),
        );
        Self {
            tap,
            stop,
            task: Some(task),
            clock: Arc::clone(&env.clock),
        }
    }

    /// No task: the proxy only forwards.
    pub fn disabled(env: &Env) -> Self {
        Self {
            tap: LiveTap::disabled(env),
            stop: Arc::new(Notify::new()),
            task: None,
            clock: Arc::clone(&env.clock),
        }
    }

    /// The forwarding face.
    pub fn tap(&self) -> LiveTap {
        self.tap.clone()
    }

    /// Asks the task to analyse what is queued and export what is buffered, and waits at most
    /// [`FLUSH_DEADLINE`] for it. Returns the final counters when the task finished in time.
    pub async fn finish(mut self) -> Option<Stats> {
        let mut task = self.task.take()?;
        self.stop.notify_one();
        match self.clock.timeout(FLUSH_DEADLINE, task.join()).await {
            Ok(stats) => stats.ok(),
            Err(_elapsed) => {
                task.abort();
                None
            }
        }
    }
}

/// Sends one batch.
async fn export(env: Env, config: ExportConfig, batch: Batch) -> (Batch, ExportResult) {
    let body = encode(&config.resource, &batch.ops);
    let authorization = HeaderValue::from_str(&format!("Bearer {}", config.secret_key));
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("{}/v1/traces", config.endpoint.path))
        .header(CONTENT_TYPE, "application/x-protobuf")
        .header(
            USER_AGENT,
            concat!("crucible-proxy/", env!("CARGO_PKG_VERSION")),
        )
        .body(Full::new(Bytes::from(body)));
    let (Ok(authorization), Ok(mut request)) = (authorization, request) else {
        // A key that is no header value can never be accepted.
        tracing::warn!("the secret key is not a valid header value; telemetry is dropped");
        return (batch, ExportResult::Rejected);
    };
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let result = match request_once(&env, &config.endpoint, request, 64 * 1024, config.timeout)
        .await
    {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                ExportResult::Delivered
            } else if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
                tracing::debug!(status = status.as_u16(), "the collector asked to retry");
                ExportResult::Retry
            } else {
                tracing::warn!(
                    status = status.as_u16(),
                    "the collector refused the export; check the endpoint and that the secret key has the \
                     ingest scope"
                );
                ExportResult::Rejected
            }
        }
        Err(error) => {
            log_failure(&error);
            ExportResult::Retry
        }
    };
    (batch, result)
}

fn log_failure(error: &RequestError) {
    tracing::debug!(%error, "the collector could not be reached; the export will be retried");
}

async fn run(
    env: Env,
    mut core: ProxyCore,
    config: ExportConfig,
    wake: Arc<Notify>,
    stop: Arc<Notify>,
) -> Stats {
    let mut in_flight: Option<BoxFuture<'static, (Batch, ExportResult)>> = None;
    let mut stopping = false;
    let mut reported_content_drops = 0;
    loop {
        core.drain();
        let now = env.clock.now();
        if in_flight.is_none()
            && let Some(batch) = core.next_export(now, stopping)
        {
            in_flight = Some(export(env.clone(), config.clone(), batch).boxed());
        }
        if stopping && in_flight.is_none() {
            // Whatever is left cannot be sent now (the collector is backing off): it is dropped with the process.
            return core.stats();
        }
        let timer = match core.next_export_at() {
            Some(at) => env.clock.sleep_until(at),
            None => futures::future::pending().boxed(),
        };
        let exporting = async {
            match in_flight.as_mut() {
                Some(export) => export.await,
                None => futures::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = stop.notified(), if !stopping => stopping = true,
            (batch, result) = exporting => {
                in_flight = None;
                let dropped = core.export_done(batch, result, env.clock.now());
                if dropped > 0 {
                    tracing::warn!(
                        dropped,
                        "operations were dropped while the collector was unreachable or the buffer was full"
                    );
                }
                let content_drops = core.stats().dropped_content;
                if content_drops > reported_content_drops {
                    tracing::warn!(
                        dropped = content_drops - reported_content_drops,
                        "captured calls were recorded without their content: the arguments of the calls waiting \
                         for a response reached their 8 MiB bound"
                    );
                    reported_content_drops = content_drops;
                }
            }
            () = wake.notified() => {}
            () = timer => {}
        }
    }
}
