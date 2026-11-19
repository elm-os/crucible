//! Pre-encoded OTLP request pools.
//!
//! A pool holds a few template requests. Each template is encoded once with prost, with random marker values
//! in the fields that differ between requests; the markers' byte offsets are found once, and every request is
//! a copy of a template with those fields overwritten. The overwritten fields are fixed-width (`trace_id`,
//! `span_id`, `parent_span_id`, the `fixed64` times and a 20-digit sequence string), so a request is exactly
//! as long as its template and no length prefix ever changes. Each template is checked once at build time:
//! patching it must give byte for byte what prost encodes for the same values.
//!
//! Content (span names, kinds, durations and payload text) is a function of the seed alone. Identity is a
//! function of the run id and the request index; request `index` uses template `index % templates`.
//!
//! A request holds `traces_per_request` traces: its spans split into runs of consecutive positions, one per
//! trace, whose lengths differ by at most one. The first span of each run is that trace's root:
//!
//! | Field | Value |
//! |---|---|
//! | `trace_id` | the run tag (8 bytes) then `mix64(index * traces_per_request + trace + 1)` (8 bytes), big-endian |
//! | `span_id` | `mix64(seq + 1)`, big-endian |
//! | `parent_span_id` | the trace's first span, for every other span of the trace. With `rootless`, the first span names `mix64(!seq)` (bitwise not), which no span of the run carries, so no trace has a root |
//! | `start_time_unix_nano` | the request's start time plus the span's offset in the template |
//! | `loadgen.run_id` | the run id, canonical form |
//! | `loadgen.seq` | `seq = index * spans_per_request + position`, as 20 zero-padded decimal digits |
//!
//! `mix64` is a bijection on `u64` that maps only 0 to 0, so no id is ever zero and no trace id repeats
//! within a run. A sequence number stays below 2^48 (at most 2^32 requests of 50,000 spans), so `!seq` is never
//! `seq' + 1` for another span and a missing parent never names a span of the run. The run tag is the two
//! halves of the run id XORed together.

use std::fmt;
use std::str::FromStr;

use crucible_env::{ChaChaRng, Rng};
use crucible_records::{SpanId, TraceId, Uuid};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

/// The largest canonical OTLP body of one request: 64 MiB minus the 64 KiB envelope reserve (contract 4.3).
pub const MAX_BODY_BYTES: usize = 67_043_328;
/// The most spans one request may carry (contract 4.8).
pub const MAX_SPANS_PER_REQUEST: u32 = 50_000;
/// The span attribute that carries the run id.
pub const RUN_ID_KEY: &str = "loadgen.run_id";
/// The span attribute that carries the span's sequence number.
pub const SEQ_KEY: &str = "loadgen.seq";

const PAYLOAD_KEY: &str = "loadgen.payload";
const SEQ_DIGITS: usize = 20;
const SERVICE_NAME: &str = "crucible-loadgen";
/// Child spans start this far apart.
const CHILD_STAGGER_NANOS: u64 = 10_000;
/// The request start time used to check each template at build time (2026-01-01T00:00:00Z).
const CHECK_NANOS: u64 = 1_767_225_600_000_000_000;

const SPAN_NAMES: [&str; 6] = [
    "agent.step",
    "llm.chat",
    "tool.call",
    "retriever.query",
    "chain.run",
    "embedding.create",
];

/// Payload text is words from this list, so the gzip lane compresses roughly the way prose does.
const WORDS: [&str; 32] = [
    "agent", "tool", "model", "prompt", "token", "query", "result", "context", "server", "client",
    "search", "answer", "the", "a", "of", "to", "and", "with", "from", "request", "response",
    "document", "summary", "plan", "step", "memory", "user", "system", "message", "function",
    "value", "error",
];

/// The encoded size of each span in a pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpanSize {
    /// 1,024-byte spans (`1k`).
    Kib1,
    /// 10,240-byte spans (`10k`).
    Kib10,
}

impl SpanSize {
    /// The encoded length of one `Span` message (without its tag and length prefix), in bytes.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Kib1 => 1_024,
            Self::Kib10 => 10_240,
        }
    }
}

/// A span size other than `1k` or `10k`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("span size must be 1k or 10k")]
pub struct SpanSizeError;

impl FromStr for SpanSize {
    type Err = SpanSizeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1k" => Ok(Self::Kib1),
            "10k" => Ok(Self::Kib10),
            _ => Err(SpanSizeError),
        }
    }
}

impl fmt::Display for SpanSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Kib1 => "1k",
            Self::Kib10 => "10k",
        })
    }
}

/// What a pool holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolConfig {
    /// The encoded size of each span.
    pub span_size: SpanSize,
    /// Spans in each request, at most [`MAX_SPANS_PER_REQUEST`].
    pub spans_per_request: u32,
    /// Traces in each request, from 1 to `spans_per_request`.
    pub traces_per_request: u32,
    /// The first span of every trace names a parent that no span carries, so no trace has a root.
    pub rootless: bool,
    /// Distinct template requests; memory use is about `templates * spans_per_request * span bytes`.
    pub templates: usize,
    /// Fixes the content of every template.
    pub seed: u64,
}

/// Why a pool could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PoolError {
    /// Spans per request outside `1..=MAX_SPANS_PER_REQUEST`.
    #[error("spans per request must be between 1 and {MAX_SPANS_PER_REQUEST}, got {0}")]
    SpansPerRequest(u32),
    /// Traces per request outside `1..=spans_per_request`.
    #[error("traces per request must be between 1 and the spans per request, got {0}")]
    TracesPerRequest(u32),
    /// A pool needs at least one template.
    #[error("a pool needs at least one template")]
    NoTemplates,
    /// A request body would exceed [`MAX_BODY_BYTES`].
    #[error("a request of {0} bytes exceeds the {MAX_BODY_BYTES}-byte body cap")]
    TooLarge(usize),
    /// A marker was not found in the encoded template.
    #[error("template {template}: the {field} slot was not found")]
    SlotNotFound {
        /// The field whose marker is missing.
        field: &'static str,
        /// The template index.
        template: usize,
    },
    /// Patching a template did not give the bytes prost encodes for the same values.
    #[error("template {0}: patched bytes differ from the encoded request")]
    PatchMismatch(usize),
}

/// The byte offsets of one span's patched fields in a template, the span's place in its trace and its timing.
#[derive(Clone, Copy, Debug)]
struct Slots {
    trace_id: usize,
    span_id: usize,
    parent_span_id: Option<usize>,
    start: usize,
    end: usize,
    seq: usize,
    /// The trace of the span within its request, from 0.
    trace: u32,
    /// The position of the trace's first span; the span's own position when it is that span.
    root: usize,
    offset_nanos: u64,
    duration_nanos: u64,
}

#[derive(Debug)]
struct Template {
    body: Vec<u8>,
    spans: Vec<Slots>,
}

/// The random stand-ins encoded into a template's patched fields, found again by byte search.
#[derive(Clone, Copy, Debug)]
struct Markers {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: [u8; 8],
    start: [u8; 8],
    end: [u8; 8],
    seq: [u8; SEQ_DIGITS],
}

impl Markers {
    fn draw(rng: &dyn Rng) -> Self {
        let mut markers = Self {
            trace_id: [0; 16],
            span_id: [0; 8],
            parent_span_id: [0; 8],
            start: [0; 8],
            end: [0; 8],
            seq: [0; SEQ_DIGITS],
        };
        rng.fill_bytes(&mut markers.trace_id);
        rng.fill_bytes(&mut markers.span_id);
        rng.fill_bytes(&mut markers.parent_span_id);
        rng.fill_bytes(&mut markers.start);
        rng.fill_bytes(&mut markers.end);
        // A zero `fixed64` is the proto3 default and would not be encoded at all.
        markers.start[7] |= 0x80;
        markers.end[7] |= 0x80;
        for digit in &mut markers.seq {
            *digit = b'0' + (rng.next_u64() % 10) as u8;
        }
        markers
    }
}

/// Pre-encoded request templates for one run; see the module documentation.
#[derive(Debug)]
pub struct Pool {
    config: PoolConfig,
    run_id: Uuid,
    run_tag: [u8; 8],
    templates: Vec<Template>,
}

impl Pool {
    /// Encodes `config.templates` templates for the run `run_id`. Content depends only on `config.seed`.
    pub fn build(config: PoolConfig, run_id: Uuid) -> Result<Self, PoolError> {
        if config.spans_per_request == 0 || config.spans_per_request > MAX_SPANS_PER_REQUEST {
            return Err(PoolError::SpansPerRequest(config.spans_per_request));
        }
        if config.traces_per_request == 0 || config.traces_per_request > config.spans_per_request {
            return Err(PoolError::TracesPerRequest(config.traces_per_request));
        }
        if config.templates == 0 {
            return Err(PoolError::NoTemplates);
        }
        let estimate = config
            .span_size
            .bytes()
            .saturating_mul(config.spans_per_request as usize);
        if estimate > MAX_BODY_BYTES {
            return Err(PoolError::TooLarge(estimate));
        }
        let halves = u128::from_be_bytes(run_id.to_bytes());
        let mut pool = Self {
            config,
            run_id,
            run_tag: (((halves >> 64) as u64) ^ (halves as u64)).to_be_bytes(),
            templates: Vec::with_capacity(config.templates),
        };
        let content = ChaChaRng::from_u64(config.seed);
        let markers = ChaChaRng::from_u64(config.seed).fork("markers");
        for index in 0..config.templates {
            let label = format!("template/{index}");
            let template = pool.template(
                index,
                content.fork(&label).as_ref(),
                markers.fork(&label).as_ref(),
            )?;
            pool.templates.push(template);
        }
        Ok(pool)
    }

    /// The pool's configuration.
    pub fn config(&self) -> PoolConfig {
        self.config
    }

    /// The run every request of this pool belongs to.
    pub fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// The protobuf body of request `index`, with span times starting at `start_unix_nanos`.
    pub fn request(&self, index: u64, start_unix_nanos: u64) -> Vec<u8> {
        let template = &self.templates[(index % self.templates.len() as u64) as usize];
        let mut body = template.body.clone();
        self.patch(&mut body, &template.spans, index, start_unix_nanos);
        body
    }

    /// The trace id of trace `trace` (from 0) of request `index`.
    pub fn trace_id(&self, index: u64, trace: u32) -> TraceId {
        let number = index
            .wrapping_mul(u64::from(self.config.traces_per_request))
            .wrapping_add(u64::from(trace));
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&self.run_tag);
        bytes[8..].copy_from_slice(&mix64(number.wrapping_add(1)).to_be_bytes());
        TraceId::from_bytes(bytes)
    }

    /// The trace, from 0, of the span at `position` in a request.
    pub fn trace_of(&self, position: usize) -> u32 {
        let trace = position as u64 * u64::from(self.config.traces_per_request)
            / u64::from(self.config.spans_per_request);
        u32::try_from(trace).unwrap_or(u32::MAX)
    }

    /// The sequence number of the span at `position` in request `index`.
    pub fn seq(&self, index: u64, position: usize) -> u64 {
        index
            .saturating_mul(u64::from(self.config.spans_per_request))
            .saturating_add(position as u64)
    }

    /// The span id of the span with sequence number `seq`.
    pub fn span_id(seq: u64) -> SpanId {
        SpanId::from_bytes(mix64(seq.wrapping_add(1)).to_be_bytes())
    }

    /// The parent that the first span of a trace names in a rootless pool: a span id that no span of the run
    /// carries.
    pub fn missing_parent(seq: u64) -> SpanId {
        SpanId::from_bytes(mix64(!seq).to_be_bytes())
    }

    /// The parent of the span at `position` of request `index`, whose trace starts at `root`.
    fn parent(&self, index: u64, position: usize, root: usize) -> SpanId {
        if position == root {
            Self::missing_parent(self.seq(index, position))
        } else {
            Self::span_id(self.seq(index, root))
        }
    }

    fn template(
        &self,
        index: usize,
        content: &dyn Rng,
        marker_rng: &dyn Rng,
    ) -> Result<Template, PoolError> {
        let spans_per_request = self.config.spans_per_request as usize;
        let mut spans = Vec::with_capacity(spans_per_request);
        let mut markers = Vec::with_capacity(spans_per_request);
        let mut places = Vec::with_capacity(spans_per_request);
        let mut root = 0;
        for position in 0..spans_per_request {
            let marker = Markers::draw(marker_rng);
            let trace = self.trace_of(position);
            let is_root = position == 0 || self.trace_of(position - 1) != trace;
            if is_root {
                root = position;
            }
            let offset = position as u64 * CHILD_STAGGER_NANOS;
            let duration = if is_root {
                // The root lasts past every span of its trace.
                let length = (position..spans_per_request)
                    .take_while(|p| self.trace_of(*p) == trace)
                    .count();
                length as u64 * CHILD_STAGGER_NANOS + 60_000_000
            } else {
                1_000_000 + content.next_u64() % 49_000_000
            };
            let kind = if is_root {
                SpanKind::Server
            } else {
                SpanKind::Internal
            };
            let has_parent = !is_root || self.config.rootless;
            let mut span = Span {
                trace_id: marker.trace_id.to_vec(),
                span_id: marker.span_id.to_vec(),
                parent_span_id: if has_parent {
                    marker.parent_span_id.to_vec()
                } else {
                    Vec::new()
                },
                name: pick(content, &SPAN_NAMES).to_owned(),
                kind: kind as i32,
                start_time_unix_nano: u64::from_le_bytes(marker.start),
                end_time_unix_nano: u64::from_le_bytes(marker.end),
                attributes: vec![
                    string_attribute(RUN_ID_KEY, self.run_id.to_string()),
                    string_attribute(SEQ_KEY, marker.seq.iter().map(|&d| char::from(d)).collect()),
                    string_attribute(PAYLOAD_KEY, String::new()),
                ],
                ..Default::default()
            };
            fill_payload(&mut span, self.config.span_size.bytes(), content);
            spans.push(span);
            markers.push(marker);
            places.push((trace, root, has_parent, offset, duration));
        }
        let message = request_message(spans);
        let body = message.encode_to_vec();
        if body.len() > MAX_BODY_BYTES {
            return Err(PoolError::TooLarge(body.len()));
        }

        // Prost writes a span's fields in tag order, so the markers appear in this order in every span.
        let mut slots = Vec::with_capacity(spans_per_request);
        let mut cursor = 0;
        for (marker, (trace, root, has_parent, offset_nanos, duration_nanos)) in
            markers.iter().zip(places)
        {
            let found = |from: usize, needle: &[u8], field: &'static str| {
                find(&body, from, needle).ok_or(PoolError::SlotNotFound {
                    field,
                    template: index,
                })
            };
            let trace_id = found(cursor, &marker.trace_id, "trace_id")?;
            let span_id = found(trace_id + 16, &marker.span_id, "span_id")?;
            let mut next = span_id + 8;
            let parent_span_id = if has_parent {
                let at = found(next, &marker.parent_span_id, "parent_span_id")?;
                next = at + 8;
                Some(at)
            } else {
                None
            };
            let start = found(next, &marker.start, "start_time_unix_nano")?;
            let end = found(start + 8, &marker.end, "end_time_unix_nano")?;
            let seq = found(end + 8, &marker.seq, SEQ_KEY)?;
            cursor = seq + SEQ_DIGITS;
            slots.push(Slots {
                trace_id,
                span_id,
                parent_span_id,
                start,
                end,
                seq,
                trace,
                root,
                offset_nanos,
                duration_nanos,
            });
        }

        let check_index = index as u64;
        let mut patched = body.clone();
        self.patch(&mut patched, &slots, check_index, CHECK_NANOS);
        let expected = self.identified(message, &slots, check_index, CHECK_NANOS);
        if patched != expected.encode_to_vec() {
            return Err(PoolError::PatchMismatch(index));
        }
        Ok(Template { body, spans: slots })
    }

    /// Overwrites the patched fields of a template body for request `index`.
    fn patch(&self, body: &mut [u8], slots: &[Slots], index: u64, start_unix_nanos: u64) {
        let mut trace = (u32::MAX, TraceId::from_bytes([0; 16]));
        for (position, slot) in slots.iter().enumerate() {
            let seq = self.seq(index, position);
            if trace.0 != slot.trace {
                trace = (slot.trace, self.trace_id(index, slot.trace));
            }
            body[slot.trace_id..slot.trace_id + 16].copy_from_slice(trace.1.as_bytes());
            body[slot.span_id..slot.span_id + 8].copy_from_slice(Self::span_id(seq).as_bytes());
            if let Some(at) = slot.parent_span_id {
                let parent = self.parent(index, position, slot.root);
                body[at..at + 8].copy_from_slice(parent.as_bytes());
            }
            let start = start_unix_nanos.saturating_add(slot.offset_nanos);
            body[slot.start..slot.start + 8].copy_from_slice(&start.to_le_bytes());
            let end = start.saturating_add(slot.duration_nanos);
            body[slot.end..slot.end + 8].copy_from_slice(&end.to_le_bytes());
            write_digits(&mut body[slot.seq..slot.seq + SEQ_DIGITS], seq);
        }
    }

    /// The template message with the real field values of request `index`: what [`Pool::patch`] must produce.
    fn identified(
        &self,
        mut message: ExportTraceServiceRequest,
        slots: &[Slots],
        index: u64,
        start_unix_nanos: u64,
    ) -> ExportTraceServiceRequest {
        let spans = message
            .resource_spans
            .iter_mut()
            .flat_map(|r| r.scope_spans.iter_mut())
            .flat_map(|s| s.spans.iter_mut());
        for (position, (span, slot)) in spans.zip(slots).enumerate() {
            let seq = self.seq(index, position);
            span.trace_id = self.trace_id(index, slot.trace).as_bytes().to_vec();
            span.span_id = Self::span_id(seq).as_bytes().to_vec();
            if !span.parent_span_id.is_empty() {
                span.parent_span_id = self.parent(index, position, slot.root).as_bytes().to_vec();
            }
            span.start_time_unix_nano = start_unix_nanos.saturating_add(slot.offset_nanos);
            span.end_time_unix_nano = span
                .start_time_unix_nano
                .saturating_add(slot.duration_nanos);
            if let Some(attribute) = span.attributes.get_mut(1) {
                *attribute = string_attribute(SEQ_KEY, format!("{seq:020}"));
            }
        }
        message
    }
}

/// A bijection on `u64` (the splitmix64 finalizer). Only 0 maps to 0.
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn write_digits(out: &mut [u8], mut value: u64) {
    for digit in out.iter_mut().rev() {
        *digit = b'0' + (value % 10) as u8;
        value /= 10;
    }
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

fn pick<'a>(rng: &dyn Rng, items: &[&'a str]) -> &'a str {
    items[(rng.next_u64() % items.len() as u64) as usize]
}

fn string_attribute(key: &str, value: String) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value)),
        }),
        ..Default::default()
    }
}

/// Sizes the last attribute (the payload) so that the span encodes to exactly `target` bytes, or as close as
/// the length prefixes allow.
fn fill_payload(span: &mut Span, target: usize, rng: &dyn Rng) {
    let guess = target.saturating_sub(span.encoded_len());
    let mut text = String::with_capacity(guess + 32);
    while text.len() < guess + 16 {
        text.push_str(pick(rng, &WORDS));
        text.push(' ');
    }
    let mut len = guess;
    for _ in 0..4 {
        if let Some(payload) = span.attributes.last_mut() {
            // The text is ASCII, so every byte offset is a character boundary.
            *payload = string_attribute(PAYLOAD_KEY, text[..len.min(text.len())].to_owned());
        }
        let actual = span.encoded_len();
        if actual == target {
            return;
        }
        len = (len + target).saturating_sub(actual);
    }
}

fn request_message(spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string_attribute("service.name", SERVICE_NAME.to_owned())],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: SERVICE_NAME.to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    ..Default::default()
                }),
                spans,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    const RUN: Uuid = Uuid::from_v7_parts(1_767_225_600_000, 0x1234_5678_9abc_def0_1234);

    fn config(
        span_size: SpanSize,
        spans_per_request: u32,
        templates: usize,
        seed: u64,
    ) -> PoolConfig {
        PoolConfig {
            span_size,
            spans_per_request,
            traces_per_request: 1,
            rootless: false,
            templates,
            seed,
        }
    }

    fn spans(body: &[u8]) -> Vec<Span> {
        let request = ExportTraceServiceRequest::decode(body).unwrap();
        request
            .resource_spans
            .into_iter()
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .collect()
    }

    fn string_value(span: &Span, key: &str) -> String {
        let attribute = span.attributes.iter().find(|a| a.key == key).unwrap();
        match attribute.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(Value::StringValue(s)) => s.clone(),
            other => panic!("{key} is not a string: {other:?}"),
        }
    }

    #[test]
    fn every_span_encodes_to_exactly_the_requested_size() {
        for size in [SpanSize::Kib1, SpanSize::Kib10] {
            let pool = Pool::build(config(size, 7, 3, 1), RUN).unwrap();
            for index in 0..6 {
                for span in spans(&pool.request(index, CHECK_NANOS)) {
                    assert_eq!(span.encoded_len(), size.bytes(), "{size}");
                }
            }
        }
    }

    #[test]
    fn content_depends_on_the_seed_and_identity_on_the_run() {
        let a = Pool::build(config(SpanSize::Kib1, 4, 2, 9), RUN).unwrap();
        let b = Pool::build(config(SpanSize::Kib1, 4, 2, 9), RUN).unwrap();
        let other_seed = Pool::build(config(SpanSize::Kib1, 4, 2, 10), RUN).unwrap();
        let other_run = Pool::build(
            config(SpanSize::Kib1, 4, 2, 9),
            Uuid::from_v7_parts(1_767_225_600_000, 77),
        )
        .unwrap();
        for index in 0..4 {
            let body = a.request(index, CHECK_NANOS);
            assert_eq!(body, b.request(index, CHECK_NANOS));
            assert_ne!(body, other_seed.request(index, CHECK_NANOS));

            let (mine, theirs) = (spans(&body), spans(&other_run.request(index, CHECK_NANOS)));
            for (x, y) in mine.iter().zip(&theirs) {
                assert_eq!(string_value(x, PAYLOAD_KEY), string_value(y, PAYLOAD_KEY));
                assert_eq!(x.name, y.name);
                assert_ne!(x.trace_id, y.trace_id);
                assert_ne!(string_value(x, RUN_ID_KEY), string_value(y, RUN_ID_KEY));
            }
        }
    }

    #[test]
    fn trace_ids_never_repeat_within_a_run() {
        let pool = Pool::build(config(SpanSize::Kib1, 1, 1, 1), RUN).unwrap();
        let ids: BTreeSet<_> = (0..100_000u64).map(|i| pool.trace_id(i, 0)).collect();
        assert_eq!(ids.len(), 100_000);
        let span_ids: BTreeSet<_> = (0..100_000u64).map(Pool::span_id).collect();
        assert_eq!(span_ids.len(), 100_000);
        assert!(!span_ids.iter().any(SpanId::is_zero));
    }

    #[test]
    fn the_contract_caps_are_enforced() {
        let build = |spans, templates, size| Pool::build(config(size, spans, templates, 1), RUN);
        assert_eq!(
            build(0, 1, SpanSize::Kib1).unwrap_err(),
            PoolError::SpansPerRequest(0)
        );
        assert_eq!(
            build(50_001, 1, SpanSize::Kib1).unwrap_err(),
            PoolError::SpansPerRequest(50_001)
        );
        assert_eq!(
            build(1, 0, SpanSize::Kib1).unwrap_err(),
            PoolError::NoTemplates
        );
        assert!(matches!(
            build(7_000, 1, SpanSize::Kib10).unwrap_err(),
            PoolError::TooLarge(_)
        ));
    }

    #[test]
    fn traces_per_request_must_fit_the_spans() {
        let build = |spans, traces| {
            let config = PoolConfig {
                traces_per_request: traces,
                ..config(SpanSize::Kib1, spans, 1, 1)
            };
            Pool::build(config, RUN)
        };
        assert_eq!(build(4, 0).unwrap_err(), PoolError::TracesPerRequest(0));
        assert_eq!(build(4, 5).unwrap_err(), PoolError::TracesPerRequest(5));
        assert!(build(4, 4).is_ok() && build(1, 1).is_ok());
        // Every (request, trace) pair of a run gets its own trace id.
        let pool = build(9, 3).unwrap();
        let ids: BTreeSet<_> = (0..10_000u64)
            .flat_map(|index| (0..3).map(move |trace| (index, trace)))
            .map(|(index, trace)| pool.trace_id(index, trace))
            .collect();
        assert_eq!(ids.len(), 30_000);
    }

    #[test]
    fn span_sizes_parse_and_print() {
        for size in [SpanSize::Kib1, SpanSize::Kib10] {
            assert_eq!(size.to_string().parse::<SpanSize>(), Ok(size));
        }
        assert_eq!("2k".parse::<SpanSize>(), Err(SpanSizeError));
    }

    #[test]
    fn digits_are_zero_padded() {
        let mut out = [0; SEQ_DIGITS];
        write_digits(&mut out, 1_234);
        assert_eq!(&out, b"00000000000000001234");
        write_digits(&mut out, u64::MAX);
        assert_eq!(&out, b"18446744073709551615");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Any request decodes to spans that carry the run id, their sequence numbers, ids computed from both,
        /// one trace per request, and times offset from the requested start.
        #[test]
        fn patched_requests_carry_their_identity(
            seed: u64,
            spans_per_request in 1u32..12,
            templates in 1usize..4,
            index in 0u64..1_000_000_000,
            start in 1u64..4_000_000_000_000_000_000,
        ) {
            let pool = Pool::build(config(SpanSize::Kib1, spans_per_request, templates, seed), RUN).unwrap();
            let decoded = spans(&pool.request(index, start));
            prop_assert_eq!(decoded.len(), spans_per_request as usize);
            let root = Pool::span_id(pool.seq(index, 0));
            for (position, span) in decoded.iter().enumerate() {
                let seq = index * u64::from(spans_per_request) + position as u64;
                prop_assert_eq!(&span.trace_id, &pool.trace_id(index, 0).as_bytes().to_vec());
                prop_assert_eq!(&span.span_id, &Pool::span_id(seq).as_bytes().to_vec());
                let parent = if position == 0 { Vec::new() } else { root.as_bytes().to_vec() };
                prop_assert_eq!(&span.parent_span_id, &parent);
                prop_assert_eq!(string_value(span, SEQ_KEY), format!("{seq:020}"));
                prop_assert_eq!(string_value(span, RUN_ID_KEY), RUN.to_string());
                prop_assert!(span.start_time_unix_nano >= start);
                prop_assert!(span.end_time_unix_nano > span.start_time_unix_nano);
            }
        }

        /// A request of any shape decodes to `traces_per_request` traces of consecutive spans whose lengths differ
        /// by at most one. Each trace's first span is its root, or with `rootless` names a parent that no span of
        /// the request carries; every other span's parent is its trace's first span.
        #[test]
        fn patched_requests_follow_their_shape(
            seed: u64,
            (spans_per_request, traces) in (1u32..12).prop_flat_map(|s| (Just(s), 1..=s)),
            rootless: bool,
            index in 0u64..1_000_000_000,
        ) {
            let config = PoolConfig {
                traces_per_request: traces,
                rootless,
                ..config(SpanSize::Kib1, spans_per_request, 2, seed)
            };
            let pool = Pool::build(config, RUN).unwrap();
            let decoded = spans(&pool.request(index, CHECK_NANOS));
            prop_assert_eq!(decoded.len(), spans_per_request as usize);
            let ids: BTreeSet<&Vec<u8>> = decoded.iter().map(|span| &span.span_id).collect();
            let mut lengths = std::collections::BTreeMap::new();
            for (position, span) in decoded.iter().enumerate() {
                let trace = pool.trace_of(position);
                *lengths.entry(trace).or_insert(0usize) += 1;
                prop_assert_eq!(&span.trace_id, &pool.trace_id(index, trace).as_bytes().to_vec());
                let first = (0..=position).find(|p| pool.trace_of(*p) == trace).unwrap();
                if position != first {
                    let root = Pool::span_id(pool.seq(index, first));
                    prop_assert_eq!(&span.parent_span_id, &root.as_bytes().to_vec());
                } else if rootless {
                    let missing = Pool::missing_parent(pool.seq(index, position));
                    prop_assert_eq!(&span.parent_span_id, &missing.as_bytes().to_vec());
                    prop_assert!(!ids.contains(&span.parent_span_id));
                } else {
                    prop_assert!(span.parent_span_id.is_empty());
                }
            }
            prop_assert_eq!(lengths.len(), traces as usize);
            let longest = lengths.values().max().copied().unwrap_or(0);
            let shortest = lengths.values().min().copied().unwrap_or(0);
            prop_assert!(longest - shortest <= 1);
        }

        /// A missing parent never names a span of a run: sequence numbers stay below 2^48.
        #[test]
        fn missing_parents_name_no_span(a in 0u64..1 << 48, b in 0u64..1 << 48) {
            prop_assert_ne!(Pool::missing_parent(a), Pool::span_id(b));
            prop_assert!(!Pool::missing_parent(a).is_zero());
        }

        #[test]
        fn distinct_requests_get_distinct_trace_ids(a: u64, b: u64) {
            prop_assume!(a != b && a != u64::MAX && b != u64::MAX);
            let pool = Pool::build(config(SpanSize::Kib1, 1, 1, 1), RUN).unwrap();
            prop_assert_ne!(pool.trace_id(a, 0), pool.trace_id(b, 0));
            prop_assert!(!pool.trace_id(a, 0).is_zero());
        }
    }
}
