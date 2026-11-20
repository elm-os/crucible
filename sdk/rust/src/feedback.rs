//! Feedback (contract 12.6): a score with source `feedback` on a trace, a span or a session, posted to
//! `POST {api_url}/api/v1/projects/{project_id}/feedback` with the project secret key, which needs the `write`
//! scope (contract 11.3).
//!
//! [`feedback`] only queues the score, so the request leaves the caller's path as spans do (contract 9.10). One
//! worker thread posts the queue in call order, one request at a time, because the latest score of a name wins.
//! Before it posts, it flushes the span queue: the route answers 404 for a trace Crucible has not received, so the
//! spans that ended before the call go first. A failed connection, a timeout, 408, 429, 5xx and a `conflict` 409
//! are retried, three attempts in all, after `Retry-After` (at most 10 s) or else 1 s and 2 s, with the same body
//! and `Idempotency-Key`, so Crucible writes the score once. Nothing reaches the caller: a score that cannot be
//! queued counts as dropped, one that Crucible refuses or that fails every attempt as failed, and
//! [`flush`](crate::flush) returns why the last score that was not sent since the previous flush was dropped or
//! failed, with what to change.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use http::header::{CONTENT_TYPE, RETRY_AFTER, USER_AGENT};
use http::{Request, StatusCode};
use opentelemetry_http::hyper::HyperClient;
use opentelemetry_http::{Bytes, HttpClient as _};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::client::{Connector, VERSION};
use crate::export::Pipeline;

/// Scores waiting to be sent, the one being sent included.
const QUEUE_LIMIT: u64 = 1024;
const ATTEMPTS: usize = 3;
/// The waits before the second and third attempts when the answer has no `Retry-After`.
const BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];
const RETRY_AFTER_CAP: Duration = Duration::from_secs(10);
const SETTINGS: &str = "scores go to Options::api_url, the API listener, under Options::project_id, the key's project, with a secret key that has the write scope";

/// A score on a trace (`trace_id`), a span (`trace_id` and `span_id`) or a session (`session_id`). Empty strings
/// are left out of the request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feedback {
    /// The score name, for example `helpful`, without `#`. A later score of the same name (and `score_id`)
    /// replaces it.
    pub name: String,
    /// A number (`numeric`), a boolean (`boolean`) or a string (`categorical`, or `text` with `data_type`).
    pub value: ScoreValue,
    /// The trace, 32 hex characters.
    pub trace_id: String,
    /// A span of the trace, 16 hex characters.
    pub span_id: String,
    /// A session, instead of a trace.
    pub session_id: String,
    /// Keeps scores of one name apart, for example one per reviewer.
    pub score_id: String,
    /// How Crucible reads `value`: `numeric`, `boolean`, `categorical` or `text`. When empty, it follows the value.
    pub data_type: String,
    /// At most 4 KiB.
    pub comment: String,
    /// A JSON object, at most 16 KiB.
    pub metadata: Option<Map<String, Value>>,
    /// Removes the score of this name (and `score_id`) from the target.
    pub retracted: bool,
    /// Sent as `Idempotency-Key`: 1 to 128 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`. Crucible writes a score
    /// once per key and body for 24 hours. When empty, each call gets a random key.
    pub idempotency_key: String,
}

/// A score's value.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ScoreValue {
    /// A number.
    Number(f64),
    /// A boolean.
    Bool(bool),
    /// A string.
    Text(String),
}

impl Default for ScoreValue {
    fn default() -> Self {
        Self::Number(0.0)
    }
}

impl From<f64> for ScoreValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<i32> for ScoreValue {
    fn from(value: i32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<bool> for ScoreValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for ScoreValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for ScoreValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

/// The route's request body.
#[derive(Serialize)]
struct Body<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    trace_id: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    span_id: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    session_id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    score_id: &'a str,
    value: &'a ScoreValue,
    #[serde(skip_serializing_if = "str::is_empty")]
    data_type: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    comment: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a Map<String, Value>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    retracted: bool,
}

#[derive(Debug)]
struct Item {
    seq: u64,
    body: Bytes,
    key: String,
}

#[derive(Debug)]
enum Message {
    Score(Item),
    /// Answered once everything queued before it is handled.
    Flush(oneshot::Sender<()>),
    /// Answered when the worker stops.
    Stop(oneshot::Sender<()>),
}

#[derive(Debug, Default)]
struct Counters {
    sent: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    /// Queued scores, the one being sent included.
    pending: AtomicU64,
    /// The sequence number of the latest queued score.
    seq: AtomicU64,
    /// Why the last score that was not sent since the previous flush was dropped or failed.
    failure: Mutex<Option<String>>,
}

impl Counters {
    fn fail(&self, reason: String) {
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    }

    fn take_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// The queue's handle: what [`feedback`], `flush`, `shutdown` and `stats` use.
#[derive(Debug)]
pub(crate) struct Sender {
    /// `None` without a project id.
    url: Option<String>,
    messages: mpsc::UnboundedSender<Message>,
    counters: Arc<Counters>,
    closed: AtomicBool,
    /// Set when a shutdown runs out of time: it ends the request in flight and the waits between attempts.
    abandon: watch::Sender<bool>,
    timeout: Duration,
}

/// Starts the worker thread for the feedback route `url`.
pub(crate) fn start(
    url: Option<String>,
    connector: Connector,
    authorization: http::HeaderValue,
    pipeline: Pipeline,
    timeout: Duration,
) -> Result<Sender, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("the feedback runtime: {e}"))?;
    let (messages, inbox) = mpsc::unbounded_channel();
    let (abandon, abandoned) = watch::channel(false);
    let counters = Arc::new(Counters::default());
    let worker = Worker {
        url: url.clone().unwrap_or_default(),
        client: HyperClient::new(connector, timeout, Some(authorization)),
        inbox,
        abandoned,
        pipeline,
        counters: counters.clone(),
        timeout,
    };
    std::thread::Builder::new()
        .name("crucible-feedback".to_owned())
        .spawn(move || runtime.block_on(worker.run()))
        .map_err(|e| format!("the feedback thread: {e}"))?;
    Ok(Sender {
        url,
        messages,
        counters,
        closed: AtomicBool::new(false),
        abandon,
        timeout,
    })
}

impl Sender {
    /// Queues one score, or drops and counts it. It never blocks.
    pub(crate) fn add(&self, feedback: &Feedback) {
        let counters = &self.counters;
        let body = serde_json::to_vec(&Body {
            trace_id: &feedback.trace_id,
            span_id: &feedback.span_id,
            session_id: &feedback.session_id,
            name: &feedback.name,
            score_id: &feedback.score_id,
            value: &feedback.value,
            data_type: &feedback.data_type,
            comment: &feedback.comment,
            metadata: feedback.metadata.as_ref(),
            retracted: feedback.retracted,
        });
        if self.url.is_none() {
            counters.dropped.fetch_add(1, Ordering::Relaxed);
            counters.fail(
                "a score was dropped because Options::project_id is empty; set it to the key's project (crucible admin create-key prints it as project_id)".to_owned(),
            );
            return;
        }
        let open = !self.closed.load(Ordering::Acquire);
        let body = match body {
            Ok(body) if open && self.reserve() => Bytes::from(body),
            _ => {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let key = if feedback.idempotency_key.is_empty() {
            random_key()
        } else {
            feedback.idempotency_key.clone()
        };
        let seq = counters.seq.fetch_add(1, Ordering::AcqRel) + 1;
        if self
            .messages
            .send(Message::Score(Item { seq, body, key }))
            .is_err()
        {
            counters.pending.fetch_sub(1, Ordering::AcqRel);
            counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Takes a place in the queue, unless it holds [`QUEUE_LIMIT`] scores.
    fn reserve(&self) -> bool {
        self.counters
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                (pending < QUEUE_LIMIT).then_some(pending + 1)
            })
            .is_ok()
    }

    /// Waits until every score queued before the call is handled, at most the export timeout. Returns why the
    /// last score that was not sent since the previous flush was dropped or failed, or that the wait ran out.
    pub(crate) async fn flush(&self) -> Result<(), String> {
        let handled = self.request(Message::Flush, self.timeout).await;
        if let Some(reason) = self.counters.take_failure() {
            return Err(reason);
        }
        if handled {
            Ok(())
        } else {
            Err(format!(
                "the queued scores were not all sent within {:?}; raise Options::export_timeout if the server is slow",
                self.timeout
            ))
        }
    }

    /// Stops taking scores, waits up to `budget` for the queued ones and stops the worker; what is left then is
    /// counted as dropped.
    pub(crate) async fn shutdown(&self, budget: Duration) {
        self.closed.store(true, Ordering::Release);
        if !self.request(Message::Stop, budget).await {
            let _sent = self.abandon.send(true);
        }
    }

    async fn request(&self, message: fn(oneshot::Sender<()>) -> Message, wait: Duration) -> bool {
        let (reply, done) = oneshot::channel();
        if self.messages.send(message(reply)).is_err() {
            return true;
        }
        tokio::time::timeout(wait, done).await.is_ok()
    }

    /// Sent, dropped and failed scores.
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        let c = &self.counters;
        (
            c.sent.load(Ordering::Relaxed),
            c.dropped.load(Ordering::Relaxed),
            c.failed.load(Ordering::Relaxed),
        )
    }
}

/// How one attempt ended, and why it was not accepted.
enum Outcome {
    Sent,
    Refused(String),
    /// Worth another attempt, after `Retry-After` when the answer had one.
    Retry(String, Option<Duration>),
}

struct Worker {
    url: String,
    client: HyperClient<Connector>,
    inbox: mpsc::UnboundedReceiver<Message>,
    abandoned: watch::Receiver<bool>,
    pipeline: Pipeline,
    counters: Arc<Counters>,
    timeout: Duration,
}

impl Worker {
    async fn run(mut self) {
        let mut flushed = 0;
        while let Some(message) = self.inbox.recv().await {
            let item = match message {
                Message::Score(item) => item,
                Message::Flush(reply) => {
                    let _sent = reply.send(());
                    continue;
                }
                Message::Stop(reply) => {
                    let _sent = reply.send(());
                    return;
                }
            };
            let counters = self.counters.clone();
            let outcome = if *self.abandoned.borrow() {
                &counters.dropped
            } else {
                if item.seq > flushed {
                    flushed = counters.seq.load(Ordering::Acquire);
                    let _flushed = tokio::time::timeout(self.timeout, self.pipeline.flush()).await;
                }
                match self.post(&item).await {
                    Ok(()) => &counters.sent,
                    Err(_) if *self.abandoned.borrow() => &counters.dropped,
                    Err(reason) => {
                        counters.fail(format!("a score was not sent: {reason}; {SETTINGS}"));
                        &counters.failed
                    }
                }
            };
            outcome.fetch_add(1, Ordering::Relaxed);
            counters.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Sends one score, retrying what may succeed later; `Err` says why Crucible did not accept it.
    async fn post(&mut self, item: &Item) -> Result<(), String> {
        let abandoned_reason = || "the shutdown ran out of time".to_owned();
        let mut wait = Duration::ZERO;
        let mut reason = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 && !self.pause(wait).await {
                return Err(abandoned_reason());
            }
            let mut abandoned = self.abandoned.clone();
            let outcome = tokio::select! {
                biased;
                _ = abandoned.wait_for(|stop| *stop) => return Err(abandoned_reason()),
                outcome = self.request(item) => outcome,
            };
            match outcome {
                Outcome::Sent => return Ok(()),
                Outcome::Refused(why) => return Err(why),
                Outcome::Retry(why, after) => {
                    reason = why;
                    wait = after.unwrap_or(BACKOFF[attempt.min(BACKOFF.len() - 1)]);
                }
            }
        }
        Err(reason)
    }

    /// Waits `wait`; false when a shutdown abandoned the queue first.
    async fn pause(&mut self, wait: Duration) -> bool {
        let mut abandoned = self.abandoned.clone();
        tokio::select! {
            biased;
            _ = abandoned.wait_for(|stop| *stop) => false,
            () = tokio::time::sleep_until(Instant::now() + wait) => true,
        }
    }

    async fn request(&self, item: &Item) -> Outcome {
        let request = Request::post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header("idempotency-key", &item.key)
            .header(USER_AGENT, format!("crucible-sdk-rust/{VERSION}"))
            .body(item.body.clone());
        let Ok(request) = request else {
            // An Idempotency-Key that is not a header value can never be sent.
            return Outcome::Refused(format!(
                "the Idempotency-Key {:?} is not a header value",
                item.key
            ));
        };
        let response = match self.client.send_bytes(request).await {
            Ok(response) => response,
            Err(error) => {
                return Outcome::Retry(
                    format!("{} could not be reached ({error})", self.url),
                    None,
                );
            }
        };
        let status = response.status();
        if status.is_success() {
            return Outcome::Sent;
        }
        let why = format!(
            "{} answered {}{}",
            self.url,
            status.as_u16(),
            detail(response.body())
        );
        if !retryable(status, response.body()) {
            return Outcome::Refused(why);
        }
        let after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(retry_after);
        Outcome::Retry(why, after)
    }
}

/// Whether an answer may change on a later attempt: a timeout, throttling, a server fault or a key whose first
/// request is still running (a `conflict` 409; `idempotency_key_reused` never changes).
fn retryable(status: StatusCode, body: &[u8]) -> bool {
    if status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        return true;
    }
    status == StatusCode::CONFLICT
        && serde_json::from_slice::<Value>(body)
            .ok()
            .is_some_and(|problem| problem.get("code") == Some(&Value::from("conflict")))
}

/// The `code` and `detail` of a problem body, for the failure reason.
fn detail(body: &[u8]) -> String {
    let Ok(problem) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let parts: Vec<&str> = ["code", "detail"]
        .iter()
        .filter_map(|key| problem.get(key).and_then(Value::as_str))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(": "))
    }
}

fn retry_after(header: &str) -> Option<Duration> {
    let seconds: f64 = header.trim().parse().ok()?;
    (seconds >= 0.0).then(|| Duration::from_secs_f64(seconds.min(RETRY_AFTER_CAP.as_secs_f64())))
}

/// 16 random bytes as 32 lowercase hex characters.
fn random_key() -> String {
    use opentelemetry_sdk::trace::{IdGenerator as _, RandomIdGenerator};
    let bytes = RandomIdGenerator::default().new_trace_id().to_bytes();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Queues one score for the project's feedback route and returns at once; it never blocks or panics. Scores are
/// sent in call order by a worker thread, each after the spans that ended before it, and retried with the same
/// `Idempotency-Key` when the request may succeed later. Without `Options::project_id`, or when 1,024 scores are
/// already waiting, the score is dropped; [`stats`](crate::stats) counts sent, dropped and failed scores. Before
/// [`init`](crate::init) and after [`shutdown`](crate::shutdown) it does nothing.
pub fn feedback(feedback: Feedback) {
    if let Some(client) = crate::client::active() {
        client.feedback.add(&feedback);
    }
}
