//! One MCP operation: its SERVER span from the moment the wrapper sees the message to the moment the handler
//! returns, with the keys of contract 9.1.

use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use opentelemetry::trace::{SpanBuilder, SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, Extensions, JsonObject,
};
use serde::Serialize;

use super::{CaptureRule, Config, Field, Identity, OpHandle, OpState};
use crate::client::Client;
use crate::keys::mcp as k;
use crate::mcp_common::{self, Annotations, Outcome, Transport};
use crate::scrub;

/// The cap on an error message (`error_message`, contract 3.5).
const MAX_ERROR_MESSAGE: usize = 2 * 1024;
/// The cap on a handle id (contract 9.8).
const MAX_HANDLE_ID: usize = 256;

/// What the wrapper knows about an operation before its handler runs.
pub(super) struct Start<'a> {
    pub(super) method: &'a str,
    /// The tool or prompt the request names, and the key that carries it.
    pub(super) target: Option<(&'static str, &'a str)>,
    pub(super) request_id: Option<String>,
    pub(super) version: Option<String>,
    pub(super) client: Option<(String, String)>,
    pub(super) meta: &'a JsonObject,
    pub(super) extensions: &'a Extensions,
    /// The `requestState` a multi-round retry carries.
    pub(super) state_in: Option<&'a str>,
}

/// A started operation. Dropping it before [`Op::finish`] ends the span as failed.
pub(super) struct Op {
    cx: Context,
    client: Arc<Client>,
    config: Arc<Config>,
    state: Arc<OpState>,
    cancel_source: &'static str,
    capture: Option<Capture>,
    /// The `identify` hook gave a handle id, which takes precedence over `handle_params`.
    identity_handle: bool,
    finished: bool,
}

struct Capture {
    rule: CaptureRule,
    truncated: bool,
}

impl Op {
    /// Starts the span. `None` before `crucible_sdk::init`: the handler then runs uninstrumented.
    pub(super) fn begin(
        config: &Arc<Config>,
        server: &(String, String),
        start: &Start<'_>,
    ) -> Option<Self> {
        let client = crate::client::active()?;
        let options = &config.options;
        let http = start.extensions.get::<http::request::Parts>();
        let transport = if http.is_some() {
            Transport::StreamableHttp
        } else {
            options.transport
        };
        let mut attributes = vec![
            KeyValue::new(k::METHOD, start.method.to_owned()),
            KeyValue::new(k::CAPTURE_SOURCE, "sdk"),
            KeyValue::new(k::TRANSPORT, transport.as_str()),
        ];
        let modern = start.version.as_deref().is_some_and(mcp_common::is_modern);
        if let Some(version) = &start.version {
            attributes.extend(mcp_common::era(version));
        }
        match &start.request_id {
            Some(id) => attributes.push(KeyValue::new(k::REQUEST_ID, id.clone())),
            None => attributes.push(KeyValue::new(k::IS_NOTIFICATION, true)),
        }
        for (key, value) in [(k::SERVER_NAME, &server.0), (k::SERVER_VERSION, &server.1)] {
            if !value.is_empty() {
                attributes.push(KeyValue::new(key, value.clone()));
            }
        }
        if let Some((name, version)) = &start.client {
            for (key, value) in [(k::CLIENT_NAME, name), (k::CLIENT_VERSION, version)] {
                if !value.is_empty() {
                    attributes.push(KeyValue::new(key, value.clone()));
                }
            }
        }
        if let Some(parts) = http {
            let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
            if let Some(agent) = header("user-agent") {
                attributes.push(KeyValue::new(k::USER_AGENT, agent.to_owned()));
            }
            if let Some(bytes) = header("content-length").and_then(|v| v.parse::<i64>().ok()) {
                attributes.push(KeyValue::new(k::REQUEST_BYTES, bytes));
            }
            // Legacy sessions only: the raw id is never sent (contract 9.4).
            if !modern && let Some(session) = header("mcp-session-id") {
                attributes.push(KeyValue::new(
                    k::SESSION_ID,
                    mcp_common::session_hash(session),
                ));
            }
        }
        if let Some((key, name)) = start.target {
            attributes.push(KeyValue::new(key, name.to_owned()));
        }
        if let Some(state) = start.state_in {
            attributes.push(KeyValue::new(
                k::STATE_IN,
                mcp_common::state_hash(&client.secret_key, state),
            ));
        }
        let mut identity_handle = false;
        if !options.no_identity
            && let Some(identify) = &options.identify
        {
            let id = identify(start.extensions);
            identity_handle = !id.handle_id.is_empty();
            identity(&mut attributes, &id);
        }
        let name = match start.target {
            Some((_, target)) => format!("{} {target}", start.method),
            None => start.method.to_owned(),
        };
        let parent = mcp_common::extract(start.meta);
        let span = client.tracer.build_with_context(
            SpanBuilder::from_name(name)
                .with_kind(SpanKind::Server)
                .with_attributes(attributes),
            &parent,
        );
        Some(Self {
            cx: parent.with_span(span),
            client,
            config: config.clone(),
            state: Arc::default(),
            // In the modern era over HTTP, closing the response stream is the cancellation signal; elsewhere
            // it is a notifications/cancelled message.
            cancel_source: if modern && transport == Transport::StreamableHttp {
                "stream_close"
            } else {
                "notification"
            },
            capture: None,
            identity_handle,
            finished: false,
        })
    }

    /// The context the handler runs in: its spans are children of the operation's.
    pub(super) fn context(&self) -> Context {
        self.cx.clone()
    }

    /// Records the protocol version and era once they are known (an `initialize` handshake).
    pub(super) fn set_version(&self, version: &str) {
        self.cx.span().set_attributes(mcp_common::era(version));
    }

    /// The handle a tool handler reaches the operation through.
    pub(super) fn handle(&self) -> OpHandle {
        OpHandle(self.state.clone())
    }

    /// Records what a tool call's request carries: the captured arguments and the handle id.
    pub(super) fn tool_call(&mut self, request: &CallToolRequestParams) {
        let span = self.cx.span();
        let options = &self.config.options;
        if !self.identity_handle
            && let Some(param) = options.handle_params.get(request.name.as_ref())
            && let Some(value) = request
                .arguments
                .as_ref()
                .and_then(|args| args.get(param))
                .and_then(serde_json::Value::as_str)
            && let Some(handle) = self.scrubbed(Field::HandleId, value, MAX_HANDLE_ID).0
        {
            span.set_attribute(KeyValue::new(k::HANDLE_ID, handle));
        }
        let Some(rule) = options
            .capture
            .iter()
            .find(|r| glob(&r.tool, &request.name))
            .filter(|r| sampled(r.sample_rate, &self.cx))
            .cloned()
        else {
            return;
        };
        let mut truncated = false;
        if rule.args
            && let Some(args) = &request.arguments
        {
            let (text, cut) = self.captured(Field::Arguments, args, rule.max_bytes);
            truncated |= cut;
            if let Some(text) = text {
                span.set_attribute(KeyValue::new(k::TOOL_ARGUMENTS, text));
            }
        }
        self.capture = Some(Capture { rule, truncated });
    }

    /// Records the captured result of a tool call.
    pub(super) fn tool_result(&mut self, result: &CallToolResult) {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct View<'a> {
            content: &'a [ContentBlock],
            #[serde(skip_serializing_if = "Option::is_none")]
            structured_content: Option<&'a serde_json::Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            is_error: Option<bool>,
        }
        let Some(capture) = &self.capture else {
            return;
        };
        if !capture.rule.result {
            return;
        }
        let max = capture.rule.max_bytes;
        // Content and structured content only: never the request state or input requests of a multi-round
        // exchange.
        let view = View {
            content: &result.content,
            structured_content: result.structured_content.as_ref(),
            is_error: result.is_error,
        };
        let (text, cut) = self.captured(Field::Result, &view, max);
        if let Some(capture) = &mut self.capture {
            capture.truncated |= cut;
        }
        if let Some(text) = text {
            self.cx
                .span()
                .set_attribute(KeyValue::new(k::TOOL_RESULT, text));
        }
    }

    /// A value serialized as JSON, scrubbed and capped; only the part the cap reads is serialized.
    fn captured<T: Serialize + ?Sized>(
        &self,
        field: Field,
        value: &T,
        max: usize,
    ) -> (Option<String>, bool) {
        let (json, cut, produced) = json_prefix(value, scrub::window(max));
        note_work(produced, json.len());
        let (text, cut_after) = self.scrubbed(field, &json, max);
        (text, cut || cut_after)
    }

    /// Text scrubbed by the shared rules and then the `scrub` hook, both over the window the cap reads, and
    /// then capped. `None` when there is nothing to send.
    fn scrubbed(&self, field: Field, text: &str, max: usize) -> (Option<String>, bool) {
        let (window, cut) = scrub::prefix(text, scrub::window(max));
        let mut text = scrub::scrub(window);
        if let Some(hook) = &self.config.options.scrub {
            match hook(field, &text) {
                Some(changed) => text = changed,
                None => return (None, cut),
            }
        }
        let (text, cut_after) = scrub::prefix(&text, max);
        (
            (!text.is_empty()).then(|| text.to_owned()),
            cut || cut_after,
        )
    }

    /// Ends the span with the outcome of a request.
    pub(super) fn finish<T: Outcomes>(mut self, result: &Result<T, ErrorData>, cancelled: bool) {
        let mut outcome = Outcome::default();
        match result {
            Ok(value) => value.record(&mut outcome),
            Err(error) => {
                outcome.code = Some(error.code.0);
                outcome.message = error.message.to_string();
            }
        }
        if cancelled {
            outcome.cancel = Some(self.cancel_source);
        }
        self.end(&outcome, false);
    }

    /// Ends the span of a notification.
    pub(super) fn finish_notification(mut self) {
        self.end(&Outcome::default(), true);
    }

    fn end(&mut self, outcome: &Outcome, notification: bool) {
        self.finished = true;
        let span = self.cx.span();
        let annotations = Annotations {
            subclass: self
                .state
                .subclass
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .map(super::Subclass::as_str),
            validation: self.state.validation.load(Ordering::Relaxed),
            not_found: self.state.not_found.load(Ordering::Relaxed),
        };
        let (mut attributes, class) = outcome.attributes(annotations, &self.client.secret_key);
        if notification {
            attributes.retain(|kv| kv.key.as_str() != k::RESULT_TYPE);
        }
        if self.state.progress_sent.load(Ordering::Relaxed) {
            attributes.push(KeyValue::new(
                k::PROGRESS_COUNT,
                i64::from(self.state.progress.load(Ordering::Relaxed)),
            ));
        }
        if let Some(capture) = &self.capture {
            attributes.push(KeyValue::new(k::CONTENT_CAPTURED, true));
            attributes.push(KeyValue::new(
                k::CONTENT_SAMPLE_RATE,
                capture.rule.sample_rate,
            ));
            let truncation = if capture.truncated {
                "cap_exceeded"
            } else {
                "none"
            };
            attributes.push(KeyValue::new(k::CONTENT_TRUNCATION, truncation));
        }
        span.set_attributes(attributes);
        if class != "none" {
            let message = match outcome.cancel {
                Some(_) => "cancelled".to_owned(),
                None => self
                    .scrubbed(Field::ErrorMessage, &outcome.message, MAX_ERROR_MESSAGE)
                    .0
                    .unwrap_or_default(),
            };
            span.set_status(Status::error(message));
        }
        span.end();
    }
}

impl Drop for Op {
    /// A handler that panicked, or whose future was dropped, never returned: its span still ends.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // A panic is an unhandled exception (contract 9.2 row 9); a future dropped by its runtime means the
        // transport went away with the operation pending (row 3).
        let panicked = std::thread::panicking();
        let outcome = Outcome {
            panicked,
            transport: !panicked,
            message: "the handler did not return".to_owned(),
            ..Outcome::default()
        };
        self.end(&outcome, false);
    }
}

/// What a handler's result tells about the outcome.
pub(super) trait Outcomes {
    fn record(&self, outcome: &mut Outcome) {
        outcome.result_type = Some("complete");
    }
}

impl Outcomes for rmcp::model::CallToolResponse {
    fn record(&self, outcome: &mut Outcome) {
        match self {
            Self::Complete(result) => outcome.tool_result(result),
            Self::InputRequired(result) => input_required(outcome, result),
            Self::Task(task) => {
                outcome.result_type = Some("task");
                outcome.task_id = Some(task.task.task_id.clone());
            }
            _ => outcome.result_type = Some("complete"),
        }
    }
}

impl Outcomes for rmcp::model::GetPromptResponse {
    fn record(&self, outcome: &mut Outcome) {
        match self {
            Self::InputRequired(result) => input_required(outcome, result),
            _ => outcome.result_type = Some("complete"),
        }
    }
}

impl Outcomes for rmcp::model::ReadResourceResponse {
    fn record(&self, outcome: &mut Outcome) {
        match self {
            Self::InputRequired(result) => input_required(outcome, result),
            _ => outcome.result_type = Some("complete"),
        }
    }
}

fn input_required(outcome: &mut Outcome, result: &rmcp::model::InputRequiredResult) {
    outcome.result_type = Some("input_required");
    outcome.state_out = result.request_state.clone();
}

macro_rules! plain_outcomes {
    ($($t:ty),* $(,)?) => { $(impl Outcomes for $t {})* };
}

plain_outcomes!(
    (),
    rmcp::model::InitializeResult,
    rmcp::model::DiscoverResult,
    rmcp::model::CompleteResult,
    rmcp::model::ListPromptsResult,
    rmcp::model::ListResourcesResult,
    rmcp::model::ListResourceTemplatesResult,
    rmcp::model::ListToolsResult,
    rmcp::model::CustomResult,
    rmcp::model::GetTaskResult,
    rmcp::model::ListSkillsResult,
    rmcp::model::GetSkillResult,
);

/// The identity keys of contract 9.3 from an `identify` hook.
fn identity(attributes: &mut Vec<KeyValue>, id: &Identity) {
    let mut any = false;
    for (key, value) in [
        (k::USER_ID, &id.user_id),
        (k::USER_ISSUER, &id.issuer),
        (k::USER_SUBJECT, &id.subject),
        (k::TENANT, &id.tenant),
    ] {
        if !value.is_empty() {
            attributes.push(KeyValue::new(key, value.clone()));
            any = true;
        }
    }
    if !id.handle_id.is_empty() {
        let handle = scrub::capped(&id.handle_id, MAX_HANDLE_ID).0;
        attributes.push(KeyValue::new(k::HANDLE_ID, handle));
        any = true;
    }
    if any {
        attributes.push(KeyValue::new(k::IDENTITY_SOURCE, "identify_hook"));
    }
}

/// Whether a call is captured at `rate`: decided from the span id, which the SDK draws at random, before
/// anything is copied.
fn sampled(rate: f64, cx: &Context) -> bool {
    if rate >= 1.0 {
        return true;
    }
    let id = u64::from_be_bytes(cx.span().span_context().span_id().to_bytes());
    // The top 53 bits as a uniform fraction in [0, 1).
    let fraction = (id >> 11) as f64 / (1u64 << 53) as f64;
    fraction < rate
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters and `?` any one character.
fn glob(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Reports one captured field's work to the test hook (feature `test-support`).
fn note_work(serialized: usize, scrubbed: usize) {
    #[cfg(feature = "test-support")]
    super::test_support::note(serialized, scrubbed);
    #[cfg(not(feature = "test-support"))]
    let _ = (serialized, scrubbed);
}

/// The first `limit` bytes of a value's JSON text, cut on a character boundary, whether the text is longer,
/// and how many bytes the serializer produced before it stopped. Serialization stops at the limit, so a large
/// value costs no more than the cap.
fn json_prefix<T: Serialize + ?Sized>(value: &T, limit: usize) -> (String, bool, usize) {
    struct Limited {
        buf: Vec<u8>,
        limit: usize,
        full: bool,
        produced: usize,
    }
    impl io::Write for Limited {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.produced += data.len();
            let room = self.limit - self.buf.len();
            if data.len() > room {
                self.buf.extend_from_slice(&data[..room]);
                self.full = true;
                return Err(io::Error::other("the capture cap is reached"));
            }
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut out = Limited {
        buf: Vec::new(),
        limit,
        full: false,
        produced: 0,
    };
    let _done = serde_json::to_writer(&mut out, value);
    let text = match String::from_utf8(out.buf) {
        Ok(text) => text,
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
    };
    (text, out.full, out.produced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_shell_patterns() {
        assert!(glob("*", "anything"));
        assert!(glob("*", ""));
        assert!(glob("secret", "secret"));
        assert!(!glob("secret", "secrets"));
        assert!(glob("search_*", "search_docs"));
        assert!(!glob("search_*", "lookup"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("*_order*", "lookup_order_v2"));
        assert!(glob("**x", "abx"));
        assert!(!glob("", "x"));
    }

    #[test]
    fn json_prefixes_stop_at_the_limit_on_a_boundary() {
        let value = serde_json::json!({ "note": "é".repeat(100) });
        let full = value.to_string();
        let (all, cut, _) = json_prefix(&value, 10_000);
        assert_eq!((all.as_str(), cut), (full.as_str(), false));
        for limit in 0..40 {
            let (text, cut, _) = json_prefix(&value, limit);
            assert!(cut);
            assert!(text.len() <= limit);
            assert!(full.starts_with(&text));
        }
    }
}
