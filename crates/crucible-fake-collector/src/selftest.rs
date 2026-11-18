//! The self-test runner: it emits, with `opentelemetry-proto`, the spans an SDK that follows the runner contract
//! would emit for a run spec, without any MCP server or SDK. It reads the run spec and the scrub rules every SDK
//! ships, and never the goldens, so a passing self-test means the collector, the driver and the goldens agree
//! with an independent encoding of the contract.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_env::Env;
use flate2::Compression;
use flate2::write::GzEncoder;
use http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE};
use http::{Request, Uri};
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as Proto;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message as _;
use serde_json::{Value as Json, json};
use sha2::{Digest as _, Sha256};

use crate::driver::{RunOutcome, Runner};
use crate::hash::{mrtr_hash, session_hash};
use crate::scrub::ScrubRules;
use crate::spec::{ResponseEvent, RunSpec, fill_templates};
use crate::suite::{
    Action, Era, Kind, Node, ProgressCount, Step, SuiteError, ThrowOutcome, Transport,
};

/// The session id the self-test's legacy HTTP server assigns.
pub const SESSION_ID: &str = "3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5";
/// The reference server's tools.
const TOOLS: [&str; 7] = ["echo", "fail", "throw", "slow", "secret", "needs_input", "lookup_order"];
/// The `requestState` the self-test's `needs_input` tool returns.
pub const REQUEST_STATE: &str = "crucible-conformance-request-state-1";

/// A seeded defect that the driver must flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutant {
    /// Captured arguments and results are exported without scrubbing.
    #[cfg(feature = "test-mutants")]
    UnscrubbedSecret,
}

impl Mutant {
    /// Parses a `--mutant` name.
    pub fn parse(name: &str) -> Option<Self> {
        #[cfg(feature = "test-mutants")]
        if name == "unscrubbed-secret" {
            return Some(Self::UnscrubbedSecret);
        }
        let _ = name;
        None
    }
}

/// What the modelled SDK does where SDKs differ (the manifest's capability fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfTestOptions {
    /// What a tool handler's exception becomes.
    pub throw_outcome: ThrowOutcome,
    /// Whether progress notifications are counted.
    pub progress_count: ProgressCount,
    /// A seeded defect, for tests only.
    pub mutant: Option<Mutant>,
}

impl Default for SelfTestOptions {
    /// The behaviour `conformance/sdks/selftest.json` declares.
    fn default() -> Self {
        Self {
            throw_outcome: ThrowOutcome::Internal,
            progress_count: ProgressCount::Observed,
            mutant: None,
        }
    }
}

/// The self-test runner.
#[derive(Debug)]
pub struct SelfTestRunner {
    env: Env,
    options: SelfTestOptions,
    scrub: ScrubRules,
}

#[async_trait]
impl Runner for SelfTestRunner {
    async fn run(&self, spec: &RunSpec) -> RunOutcome {
        match self.play(spec).await {
            Ok(responses) => RunOutcome {
                responses,
                exit_code: Some(0),
                ..RunOutcome::default()
            },
            Err(error) => RunOutcome {
                exit_code: Some(1),
                stderr_tail: error.clone(),
                error: Some(error),
                ..RunOutcome::default()
            },
        }
    }
}

/// One MCP operation being built.
struct Op {
    span: Span,
    duration_us: u64,
    response: Option<Json>,
    notifications: u32,
}

fn kv(key: &str, value: Proto) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        ..KeyValue::default()
    }
}

fn text(key: &str, value: impl Into<String>) -> KeyValue {
    kv(key, Proto::StringValue(value.into()))
}

fn flag(key: &str, value: bool) -> KeyValue {
    kv(key, Proto::BoolValue(value))
}

fn int(key: &str, value: i64) -> KeyValue {
    kv(key, Proto::IntValue(value))
}

fn strings(key: &str, values: &[String]) -> KeyValue {
    kv(
        key,
        Proto::ArrayValue(ArrayValue {
            values: values
                .iter()
                .map(|v| AnyValue {
                    value: Some(Proto::StringValue(v.clone())),
                })
                .collect(),
        }),
    )
}

/// A JSON scalar as an attribute value: strings, booleans, integers and doubles.
fn scalar(key: &str, value: &Json) -> Option<KeyValue> {
    Some(match value {
        Json::String(s) => text(key, s.clone()),
        Json::Bool(b) => flag(key, *b),
        Json::Number(n) if !n.is_f64() => int(key, n.as_i64()?),
        Json::Number(n) => kv(key, Proto::DoubleValue(n.as_f64()?)),
        _ => return None,
    })
}

/// A string verbatim; any other JSON value as compact JSON text.
fn body_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn digest(parts: &[&str]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().into()
}

fn trace_id(parts: &[&str]) -> Vec<u8> {
    digest(parts)[..16].to_vec()
}

fn span_id(parts: &[&str]) -> Vec<u8> {
    digest(parts)[16..24].to_vec()
}

/// The trace and parent span ids of a `traceparent`.
fn traceparent(value: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut parts = value.split('-');
    let (_, trace, parent) = (parts.next()?, parts.next()?, parts.next()?);
    let hex = |s: &str| -> Option<Vec<u8>> {
        (0..s.len())
            .step_by(2)
            .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
            .collect()
    };
    let (trace, parent) = (hex(trace)?, hex(parent)?);
    (trace.len() == 16 && parent.len() == 8).then_some((trace, parent))
}

fn id_text(id: &Json) -> String {
    match id {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Cuts `text` at `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

impl SelfTestRunner {
    /// A runner modelling an SDK with `options`.
    pub fn new(env: Env, options: SelfTestOptions) -> Result<Self, SuiteError> {
        Ok(Self {
            env,
            options,
            scrub: ScrubRules::builtin()?,
        })
    }

    /// Plays a run spec; returns the response events a runner would print.
    pub async fn play(&self, spec: &RunSpec) -> Result<Vec<ResponseEvent>, String> {
        match spec.kind {
            Kind::Mcp => self.play_mcp(spec).await,
            Kind::Agent => self.play_agent(spec).await,
        }
    }

    fn scrubbed(&self, text: &str) -> String {
        #[cfg(feature = "test-mutants")]
        if self.options.mutant == Some(Mutant::UnscrubbedSecret) {
            return text.to_owned();
        }
        self.scrub.scrub(text)
    }

    fn resource(&self, spec: &RunSpec) -> Resource {
        let mut attributes = vec![
            text("service.name", spec.sdk.service_name.clone()),
            text("telemetry.sdk.name", "crucible-sdk-selftest"),
            text("telemetry.sdk.language", "rust"),
            text("telemetry.sdk.version", env!("CARGO_PKG_VERSION")),
        ];
        if let Some(server) = &spec.server {
            attributes.push(text("service.version", server.version.clone()));
            attributes.push(text(
                "service.instance.id",
                format!("selftest-{}", spec.case),
            ));
        }
        if let Some(deploy) = &spec.sdk.deploy_id {
            attributes.push(text("crucible.deploy.id", deploy.clone()));
        }
        if let Some(environment) = &spec.sdk.environment {
            attributes.push(text("deployment.environment.name", environment.clone()));
        }
        if let Some(release) = &spec.sdk.release {
            attributes.push(text("service.version", release.clone()));
        }
        Resource {
            attributes,
            ..Resource::default()
        }
    }

    async fn play_mcp(&self, spec: &RunSpec) -> Result<Vec<ResponseEvent>, String> {
        let era = spec.era.ok_or("an MCP run spec has an era")?;
        let transport = spec.transport.ok_or("an MCP run spec has a transport")?;
        let base = self.base_nanos();
        let mut cursor_us: u64 = 0;
        let mut spans = Vec::new();
        let mut events = Vec::new();
        let mut responses: BTreeMap<String, Json> = BTreeMap::new();
        let mut pending: BTreeMap<String, (Json, Op)> = BTreeMap::new();
        let mut client: Option<Json> = None;
        for step in &spec.steps {
            match step.action()? {
                Action::Send(message) => {
                    let mut message = message.clone();
                    fill_templates(&mut message, &responses)?;
                    if message.get("method") == Some(&json!("initialize")) {
                        client = message.pointer("/params/clientInfo").cloned();
                    }
                    let mut op = self.operation(
                        spec,
                        era,
                        transport,
                        &message,
                        client.as_ref(),
                        base + cursor_us * 1000,
                    );
                    if let Some(id) = message.get("id").cloned() {
                        if step.waits() {
                            cursor_us += op.duration_us;
                            let event = finish(&mut op, step, transport, era, &id);
                            if let Some(m) = &event.message {
                                responses.insert(step.step.clone(), m.clone());
                            }
                            spans.push(op.span);
                            events.push(event);
                        } else {
                            pending.insert(step.step.clone(), (id, op));
                        }
                    } else {
                        cursor_us += 1_000;
                        if message.get("method") == Some(&json!("notifications/cancelled")) {
                            let target = message.pointer("/params/requestId").cloned();
                            let found = pending
                                .iter()
                                .find(|(_, (id, _))| Some(id) == target.as_ref())
                                .map(|(name, _)| name.clone());
                            if let Some((_, mut cancelled)) =
                                found.and_then(|name| pending.remove(&name))
                            {
                                cancel(&mut cancelled, "notification", base + cursor_us * 1000);
                                spans.push(cancelled.span);
                            }
                        }
                        op.span.end_time_unix_nano = op.span.start_time_unix_nano + 1_000_000;
                        spans.push(op.span);
                        events.push(ResponseEvent {
                            step: step.step.clone(),
                            http_status: (transport == Transport::StreamableHttp).then_some(202),
                            session_id: None,
                            message: None,
                            notifications: 0,
                        });
                    }
                }
                Action::Await(target) => {
                    let (id, mut op) = pending
                        .remove(target)
                        .ok_or_else(|| format!("await names {target:?}, which is not in flight"))?;
                    let sent = spec
                        .steps
                        .iter()
                        .find(|s| s.step == target)
                        .ok_or_else(|| format!("await names unknown step {target:?}"))?;
                    let event = finish(&mut op, sent, transport, era, &id);
                    if let Some(m) = &event.message {
                        responses.insert(target.to_owned(), m.clone());
                    }
                    spans.push(op.span);
                    events.push(event);
                }
                Action::Close(target) => {
                    let (_, mut op) = pending
                        .remove(target)
                        .ok_or_else(|| format!("close names {target:?}, which is not in flight"))?;
                    cancel(&mut op, "stream_close", base + cursor_us * 1000);
                    spans.push(op.span);
                }
                Action::Sleep(ms) => cursor_us = cursor_us.saturating_add(ms.saturating_mul(1000)),
                _ => return Err(format!("step {:?} is not an MCP step", step.step)),
            }
        }
        for (_, (id, mut op)) in std::mem::take(&mut pending) {
            let step = Step {
                step: String::new(),
                ..Step::default()
            };
            let _abandoned = finish(&mut op, &step, transport, era, &id);
            spans.push(op.span);
        }
        self.export(spec, spans, transport == Transport::StreamableHttp)
            .await?;
        Ok(events)
    }

    fn base_nanos(&self) -> u64 {
        u64::try_from(self.env.clock.wall_now().micros())
            .unwrap_or(0)
            .saturating_mul(1000)
    }

    /// Builds the span and the response of one inbound message, as a server wrapper would.
    fn operation(
        &self,
        spec: &RunSpec,
        era: Era,
        transport: Transport,
        message: &Json,
        client: Option<&Json>,
        start: u64,
    ) -> Op {
        let method = message.get("method").and_then(Json::as_str).unwrap_or("");
        let id = message.get("id");
        let params = message.get("params").cloned().unwrap_or(Json::Null);
        let tool = params
            .get("name")
            .and_then(Json::as_str)
            .filter(|_| method == "tools/call");
        let http = transport == Transport::StreamableHttp;
        let step_key = id.map_or_else(|| method.to_owned(), id_text);
        let mut attributes = vec![
            text("mcp.method.name", method),
            text("crucible.mcp.capture_source", "sdk"),
            text("crucible.mcp.era", era.as_str()),
            text("crucible.mcp.transport", transport.as_str()),
            text("network.transport", if http { "tcp" } else { "pipe" }),
        ];
        if let Some(version) = &spec.protocol_version {
            attributes.push(text("mcp.protocol.version", version.clone()));
        }
        if let Some(server) = &spec.server {
            attributes.push(text("crucible.mcp.server.name", server.name.clone()));
            attributes.push(text("crucible.mcp.server.version", server.version.clone()));
        }
        match id {
            Some(id) => {
                attributes.push(text("jsonrpc.request.id", id_text(id)));
                let info = match era {
                    Era::Modern => params.pointer("/_meta/io.modelcontextprotocol~1clientInfo"),
                    Era::Legacy => client,
                };
                if let Some(info) = info {
                    for (field, key) in [
                        ("name", "crucible.mcp.client.name"),
                        ("version", "crucible.mcp.client.version"),
                    ] {
                        if let Some(value) = info.get(field).and_then(Json::as_str) {
                            attributes.push(text(key, value));
                        }
                    }
                }
            }
            None => attributes.push(flag("crucible.mcp.is_notification", true)),
        }
        if http {
            attributes.push(int(
                "http.response.status_code",
                if id.is_some() { 200 } else { 202 },
            ));
            attributes.push(text("crucible.mcp.client.ip_prefix", "127.0.0.0/24"));
            if era == Era::Legacy && method != "initialize" {
                attributes.push(text("mcp.session.id", session_hash(SESSION_ID)));
            }
        }
        if let Some(tool) = tool {
            attributes.push(text("gen_ai.tool.name", tool));
        }
        let (trace, parent) = params
            .pointer("/_meta/traceparent")
            .and_then(Json::as_str)
            .and_then(traceparent)
            .unwrap_or_else(|| (trace_id(&[&spec.case, &step_key]), Vec::new()));
        let mut op = Op {
            span: Span {
                trace_id: trace,
                span_id: span_id(&[&spec.case, &step_key]),
                parent_span_id: parent,
                name: tool.map_or_else(|| method.to_owned(), |t| format!("{method} {t}")),
                kind: SpanKind::Server as i32,
                start_time_unix_nano: start,
                attributes,
                ..Span::default()
            },
            duration_us: 1_000,
            response: Some(json!({})),
            notifications: 0,
        };
        match method {
            "initialize" => {
                op.response = Some(json!({
                    "protocolVersion": spec.protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": spec.server,
                }));
            }
            "tools/list" => {
                let tools: Vec<Json> = TOOLS
                    .iter()
                    .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
                    .collect();
                op.response = Some(json!({ "tools": tools }));
            }
            "tools/call" => self.tool(spec, tool.unwrap_or(""), &params, &mut op),
            _ => {}
        }
        op
    }

    /// The behaviour of the reference server's tools, and what the SDK records about each call.
    fn tool(&self, spec: &RunSpec, tool: &str, params: &Json, op: &mut Op) {
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let arg = |name: &str| {
            args.get(name)
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let content = |text: String| json!({ "content": [{ "type": "text", "text": text }] });
        match tool {
            "echo" => op.response = Some(content(arg("text"))),
            "fail" => {
                let reason = arg("reason");
                op.span.attributes.push(text("error.type", "tool_error"));
                op.span
                    .attributes
                    .push(text("crucible.mcp.error.class", "tool_execution"));
                op.span
                    .attributes
                    .push(text("crucible.mcp.error.subclass", "upstream"));
                op.span.status = Some(Status {
                    message: self.scrubbed(&reason),
                    code: StatusCode::Error as i32,
                });
                let mut result = content(reason);
                result["isError"] = json!(true);
                op.response = Some(result);
            }
            "throw" => {
                let message = arg("message");
                op.span.status = Some(Status {
                    message: self.scrubbed(&message),
                    code: StatusCode::Error as i32,
                });
                match self.options.throw_outcome {
                    ThrowOutcome::Internal => {
                        op.span.attributes.push(text("error.type", "-32603"));
                        op.span
                            .attributes
                            .push(text("rpc.response.status_code", "-32603"));
                        op.response = None;
                    }
                    ThrowOutcome::ToolError => {
                        op.span.attributes.push(text("error.type", "tool_error"));
                        op.span
                            .attributes
                            .push(text("crucible.mcp.error.class", "tool_execution"));
                        let mut result = content(message);
                        result["isError"] = json!(true);
                        op.response = Some(result);
                    }
                }
            }
            "slow" => {
                let ms = args.get("ms").and_then(Json::as_u64).unwrap_or(0);
                let steps = args.get("steps").and_then(Json::as_u64).unwrap_or(0);
                op.duration_us = ms.saturating_mul(1000).max(1_000);
                if params.pointer("/_meta/progressToken").is_some() {
                    op.notifications = u32::try_from(steps).unwrap_or(u32::MAX);
                    if self.options.progress_count == ProgressCount::Observed {
                        op.span.attributes.push(int(
                            "crucible.mcp.progress.count",
                            i64::try_from(steps).unwrap_or(i64::MAX),
                        ));
                    }
                }
                op.response = Some(content("done".to_owned()));
            }
            "secret" => op.response = Some(content(arg("note"))),
            "needs_input" => {
                let secret = &spec.keys.secret_key;
                match params.get("requestState").and_then(Json::as_str) {
                    Some(state) => {
                        op.span
                            .attributes
                            .push(text("crucible.mcp.mrtr.state_in", mrtr_hash(secret, state)));
                        op.span
                            .attributes
                            .push(text("crucible.mcp.result_type", "complete"));
                        op.response = Some(content(format!("confirmed {}", arg("item"))));
                    }
                    None => {
                        op.span
                            .attributes
                            .push(text("crucible.mcp.result_type", "input_required"));
                        op.span.attributes.push(text(
                            "crucible.mcp.mrtr.state_out",
                            mrtr_hash(secret, REQUEST_STATE),
                        ));
                        op.response = Some(json!({
                            "resultType": "input_required",
                            "requestState": REQUEST_STATE,
                            "inputRequests": { "confirm": {
                                "method": "elicitation/create",
                                "params": { "mode": "form", "message": "Confirm the item", "requestedSchema": {
                                    "type": "object",
                                    "properties": { "confirmed": { "type": "boolean" } },
                                    "required": ["confirmed"],
                                } },
                            } },
                        }));
                    }
                }
            }
            "lookup_order" => {
                op.response = Some(content(format!("order {}: shipped", arg("order_id"))))
            }
            _ => {
                op.span.attributes.push(text("error.type", "-32602"));
                op.span
                    .attributes
                    .push(text("rpc.response.status_code", "-32602"));
                op.response = None;
            }
        }
        if let Some(param) = spec.sdk.handle_params.get(tool)
            && let Some(value) = args.get(param).and_then(Json::as_str)
        {
            let scrubbed = self.scrubbed(value);
            op.span
                .attributes
                .push(text("crucible.mcp.handle.id", cut(&scrubbed, 256).0));
        }
        if let Some(rule) = spec
            .sdk
            .capture
            .iter()
            .find(|r| r.tool == tool || r.tool == "*")
        {
            let mut truncated = false;
            let mut capture = |key: &str, value: &Json| {
                let scrubbed = self.scrubbed(&value.to_string());
                let (kept, was_cut) = cut(&scrubbed, rule.max_bytes);
                truncated |= was_cut;
                text(key, kept)
            };
            let mut captured = Vec::new();
            if rule.args {
                captured.push(capture("gen_ai.tool.call.arguments", &args));
            }
            if rule.result
                && let Some(result) = &op.response
            {
                captured.push(capture("gen_ai.tool.call.result", result));
            }
            captured.push(flag("crucible.mcp.content.captured", true));
            captured.push(kv(
                "crucible.mcp.content.sample_rate",
                Proto::DoubleValue(rule.sample_rate),
            ));
            captured.push(text(
                "crucible.mcp.content.truncation",
                if truncated { "cap_exceeded" } else { "none" },
            ));
            op.span.attributes.extend(captured);
        }
    }

    async fn play_agent(&self, spec: &RunSpec) -> Result<Vec<ResponseEvent>, String> {
        let base = self.base_nanos();
        let mut cursor = base;
        let mut buffer: Vec<Span> = Vec::new();
        let mut shut_down = false;
        for step in &spec.steps {
            match step.action()? {
                Action::Observe(node) => {
                    if shut_down {
                        // Spans started after shutdown are dropped without an error.
                        continue;
                    }
                    let mut tree = Tree {
                        out: &mut buffer,
                        case: &spec.case,
                        trace: trace_id(&[&spec.case, &step.step]),
                    };
                    cursor = tree.node(&step.step, &[], node, &Inherited::default(), cursor);
                }
                Action::Flush => {
                    let spans = std::mem::take(&mut buffer);
                    if !spans.is_empty() {
                        self.export(spec, spans, false).await?;
                    }
                }
                Action::Checkpoint(name) => {
                    let body = json!({ "name": name }).to_string();
                    let status = self
                        .post(
                            &spec.checkpoint_url,
                            "application/json",
                            None,
                            body.into_bytes(),
                            false,
                        )
                        .await?;
                    if status != 204 {
                        return Err(format!("the checkpoint answered {status}"));
                    }
                }
                Action::Shutdown => {
                    let spans = std::mem::take(&mut buffer);
                    if !spans.is_empty() {
                        self.export(spec, spans, false).await?;
                    }
                    shut_down = true;
                }
                Action::Sleep(ms) => cursor = cursor.saturating_add(ms.saturating_mul(1_000_000)),
                _ => return Err(format!("step {:?} is not an agent step", step.step)),
            }
        }
        Ok(Vec::new())
    }

    async fn export(&self, spec: &RunSpec, spans: Vec<Span>, gzip: bool) -> Result<(), String> {
        let request = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(self.resource(spec)),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "crucible-sdk-selftest".to_owned(),
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                        ..InstrumentationScope::default()
                    }),
                    spans,
                    ..ScopeSpans::default()
                }],
                ..ResourceSpans::default()
            }],
        };
        let auth = format!("Bearer {}", spec.keys.secret_key);
        let status = self
            .post(
                &spec.otlp.traces_url,
                "application/x-protobuf",
                Some(&auth),
                request.encode_to_vec(),
                gzip,
            )
            .await?;
        if status == 200 {
            Ok(())
        } else {
            Err(format!("the collector answered {status}"))
        }
    }

    async fn post(
        &self,
        url: &str,
        content_type: &str,
        auth: Option<&str>,
        body: Vec<u8>,
        gzip: bool,
    ) -> Result<u16, String> {
        let uri: Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
        let host: IpAddr = uri
            .host()
            .and_then(|h| h.parse().ok())
            .ok_or_else(|| format!("{url}: the host is not an IP address"))?;
        let addr = SocketAddr::new(host, uri.port_u16().unwrap_or(80));
        let body = if gzip {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&body).map_err(|e| e.to_string())?;
            encoder.finish().map_err(|e| e.to_string())?
        } else {
            body
        };
        let deadline = self.env.clock.now().saturating_add(Duration::from_secs(10));
        let stream = self
            .env
            .net
            .connect(addr, deadline)
            .await
            .map_err(|e| format!("connect {addr}: {e}"))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        let driver = self.env.spawn("selftest-connection", async move {
            let _closed = connection.await;
        });
        let target = uri.path_and_query().map_or("/", |p| p.as_str());
        let mut request = Request::post(target)
            .header(CONTENT_TYPE, content_type)
            .header("host", addr.to_string());
        if gzip {
            request = request.header(CONTENT_ENCODING, "gzip");
        }
        if let Some(auth) = auth {
            request = request.header(AUTHORIZATION, auth);
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| e.to_string())?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let _body = response.into_body().collect().await;
        driver.abort();
        Ok(status)
    }
}

/// Ends an operation normally: closes its span and builds the runner's response event.
fn finish(op: &mut Op, step: &Step, transport: Transport, era: Era, id: &Json) -> ResponseEvent {
    op.span.end_time_unix_nano = op.span.start_time_unix_nano + op.duration_us * 1000;
    let message = match op.response.take() {
        Some(mut result) => {
            if era == Era::Modern && result.get("resultType").is_none() {
                result["resultType"] = json!("complete");
            }
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        }
        None => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": error_code(&op.span), "message": "error" } })
        }
    };
    let initialize = step.send.as_ref().and_then(|m| m.get("method")) == Some(&json!("initialize"));
    let http = transport == Transport::StreamableHttp;
    ResponseEvent {
        step: step.step.clone(),
        http_status: http.then_some(200),
        session_id: (http && era == Era::Legacy && initialize).then(|| SESSION_ID.to_owned()),
        message: Some(message),
        notifications: op.notifications,
    }
}

fn error_code(span: &Span) -> i64 {
    span.attributes
        .iter()
        .find(|kv| kv.key == "rpc.response.status_code")
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match &v.value {
            Some(Proto::StringValue(s)) => s.parse().ok(),
            _ => None,
        })
        .unwrap_or(-32603)
}

/// Ends a cancelled operation at `at`.
fn cancel(op: &mut Op, source: &str, at: u64) {
    op.span.end_time_unix_nano = at.max(op.span.start_time_unix_nano);
    op.span
        .attributes
        .push(text("crucible.mcp.cancel.source", source));
    op.span
        .attributes
        .push(text("crucible.mcp.error.class", "cancelled"));
    op.span.status = Some(Status {
        message: "cancelled".to_owned(),
        code: StatusCode::Error as i32,
    });
}

/// Context set on a node, inherited by its subtree.
#[derive(Clone, Default)]
struct Inherited {
    attributes: Vec<KeyValue>,
}

/// The spans of one agent step.
struct Tree<'a> {
    out: &'a mut Vec<Span>,
    case: &'a str,
    trace: Vec<u8>,
}

impl Tree<'_> {
    /// Emits a node and its children, depth first, starting at `start`; returns the end time.
    fn node(
        &mut self,
        path: &str,
        parent: &[u8],
        node: &Node,
        inherited: &Inherited,
        start: u64,
    ) -> u64 {
        let path = format!("{path}/{}", node.name);
        let mut context = inherited.clone();
        if let Some(c) = &node.context {
            if let Some(session) = &c.session_id {
                context
                    .attributes
                    .push(text("crucible.session.id", session.clone()));
            }
            if let Some(user) = &c.user_id {
                context
                    .attributes
                    .push(text("crucible.user.id", user.clone()));
            }
            if !c.tags.is_empty() {
                context.attributes.push(strings("crucible.tags", &c.tags));
            }
            for (key, value) in &c.metadata {
                if let Some(attribute) = scalar(&format!("crucible.metadata.{key}"), value) {
                    context.attributes.push(attribute);
                }
            }
        }
        let id = span_id(&[self.case, &path]);
        let mut attributes = context.attributes.clone();
        attributes.push(text("crucible.observation.type", node.observation.clone()));
        if let Some(name) = &node.trace_name {
            attributes.push(text("crucible.trace.name", name.clone()));
        }
        if let Some(input) = &node.input {
            attributes.push(text("crucible.input", body_text(input)));
        }
        if let Some(output) = &node.output {
            attributes.push(text("crucible.output", body_text(output)));
        }
        if let Some(model) = &node.model {
            attributes.push(text("gen_ai.request.model", model.clone()));
        }
        if let Some(provider) = &node.provider {
            attributes.push(text("gen_ai.provider.name", provider.clone()));
        }
        if let Some(messages) = &node.input_messages {
            attributes.push(text("gen_ai.input.messages", messages.to_string()));
        }
        if let Some(messages) = &node.output_messages {
            attributes.push(text("gen_ai.output.messages", messages.to_string()));
        }
        if let Some(reasons) = &node.finish_reasons {
            attributes.push(strings("gen_ai.response.finish_reasons", reasons));
        }
        for (key, count) in &node.usage {
            attributes.push(int(
                &format!("gen_ai.usage.{key}"),
                i64::try_from(*count).unwrap_or(i64::MAX),
            ));
        }
        for (key, usd) in &node.cost_usd {
            attributes.push(text(&format!("crucible.cost.{key}"), usd.clone()));
        }
        let mut end = start + 1_000_000;
        for child in &node.children {
            end = self.node(&path, &id, child, &context, end) + 1_000_000;
        }
        self.out.push(Span {
            trace_id: self.trace.clone(),
            span_id: id,
            parent_span_id: parent.to_vec(),
            name: node.name.clone(),
            kind: SpanKind::Internal as i32,
            start_time_unix_nano: start,
            end_time_unix_nano: end,
            attributes,
            ..Span::default()
        });
        end
    }
}
