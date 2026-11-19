//! The `otlp-json` format (contract 12.8): one span per line, each line a complete OTLP JSON resource-spans
//! document that holds exactly one span.
//!
//! The span is re-emitted from the normalized record. Its fields go out under the keys the native tables read
//! (contract 10.6): the OTel GenAI keys where a semantic convention exists (operation name, request and response
//! model, provider, input and output messages, system instructions, tool definitions, token usage, finish
//! reasons, request parameters, the tool of a tool span), and the `crucible.*` keys otherwise (observation type,
//! trace name, session, user, environment, release, version, tags, metadata, level, prompt, provided cost,
//! completion start). Every residual attribute follows; a key that dialect mapping consumed is not in the
//! residual and is never re-emitted, and no key of another product's dialect is ever written. The MCP operation
//! of a span goes out under the keys of contract 9.1, which its projector removed from the residual (see
//! [`mcp`]). Re-ingested, a natively instrumented span normalizes to the same record and the same operation (the
//! round-trip property of the tests); a span typed without an operation name keeps the one the line adds in its
//! residual, since the mapping does not consume that key.
//!
//! Messages by `io_shape`: `genai_messages` as stored; `indexed_messages` as the array the normalizer stored
//! (contract 10.5); `raw` whose value is an object with a `messages` array emits that array; any other `raw`
//! value is wrapped in one message (`user` for the input, `assistant` for the output) and the span carries
//! `crucible.io_shape`. The raw input and output of a tool span or an MCP operation are its call's arguments and
//! result, which go out under the tool keys instead. A span with a field that a cap cut (captured MCP content
//! included) is skipped by default; kept, its cut fields go out as opaque text where one value holds them, and the
//! cut maps are named in `crucible.truncated_fields`.

use crucible_query::ExportRecord;
use crucible_records::{
    IoShape, Level, ObservationType, PayloadField, ProjectScope, SpanCore, SpanId, SpanPayload,
    StatusCode, Timestamp, TracePublic, Truncation, UsageEntry,
};

use crate::json::{self, Json, Obj};

mod mcp;

/// What became of one span.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Written {
    /// The span's line was appended.
    Line,
    /// The span has a field a cap cut and the export skips such spans; nothing was appended.
    Skipped,
}

/// The semantic-convention key of each normalized usage key (contract 10.6, dialect 0).
const USAGE_KEYS: [(&str, &str); 5] = [
    (UsageEntry::INPUT, "gen_ai.usage.input_tokens"),
    (UsageEntry::OUTPUT, "gen_ai.usage.output_tokens"),
    (
        UsageEntry::CACHE_READ,
        "gen_ai.usage.cache_read.input_tokens",
    ),
    (
        UsageEntry::CACHE_WRITE,
        "gen_ai.usage.cache_creation.input_tokens",
    ),
    (
        UsageEntry::REASONING,
        "gen_ai.usage.reasoning.output_tokens",
    ),
];

/// Appends an OTLP `AnyValue`.
fn any(out: &mut Vec<u8>, value: &Json) {
    let mut obj = Obj::new(out);
    match value {
        Json::Null => {}
        Json::Bool(b) => obj.bool("boolValue", *b),
        Json::Int(i) => obj.str("intValue", &i.to_string()),
        Json::UInt(u) => obj.str("stringValue", &u.to_string()),
        Json::Float(f) => json::float(obj.key("doubleValue"), *f),
        Json::Str(s) => obj.str("stringValue", s),
        Json::Arr(items) => {
            let mut array = Obj::new(obj.key("arrayValue"));
            let values = array.key("values");
            values.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    values.push(b',');
                }
                any(values, item);
            }
            values.push(b']');
            array.end();
        }
        Json::Obj(members) => {
            let mut list = Obj::new(obj.key("kvlistValue"));
            let values = list.key("values");
            values.push(b'[');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    values.push(b',');
                }
                let mut kv = Obj::new(values);
                kv.str("key", key);
                any(kv.key("value"), item);
                kv.end();
            }
            values.push(b']');
            list.end();
        }
    }
    obj.end();
}

/// An attribute list being written, and its keys.
struct Attrs {
    out: Vec<u8>,
    keys: Vec<String>,
}

impl Attrs {
    fn new() -> Self {
        Self {
            out: vec![b'['],
            keys: Vec::new(),
        }
    }

    fn has(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }

    fn put(&mut self, key: &str, value: &Json) {
        if self.out.len() > 1 {
            self.out.push(b',');
        }
        self.keys.push(key.to_owned());
        let mut kv = Obj::new(&mut self.out);
        kv.str("key", key);
        any(kv.key("value"), value);
        kv.end();
    }

    fn text(&mut self, key: &str, value: &str) {
        if !value.is_empty() {
            self.put(key, &Json::Str(value.to_owned()));
        }
    }

    fn texts<S: AsRef<str>>(&mut self, key: &str, values: &[S]) {
        if !values.is_empty() {
            let items = values.iter().map(|v| Json::Str(v.as_ref().to_owned()));
            self.put(key, &Json::Arr(items.collect()));
        }
    }

    /// The residual attributes of a stored object, after the derived ones, every member as stored. A member
    /// whose key a derived attribute also has is an instance the mapping did not take (an empty value, or one
    /// that lost to a higher-priority source); the derived attribute comes first, so it is again the one taken.
    fn residual(&mut self, stored: &[u8]) {
        if let Some(Json::Obj(members)) = Json::parse(stored) {
            for (key, value) in &members {
                self.put(key, value);
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.out.push(b']');
        self.out
    }
}

fn nanos(t: Timestamp) -> String {
    i128::from(t.micros()).saturating_mul(1_000).to_string()
}

/// A cost in nano-USD as an exact decimal USD string (contract 10.6 accepts a decimal string).
fn usd_text(nanos: i64) -> String {
    let sign = if nanos < 0 { "-" } else { "" };
    let n = nanos.unsigned_abs();
    format!("{sign}{}.{:09}", n / 1_000_000_000, n % 1_000_000_000)
}

/// The level the normalizer derives when no level is given (contract 3.1).
fn derived_level(core: &SpanCore) -> Level {
    if core.status_code == StatusCode::Error {
        Level::Error
    } else {
        Level::Default
    }
}

/// The semantic-convention operation name of an observation type, where one exists.
fn operation(kind: ObservationType) -> Option<&'static str> {
    match kind {
        ObservationType::Generation => Some("chat"),
        ObservationType::Embedding => Some("embeddings"),
        ObservationType::Tool => Some("execute_tool"),
        ObservationType::Agent => Some("invoke_agent"),
        _ => None,
    }
}

/// One message wrapping a value that is not a message list.
fn wrapped(role: &str, body: &[u8]) -> Json {
    let text = match Json::parse(body) {
        Some(Json::Str(s)) => s,
        _ => String::from_utf8_lossy(body).into_owned(),
    };
    let part = Json::Obj(vec![
        ("type".to_owned(), Json::Str("text".to_owned())),
        ("content".to_owned(), Json::Str(text)),
    ]);
    Json::Arr(vec![Json::Obj(vec![
        ("role".to_owned(), Json::Str(role.to_owned())),
        ("parts".to_owned(), Json::Arr(vec![part])),
    ])])
}

/// The message list of a body under the `io_shape` rule, and whether it had to be wrapped.
fn messages(shape: IoShape, role: &str, body: &[u8], cut: bool) -> (String, bool) {
    if cut {
        let mut out = Vec::new();
        wrapped(role, body).write(&mut out);
        return (String::from_utf8_lossy(&out).into_owned(), true);
    }
    let parsed = Json::parse(body);
    let list = match (shape, &parsed) {
        (IoShape::GenaiMessages | IoShape::IndexedMessages, Some(value)) => Some(value),
        (IoShape::Raw, Some(Json::Obj(members))) => members
            .iter()
            .find(|(key, value)| key == "messages" && matches!(value, Json::Arr(_)))
            .map(|(_, value)| value),
        _ => None,
    };
    let (value, wrap) = match list {
        Some(value) => (value.clone(), false),
        None => (wrapped(role, body), true),
    };
    if !wrap && matches!(shape, IoShape::GenaiMessages | IoShape::IndexedMessages) {
        // As stored, byte for byte: the normalizer keeps the message text verbatim.
        return (String::from_utf8_lossy(body).into_owned(), false);
    }
    let mut out = Vec::new();
    value.write(&mut out);
    (String::from_utf8_lossy(&out).into_owned(), wrap)
}

/// Whether a payload field was cut by a cap.
fn is_cut(core: &SpanCore, payload: &SpanPayload, field: PayloadField) -> bool {
    payload.truncated_fields.contains(field)
        || match field {
            PayloadField::Input => core.input_truncation == Truncation::CapExceeded,
            PayloadField::Output => core.output_truncation == Truncation::CapExceeded,
            _ => false,
        }
}

/// The attributes of the span.
fn span_attributes(record: &ExportRecord) -> Vec<u8> {
    let (core, payload) = (&record.span.core, &record.span.payload);
    let cut = |field| is_cut(core, payload, field);
    let mut attrs = Attrs::new();
    let residual: Vec<String> = match Json::parse(&payload.attributes) {
        Some(Json::Obj(members)) if !cut(PayloadField::Attributes) => {
            members.into_iter().map(|(key, _)| key).collect()
        }
        _ => Vec::new(),
    };
    let residual_has = |key: &str| residual.iter().any(|k| k == key);

    attrs.text("crucible.observation.type", core.observation_type.as_str());
    if let Some(name) =
        operation(core.observation_type).filter(|_| !residual_has("gen_ai.operation.name"))
    {
        attrs.text("gen_ai.operation.name", name);
    }
    attrs.text("gen_ai.request.model", &core.model_requested);
    attrs.text("gen_ai.response.model", &core.model_response);
    attrs.text("gen_ai.provider.name", &core.provider);
    attrs.texts("gen_ai.response.finish_reasons", &core.finish_reasons);
    for (key, units) in &payload.usage_provided {
        if let Some((_, name)) = USAGE_KEYS.iter().find(|(k, _)| *k == &**key) {
            attrs.put(name, &Json::Int(i64::try_from(*units).unwrap_or(i64::MAX)));
        }
    }

    // Input and output. A body of a tool span or an MCP operation that is the call's arguments or result goes out
    // under the tool key (contract 9.1 carries an operation's arguments and result there): as the captured
    // content says when there is some, else when the body is raw or no message key could have held it. A tool
    // key left in the residual is one the span did not take.
    let tool_like = core.observation_type == ObservationType::Tool || record.mcp.is_some();
    let content = record.mcp.as_ref().and_then(|op| op.content.as_deref());
    let mut io_wrapped = false;
    for (field, body, role, messages_key, tool_key, content_column) in [
        (
            PayloadField::Input,
            &payload.input,
            "user",
            "gen_ai.input.messages",
            "gen_ai.tool.call.arguments",
            "args",
        ),
        (
            PayloadField::Output,
            &payload.output,
            "assistant",
            "gen_ai.output.messages",
            "gen_ai.tool.call.result",
            "result",
        ),
    ] {
        if body.is_empty() {
            continue;
        }
        let message_list = matches!(Json::parse(body), Some(Json::Arr(_)));
        let call = tool_like
            && !cut(field)
            && !residual_has(tool_key)
            && match content {
                Some(row) => mcp::content(row, content_column).as_bytes() == &body[..],
                None => core.io_shape == IoShape::Raw || !message_list,
            };
        if call {
            // The call's arguments and result as stored, which the native key reads back byte for byte.
            attrs.text(tool_key, &String::from_utf8_lossy(body));
        } else if tool_like && !cut(field) && message_list && core.io_shape == IoShape::Raw {
            // A message list beside the call's raw arguments or result, as stored.
            attrs.text(messages_key, &String::from_utf8_lossy(body));
        } else {
            let (text, wrap) = messages(core.io_shape, role, body, cut(field));
            io_wrapped |= wrap;
            attrs.put(messages_key, &Json::Str(text));
        }
    }
    if io_wrapped {
        attrs.text("crucible.io_shape", core.io_shape.as_str());
    }
    for (field, key) in [
        (
            PayloadField::SystemInstructions,
            "gen_ai.system_instructions",
        ),
        (PayloadField::ToolDefinitions, "gen_ai.tool.definitions"),
    ] {
        let body = payload.field(field);
        if body.is_empty() {
            continue;
        }
        attrs.text(key, &String::from_utf8_lossy(body));
    }
    if core.observation_type == ObservationType::Tool
        && let Some(name) = core.tool_names.first()
    {
        attrs.text("gen_ai.tool.name", name);
    }
    if !cut(PayloadField::ModelParameters)
        && let Some(Json::Obj(members)) = Json::parse(&payload.model_parameters)
    {
        for (key, value) in &members {
            let name = if key == "model" {
                "crucible.model_parameters.model".to_owned()
            } else {
                format!("gen_ai.request.{key}")
            };
            attrs.put(&name, value);
        }
    }

    // The fields without a semantic convention.
    attrs.text("crucible.trace.name", &core.trace_name);
    attrs.text("crucible.session.id", &core.session_id);
    // An MCP operation's `user.id` (below) also gives the span its user; the native key would win over it and
    // leave it in the residual.
    let user_from_mcp = !residual_has("user.id")
        && record
            .mcp
            .as_ref()
            .is_some_and(|op| *mcp::user_id(op) == *core.user_id);
    if !user_from_mcp {
        attrs.text("crucible.user.id", &core.user_id);
    }
    if &*core.environment != "default" {
        attrs.text("crucible.environment", &core.environment);
    }
    attrs.text("crucible.release", &core.release);
    attrs.text("crucible.version", &core.version);
    attrs.texts("crucible.tags", &core.tags);
    attrs.texts("crucible.trace.tags", &core.trace_tags);
    match core.trace_public {
        TracePublic::Unset => {}
        public => attrs.put(
            "crucible.trace.public",
            &Json::Bool(public == TracePublic::Public),
        ),
    }
    if core.level != derived_level(core) {
        attrs.text("crucible.level", core.level.as_str());
    }
    attrs.text("crucible.prompt.name", &core.prompt_name);
    attrs.text("crucible.prompt.version", &core.prompt_version);
    if let Some(t) = core.completion_start_time {
        attrs.text("crucible.completion_start_time", &t.to_string());
    }
    for (key, nanos) in &payload.cost_provided {
        attrs.text(&format!("crucible.cost.{key}"), &usd_text(nanos.nanos()));
    }
    if !cut(PayloadField::Metadata) {
        for (key, value) in payload.metadata_keys.iter().zip(&payload.metadata_values) {
            let value = match Json::parse(value.as_bytes()) {
                Some(Json::Str(_)) | None => Json::Str(value.to_string()),
                Some(parsed) => parsed,
            };
            attrs.put(&format!("crucible.metadata.{key}"), &value);
        }
    }
    if !cut(PayloadField::TraceMetadata)
        && let Some(Json::Obj(members)) = Json::parse(&payload.trace_metadata)
    {
        for (key, value) in &members {
            attrs.put(&format!("crucible.trace.metadata.{key}"), value);
        }
    }
    if let Some(op) = &record.mcp {
        let mut line = mcp::Line {
            attrs: &mut attrs,
            residual: &residual,
        };
        mcp::attributes(&mut line, core, op);
    }
    let cut_maps: Vec<&str> = [
        PayloadField::Attributes,
        PayloadField::ResourceAttributes,
        PayloadField::Events,
        PayloadField::Links,
        PayloadField::Metadata,
        PayloadField::TraceMetadata,
        PayloadField::ModelParameters,
        PayloadField::ToolCalls,
    ]
    .into_iter()
    .filter(|f| cut(*f))
    .map(PayloadField::name)
    .collect();
    attrs.texts("crucible.truncated_fields", &cut_maps);
    if !cut(PayloadField::Attributes) {
        attrs.residual(&payload.attributes);
    }
    attrs.finish()
}

fn resource_attributes(record: &ExportRecord) -> Vec<u8> {
    let (core, payload) = (&record.span.core, &record.span.payload);
    let mut attrs = Attrs::new();
    attrs.text("service.name", &core.service_name);
    attrs.text("service.version", &core.service_version);
    attrs.text("telemetry.sdk.name", &core.telemetry_sdk_name);
    attrs.text("telemetry.sdk.language", &core.telemetry_sdk_language);
    attrs.text("telemetry.sdk.version", &core.telemetry_sdk_version);
    if !is_cut(core, payload, PayloadField::ResourceAttributes) {
        attrs.residual(&payload.resource_attributes);
    }
    attrs.finish()
}

/// The attribute list of a stored `attributes` member (of an event or a link).
fn member_attributes(value: Option<&Json>) -> Vec<u8> {
    let mut attrs = Attrs::new();
    if let Some(Json::Obj(members)) = value {
        for (key, value) in members {
            attrs.put(key, value);
        }
    }
    attrs.finish()
}

fn member<'a>(members: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    members
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn member_str<'a>(members: &'a [(String, Json)], name: &str) -> &'a str {
    match member(members, name) {
        Some(Json::Str(s)) => s,
        _ => "",
    }
}

fn events(out: &mut Vec<u8>, stored: &[u8]) {
    out.push(b'[');
    if let Some(Json::Arr(items)) = Json::parse(stored) {
        let mut first = true;
        for item in &items {
            let Json::Obj(members) = item else { continue };
            if !first {
                out.push(b',');
            }
            first = false;
            let mut event = Obj::new(out);
            let time =
                Timestamp::parse_rfc3339(member_str(members, "time")).unwrap_or(Timestamp::EPOCH);
            event.str("timeUnixNano", &nanos(time));
            event.str("name", member_str(members, "name"));
            event.raw(
                "attributes",
                &member_attributes(member(members, "attributes")),
            );
            event.end();
        }
    }
    out.push(b']');
}

fn links(out: &mut Vec<u8>, stored: &[u8]) {
    out.push(b'[');
    if let Some(Json::Arr(items)) = Json::parse(stored) {
        let mut first = true;
        for item in &items {
            let Json::Obj(members) = item else { continue };
            if !first {
                out.push(b',');
            }
            first = false;
            let mut link = Obj::new(out);
            link.str("traceId", member_str(members, "trace_id"));
            link.str("spanId", member_str(members, "span_id"));
            link.raw(
                "attributes",
                &member_attributes(member(members, "attributes")),
            );
            link.end();
        }
    }
    out.push(b']');
}

/// Appends the line of one span, or nothing when the span has a field a cap cut and `keep_truncated` is false.
/// The project is the reads' `scope`; an OTLP document carries no project, so the scope only guards that the
/// record belongs to it.
pub fn write_otlp(
    scope: ProjectScope,
    record: &ExportRecord,
    keep_truncated: bool,
    out: &mut Vec<u8>,
) -> Written {
    let (core, payload) = (&record.span.core, &record.span.payload);
    let truncated = !payload.truncated_fields.is_empty()
        || core.input_truncation == Truncation::CapExceeded
        || core.output_truncation == Truncation::CapExceeded
        || record.mcp.as_ref().is_some_and(mcp::content_cut);
    if (truncated && !keep_truncated) || core.project_id != scope.project_id() {
        return Written::Skipped;
    }
    let mut doc = Obj::new(out);
    let spans = doc.key("resourceSpans");
    spans.push(b'[');
    let mut resource_spans = Obj::new(spans);
    {
        let mut resource = Obj::new(resource_spans.key("resource"));
        resource.raw("attributes", &resource_attributes(record));
        resource.end();
    }
    let scopes = resource_spans.key("scopeSpans");
    scopes.push(b'[');
    let mut scope_spans = Obj::new(scopes);
    {
        let mut instrumentation = Obj::new(scope_spans.key("scope"));
        instrumentation.str("name", &core.scope_name);
        instrumentation.str("version", &core.scope_version);
        instrumentation.end();
    }
    let list = scope_spans.key("spans");
    list.push(b'[');
    let mut span = Obj::new(list);
    span.display("traceId", core.trace_id);
    span.display("spanId", core.span_id);
    if core.parent_span_id != SpanId::ZERO {
        span.display("parentSpanId", core.parent_span_id);
    }
    if !payload.trace_state.is_empty() {
        span.str("traceState", &String::from_utf8_lossy(&payload.trace_state));
    }
    span.str("name", &core.name);
    json::uint(span.key("kind"), u64::from(core.kind.value()));
    span.str("startTimeUnixNano", &nanos(core.start_time));
    span.str("endTimeUnixNano", &nanos(core.end_time));
    span.raw("attributes", &span_attributes(record));
    if !payload.events.is_empty() && !is_cut(core, payload, PayloadField::Events) {
        events(span.key("events"), &payload.events);
    }
    if !payload.links.is_empty() && !is_cut(core, payload, PayloadField::Links) {
        links(span.key("links"), &payload.links);
    }
    {
        let mut status = Obj::new(span.key("status"));
        json::uint(status.key("code"), u64::from(core.status_code.value()));
        if !core.status_message.is_empty() {
            status.str("message", &core.status_message);
        }
        status.end();
    }
    span.end();
    list.push(b']');
    scope_spans.end();
    scopes.push(b']');
    resource_spans.end();
    spans.push(b']');
    doc.end();
    out.push(b'\n');
    Written::Line
}
