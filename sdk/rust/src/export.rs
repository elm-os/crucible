//! The export pipeline: a span processor that puts finished spans on a bounded queue without blocking, and one
//! worker thread that exports the queue in batches.
//!
//! The worker runs a current-thread runtime of its own, so exporting never shares a thread with the
//! application, and the SDK works whether or not the application runs an async runtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::trace::Status;
use opentelemetry::{Array, Context, KeyValue, Value};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanExporter as _, SpanProcessor};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use crate::agent::TraceAttrs;
use crate::client::{Config, FlushError, Stats};
use crate::keys;

/// The most spans one export request carries.
const BATCH_SIZE: usize = 512;

type Reply = Option<oneshot::Sender<Result<(), FlushError>>>;

#[derive(Debug)]
enum Command {
    Flush(Reply),
    Shutdown(Reply),
}

#[derive(Debug, Default)]
struct Counters {
    exported: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    /// The estimated bytes of the queued spans and of the batch being exported (see [`span_bytes`]).
    queued_bytes: AtomicU64,
}

impl Counters {
    fn add(counter: &AtomicU64, n: usize) {
        counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

/// What a span costs beyond its strings: ids, times, kind, status code, links and the resource and scope refs.
const SPAN_OVERHEAD_BYTES: usize = 256;

/// A span's size, estimated from lengths already known (its name, attributes, events and status message) plus a
/// fixed allowance; the attribute count limit bounds the work.
fn span_bytes(span: &SpanData) -> u64 {
    let status = match &span.status {
        Status::Error { description } => description.len(),
        _ => 0,
    };
    let events: usize = span
        .events
        .iter()
        .map(|event| event.name.len() + attribute_bytes(&event.attributes))
        .sum();
    let bytes =
        SPAN_OVERHEAD_BYTES + span.name.len() + status + attribute_bytes(&span.attributes) + events;
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn attribute_bytes(attributes: &[KeyValue]) -> usize {
    attributes
        .iter()
        .map(|kv| {
            kv.key.as_str().len()
                + match &kv.value {
                    Value::String(s) => s.as_str().len(),
                    Value::Array(Array::String(items)) => {
                        items.iter().map(|s| s.as_str().len()).sum()
                    }
                    _ => 8,
                }
        })
        .sum()
}

/// Gives a batch's bytes back to the queue's count when its export ends, finished or abandoned at a deadline.
struct Release {
    counters: Arc<Counters>,
    bytes: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.counters
            .queued_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// The SDK's span processor. At span start it copies the session, user, tags and metadata that the parent
/// context carries onto the span (contract 3.3: every span carries them). At span end it queues the span, or
/// drops and counts it when the queue is full.
#[derive(Debug)]
pub(crate) struct Processor {
    queue: mpsc::Sender<(SpanData, u64)>,
    commands: mpsc::UnboundedSender<Command>,
    release: Option<KeyValue>,
    counters: Arc<Counters>,
}

/// The handle behind `flush`, `shutdown` and `stats`.
#[derive(Debug)]
pub(crate) struct Pipeline {
    commands: mpsc::UnboundedSender<Command>,
    counters: Arc<Counters>,
}

/// Starts the worker thread and returns the processor that feeds it and the handle that drives it.
pub(crate) fn start(
    mut exporter: opentelemetry_otlp::SpanExporter,
    config: &Config,
) -> Result<(Processor, Pipeline), String> {
    exporter.set_resource(&config.resource);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("the export runtime: {e}"))?;
    let (queue, spans) = mpsc::channel(config.queue_size);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let counters = Arc::new(Counters::default());
    let worker = Worker {
        exporter,
        spans,
        commands: command_rx,
        counters: counters.clone(),
        interval: config.flush_interval,
        timeout: config.export_timeout,
        batch: Vec::new(),
        batch_bytes: 0,
    };
    std::thread::Builder::new()
        .name("crucible-export".to_owned())
        .spawn(move || runtime.block_on(worker.run()))
        .map_err(|e| format!("the export thread: {e}"))?;
    let release =
        (!config.release.is_empty()).then(|| KeyValue::new(keys::RELEASE, config.release.clone()));
    Ok((
        Processor {
            queue,
            commands: commands.clone(),
            release,
            counters: counters.clone(),
        },
        Pipeline { commands, counters },
    ))
}

impl SpanProcessor for Processor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        if let Some(attrs) = cx.get::<TraceAttrs>() {
            attrs.apply(span);
        }
        if let Some(release) = &self.release {
            opentelemetry::trace::Span::set_attribute(span, release.clone());
        }
    }

    fn on_end(&self, span: SpanData) {
        let size = span_bytes(&span);
        self.counters
            .queued_bytes
            .fetch_add(size, Ordering::Relaxed);
        if self.queue.try_send((span, size)).is_err() {
            self.counters
                .queued_bytes
                .fetch_sub(size, Ordering::Relaxed);
            Counters::add(&self.counters.dropped, 1);
        }
    }

    // The provider calls these from synchronous code, possibly on a runtime thread, so they only signal the
    // worker and never wait. `crucible_sdk::flush` and `shutdown` are the waiting forms.
    fn force_flush(&self) -> OTelSdkResult {
        let _sent = self.commands.send(Command::Flush(None));
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        let _sent = self.commands.send(Command::Shutdown(None));
        Ok(())
    }
}

impl Pipeline {
    pub(crate) async fn flush(&self) -> Result<(), FlushError> {
        self.request(Command::Flush).await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), FlushError> {
        self.request(Command::Shutdown).await
    }

    async fn request(&self, command: fn(Reply) -> Command) -> Result<(), FlushError> {
        let (reply, done) = oneshot::channel();
        if self.commands.send(command(Some(reply))).is_err() {
            return Err(FlushError("the exporter has stopped".to_owned()));
        }
        done.await
            .unwrap_or_else(|_| Err(FlushError("the exporter has stopped".to_owned())))
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            exported: self.counters.exported.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            failed: self.counters.failed.load(Ordering::Relaxed),
            queued_bytes: self.counters.queued_bytes.load(Ordering::Relaxed),
        }
    }
}

struct Worker {
    exporter: opentelemetry_otlp::SpanExporter,
    spans: mpsc::Receiver<(SpanData, u64)>,
    commands: mpsc::UnboundedReceiver<Command>,
    counters: Arc<Counters>,
    interval: Duration,
    timeout: Duration,
    batch: Vec<SpanData>,
    batch_bytes: u64,
}

impl Worker {
    async fn run(mut self) {
        // The first tick comes one interval after start: spans leave on a tick, a full batch, flush or shutdown.
        let mut ticker = tokio::time::interval_at(Instant::now() + self.interval, self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    let (reply, stop) = match command {
                        Some(Command::Flush(reply)) => (reply, false),
                        Some(Command::Shutdown(reply)) => (reply, true),
                        None => (None, true),
                    };
                    let result = self.drain().await;
                    if stop {
                        let _shut = self.exporter.shutdown();
                    }
                    if let Some(reply) = reply {
                        let _sent = reply.send(result);
                    }
                    if stop {
                        return;
                    }
                }
                span = self.spans.recv() => match span {
                    Some((span, size)) => {
                        self.batch.push(span);
                        self.batch_bytes += size;
                        if self.batch.len() >= BATCH_SIZE {
                            let _result = self.export().await;
                        }
                    }
                    // The processor is gone: export what is left and stop.
                    None => {
                        let _result = self.drain().await;
                        return;
                    }
                },
                _ = ticker.tick() => {
                    let _result = self.export().await;
                }
            }
        }
    }

    /// Exports every span queued when the drain starts, within the export timeout. Spans queued while it runs
    /// wait for the next export.
    async fn drain(&mut self) -> Result<(), FlushError> {
        let deadline = Instant::now() + self.timeout;
        let mut pending = self.spans.len();
        let mut result = Ok(());
        loop {
            while pending > 0 && self.batch.len() < BATCH_SIZE {
                match self.spans.try_recv() {
                    Ok((span, size)) => {
                        self.batch.push(span);
                        self.batch_bytes += size;
                    }
                    Err(_) => break,
                }
                pending -= 1;
            }
            if self.batch.is_empty() {
                return result;
            }
            let size = self.batch.len();
            match tokio::time::timeout_at(deadline, self.export()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(error),
                Err(_) => {
                    Counters::add(&self.counters.failed, size);
                    return Err(FlushError(format!(
                        "the export did not finish within {:?}; raise Options::export_timeout if the server is slow",
                        self.timeout
                    )));
                }
            }
        }
    }

    async fn export(&mut self) -> Result<(), FlushError> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let _release = Release {
            counters: self.counters.clone(),
            bytes: std::mem::take(&mut self.batch_bytes),
        };
        let size = batch.len();
        match self.exporter.export(batch).await {
            Ok(()) => {
                Counters::add(&self.counters.exported, size);
                Ok(())
            }
            Err(error) => {
                Counters::add(&self.counters.failed, size);
                Err(FlushError(format!("the export failed: {error}")))
            }
        }
    }
}
