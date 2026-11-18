//! Span records (contract 3.1): the core tier, the payload tier and the payload truncation rule.
//!
//! # Representation
//!
//! The normalizer builds about 250,000 `SpanRecord`s per second, so each value is written once, at its final
//! size, and never grown:
//!
//! - Strings are `Box<str>`: one exact-size allocation with no spare capacity and 16 bytes inline. An empty value,
//!   the common case for optional fields, does not allocate. Values are cut to their caps before boxing.
//! - Lists are `Vec<Box<str>>`: short and capped, and an empty list does not allocate.
//! - Payload bodies are [`Bytes`]: a body can be a zero-copy slice of the decoded request, cutting it to a cap is
//!   O(1), and a clone shares the buffer instead of copying up to 8 MiB.
//! - Small maps (`usage`, `usage_provided`, `cost_provided`) are vectors with unique keys in ascending byte order.
//! - `truncated_fields` is a bit set ([`PayloadFields`]), not a list of strings.
//!
//! Derived columns (`duration_us`, `start_minute`, `trace_hash`) are methods, so they cannot disagree with the
//! fields they are computed from. A `span_payload` row takes its key (`project_id`, `trace_id`, `span_id`,
//! `start_time`), `user_id`, `session_id` and ingestion columns from the core tier, which holds the only copy.

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::caps;
use crate::cost::NanoUsd;
use crate::enums::{
    CostSource, IoShape, Level, McpSide, ObservationType, SourceFormat, SpanKind, StatusCode,
    TracePublic, Truncation,
};
use crate::id::{ApiKeyId, ProjectId, SpanId, TraceId, Uuid};
use crate::ingestion::Ingestion;
use crate::time::Timestamp;
use crate::truncate::{utf8_floor, utf8_prefix};

/// One normalized span: both tiers of the row identity `(project_id, trace_id, span_id)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanRecord {
    /// Narrow columns and previews, for lists and aggregates (`span_core`).
    pub core: SpanCore,
    /// Full values, for point reads and search (`span_payload`).
    pub payload: SpanPayload,
}

/// The core tier (`span_core`). Caps are the constants in [`caps`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanCore {
    /// From the credential only.
    pub project_id: ProjectId,
    /// OTel trace id.
    pub trace_id: TraceId,
    /// OTel span id.
    pub span_id: SpanId,
    /// Parent span id; [`SpanId::ZERO`] when absent.
    pub parent_span_id: SpanId,
    /// No parent, or the dialect's root flag.
    pub is_root: bool,
    /// The dialect's application-root flag.
    pub is_app_root: bool,
    /// OTel span kind.
    pub kind: SpanKind,
    /// Span name; [`caps::SPAN_NAME_BYTES`].
    pub name: Box<str>,
    /// Start of the span.
    pub start_time: Timestamp,
    /// End of the span, clamped to at least `start_time`.
    pub end_time: Timestamp,
    /// Time of the first output token.
    pub completion_start_time: Option<Timestamp>,
    /// OTel status code.
    pub status_code: StatusCode,
    /// OTel status message; [`caps::STATUS_MESSAGE_BYTES`].
    pub status_message: Box<str>,
    /// From the resource; [`caps::SERVICE_BYTES`].
    pub service_name: Box<str>,
    /// From the resource; [`caps::SERVICE_BYTES`].
    pub service_version: Box<str>,
    /// Instrumentation scope; [`caps::SERVICE_BYTES`].
    pub scope_name: Box<str>,
    /// Instrumentation scope; [`caps::SERVICE_BYTES`].
    pub scope_version: Box<str>,
    /// From the resource; [`caps::SDK_BYTES`].
    pub telemetry_sdk_name: Box<str>,
    /// From the resource; [`caps::SDK_BYTES`].
    pub telemetry_sdk_language: Box<str>,
    /// From the resource; [`caps::SDK_BYTES`].
    pub telemetry_sdk_version: Box<str>,
    /// From request headers or the resource; [`caps::SDK_BYTES`].
    pub sdk_name: Box<str>,
    /// From request headers or the resource; [`caps::SDK_BYTES`].
    pub sdk_version: Box<str>,
    /// Chosen by the observation-type priority rule (contract 10.4); default `span`.
    pub observation_type: ObservationType,
    /// The explicit level, else `error` when `status_code` is `error`, else `default`.
    pub level: Level,
    /// Default `default`; [`caps::ENVIRONMENT_BYTES`].
    pub environment: Box<str>,
    /// [`caps::RELEASE_BYTES`].
    pub release: Box<str>,
    /// [`caps::RELEASE_BYTES`].
    pub version: Box<str>,
    /// Trace-level (contract 3.3); [`caps::TRACE_ATTRIBUTE_BYTES`].
    pub trace_name: Box<str>,
    /// Trace-level (contract 3.3); [`caps::TRACE_ATTRIBUTE_BYTES`].
    pub user_id: Box<str>,
    /// Trace-level (contract 3.3); [`caps::TRACE_ATTRIBUTE_BYTES`].
    pub session_id: Box<str>,
    /// Trace-level (contract 3.3); [`caps::TAGS_MAX_ITEMS`] items of [`caps::TAG_BYTES`].
    pub trace_tags: Vec<Box<str>>,
    /// Ingest-time value; later edits are facts.
    pub trace_public: TracePublic,
    /// Span tags; [`caps::TAGS_MAX_ITEMS`] items of [`caps::TAG_BYTES`].
    pub tags: Vec<Box<str>>,
    /// [`caps::MODEL_BYTES`].
    pub model_requested: Box<str>,
    /// [`caps::MODEL_BYTES`].
    pub model_response: Box<str>,
    /// [`caps::MODEL_BYTES`].
    pub provider: Box<str>,
    /// [`caps::FINISH_REASONS_MAX_ITEMS`] items of [`caps::FINISH_REASON_BYTES`].
    pub finish_reasons: Vec<Box<str>>,
    /// Calculated usage with its per-key cost (the `usage` map and the aligned `cost` list), computed only from
    /// provided usage; [`caps::USAGE_MAX_KEYS`] keys in ascending byte order.
    pub usage: Vec<UsageEntry>,
    /// Scalar for aggregates.
    pub input_tokens: u64,
    /// Scalar for aggregates.
    pub output_tokens: u64,
    /// Scalar for aggregates.
    pub total_tokens: u64,
    /// The provided total, else the sum of provided parts, else the calculated cost (contract 3.4).
    pub total_cost_nusd: NanoUsd,
    /// Provided wins over calculated.
    pub cost_source: CostSource,
    /// The price row used.
    pub price_model_id: Option<Uuid>,
    /// The price tier used.
    pub price_tier_id: Option<Uuid>,
    /// Prompt reference; [`caps::PROMPT_NAME_BYTES`].
    pub prompt_name: Box<str>,
    /// Prompt reference; [`caps::PROMPT_VERSION_BYTES`].
    pub prompt_version: Box<str>,
    /// [`caps::TOOL_NAMES_MAX_ITEMS`] items of [`caps::TOOL_NAME_BYTES`].
    pub tool_names: Vec<Box<str>>,
    /// Tool calls made by the span.
    pub tool_call_count: u32,
    /// When not `none`, an `McpOpRecord` with this key exists.
    pub mcp_side: McpSide,
    /// Tells exports how to re-emit the bodies.
    pub io_shape: IoShape,
    /// Cut from the original input; [`caps::PREVIEW_BYTES`].
    pub input_preview: Box<str>,
    /// Cut from the original output; [`caps::PREVIEW_BYTES`].
    pub output_preview: Box<str>,
    /// Size of the original input, before any truncation.
    pub input_bytes: u64,
    /// Size of the original output, before any truncation.
    pub output_bytes: u64,
    /// Whether the stored input was cut.
    pub input_truncation: Truncation,
    /// Whether the stored output was cut.
    pub output_truncation: Truncation,
    /// SHA-256 of the original input; set only when it was cut.
    pub input_sha256: Option<[u8; 32]>,
    /// SHA-256 of the original output; set only when it was cut.
    pub output_sha256: Option<[u8; 32]>,
    /// How the span arrived.
    pub source_format: SourceFormat,
    /// Compat dialect id (contract 10); 0 is native.
    pub compat_dialect: u8,
    /// Version of the normalizer that built the record.
    pub normalizer_version: u32,
    /// The span's times were clamped or ran ahead of `received_at` (contract 1.2).
    pub clock_skew: bool,
    /// Where and when the WAL accepted the span.
    pub ingestion: Ingestion,
    /// The API key that sent the span.
    pub api_key_id: ApiKeyId,
    /// xxh3-64 of the normalized row excluding the ingestion group.
    pub row_digest: u64,
    /// Size of the span in the canonical request, saturating.
    pub event_bytes: u32,
}

impl SpanCore {
    /// `duration_us`: `end_time - start_time`, or zero when `end_time` is not after `start_time`.
    pub const fn duration_us(&self) -> u64 {
        let micros = self
            .end_time
            .micros()
            .saturating_sub(self.start_time.micros());
        if micros > 0 { micros as u64 } else { 0 }
    }

    /// `start_minute` (contract 6.1).
    pub const fn start_minute(&self) -> u32 {
        self.start_time.start_minute()
    }

    /// `trace_hash` (contract 6.1).
    pub fn trace_hash(&self) -> u32 {
        self.trace_id.trace_hash()
    }
}

/// One `usage` key with its units and its cost.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct UsageEntry {
    /// A normalized key ([`UsageEntry::INPUT`] and the other constants) or another provided key.
    pub key: Box<str>,
    /// Units, for example tokens.
    pub units: u64,
    /// The cost of these units; zero when the key has no price.
    pub cost: NanoUsd,
}

impl UsageEntry {
    /// Input units.
    pub const INPUT: &'static str = "input";
    /// Output units.
    pub const OUTPUT: &'static str = "output";
    /// Total units.
    pub const TOTAL: &'static str = "total";
    /// Units read from a cache.
    pub const CACHE_READ: &'static str = "cache_read";
    /// Units written to a cache.
    pub const CACHE_WRITE: &'static str = "cache_write";
    /// Reasoning units.
    pub const REASONING: &'static str = "reasoning";
}

/// The payload tier (`span_payload`) without the values it shares with the core tier.
///
/// Every body is stored verbatim from the source (JSON or UTF-8 text). A field named in `truncated_fields` was cut
/// and is opaque text, not valid JSON.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SpanPayload {
    /// Input body.
    pub input: Bytes,
    /// Output body.
    pub output: Bytes,
    /// System instructions, JSON.
    pub system_instructions: Bytes,
    /// User metadata, a JSON object.
    pub metadata: Bytes,
    /// Keys of `metadata`; [`caps::METADATA_MAX_KEYS`].
    pub metadata_keys: Vec<Box<str>>,
    /// Values of `metadata`, aligned with `metadata_keys`; JSON text for non-strings.
    pub metadata_values: Vec<Box<str>>,
    /// `key=value` items, each cut at [`caps::METADATA_KV_ITEM_BYTES`].
    pub metadata_kv: Vec<Box<str>>,
    /// Trace metadata, a JSON object (contract 3.3).
    pub trace_metadata: Bytes,
    /// Residual span attributes not consumed by mapping, a JSON object.
    pub attributes: Bytes,
    /// Residual resource attributes, a JSON object.
    pub resource_attributes: Bytes,
    /// JSON list of `{time, name, attributes}`; [`caps::EVENTS_MAX_ITEMS`].
    pub events: Bytes,
    /// JSON list of `{trace_id, span_id, attributes}`; [`caps::LINKS_MAX_ITEMS`].
    pub links: Bytes,
    /// Tool definitions, JSON.
    pub tool_definitions: Bytes,
    /// JSON list of `{id, name, arguments}`.
    pub tool_calls: Bytes,
    /// Model parameters, a JSON object; [`caps::MODEL_PARAMETERS_BYTES`].
    pub model_parameters: Bytes,
    /// Usage as sent by the client, never estimated; unique keys in ascending byte order.
    pub usage_provided: Vec<(Box<str>, u64)>,
    /// Provided costs (decimal USD inputs converted per contract 3.4); unique keys in ascending byte order.
    pub cost_provided: Vec<(Box<str>, NanoUsd)>,
    /// W3C trace state, text; [`caps::TRACE_STATE_BYTES`].
    pub trace_state: Bytes,
    /// Payload fields cut by either cap.
    pub truncated_fields: PayloadFields,
}

impl SpanPayload {
    /// The stored value of a payload field.
    pub const fn field(&self, field: PayloadField) -> &Bytes {
        match field {
            PayloadField::Input => &self.input,
            PayloadField::Output => &self.output,
            PayloadField::SystemInstructions => &self.system_instructions,
            PayloadField::Metadata => &self.metadata,
            PayloadField::TraceMetadata => &self.trace_metadata,
            PayloadField::Attributes => &self.attributes,
            PayloadField::ResourceAttributes => &self.resource_attributes,
            PayloadField::Events => &self.events,
            PayloadField::Links => &self.links,
            PayloadField::ToolDefinitions => &self.tool_definitions,
            PayloadField::ToolCalls => &self.tool_calls,
            PayloadField::ModelParameters => &self.model_parameters,
            PayloadField::TraceState => &self.trace_state,
        }
    }

    const fn field_mut(&mut self, field: PayloadField) -> &mut Bytes {
        match field {
            PayloadField::Input => &mut self.input,
            PayloadField::Output => &mut self.output,
            PayloadField::SystemInstructions => &mut self.system_instructions,
            PayloadField::Metadata => &mut self.metadata,
            PayloadField::TraceMetadata => &mut self.trace_metadata,
            PayloadField::Attributes => &mut self.attributes,
            PayloadField::ResourceAttributes => &mut self.resource_attributes,
            PayloadField::Events => &mut self.events,
            PayloadField::Links => &mut self.links,
            PayloadField::ToolDefinitions => &mut self.tool_definitions,
            PayloadField::ToolCalls => &mut self.tool_calls,
            PayloadField::ModelParameters => &mut self.model_parameters,
            PayloadField::TraceState => &mut self.trace_state,
        }
    }

    /// The sum of the stored payload fields, which the per-span cap bounds.
    pub fn stored_bytes(&self) -> usize {
        PayloadField::ALL
            .iter()
            .fold(0usize, |sum, &f| sum.saturating_add(self.field(f).len()))
    }
}

/// A payload body field that the caps can cut.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum PayloadField {
    /// `input`.
    Input,
    /// `output`.
    Output,
    /// `system_instructions`.
    SystemInstructions,
    /// `metadata`.
    Metadata,
    /// `trace_metadata`.
    TraceMetadata,
    /// `attributes`.
    Attributes,
    /// `resource_attributes`.
    ResourceAttributes,
    /// `events`.
    Events,
    /// `links`.
    Links,
    /// `tool_definitions`.
    ToolDefinitions,
    /// `tool_calls`.
    ToolCalls,
    /// `model_parameters`.
    ModelParameters,
    /// `trace_state`.
    TraceState,
}

impl PayloadField {
    /// Every payload body field, in declaration order.
    pub const ALL: [Self; 13] = [
        Self::Input,
        Self::Output,
        Self::SystemInstructions,
        Self::Metadata,
        Self::TraceMetadata,
        Self::Attributes,
        Self::ResourceAttributes,
        Self::Events,
        Self::Links,
        Self::ToolDefinitions,
        Self::ToolCalls,
        Self::ModelParameters,
        Self::TraceState,
    ];

    /// The order in which the per-span cap cuts fields (contract 3.1).
    pub const SPAN_CUT_ORDER: [Self; 11] = [
        Self::Events,
        Self::Links,
        Self::ResourceAttributes,
        Self::Attributes,
        Self::ToolDefinitions,
        Self::ToolCalls,
        Self::TraceMetadata,
        Self::Metadata,
        Self::SystemInstructions,
        Self::Output,
        Self::Input,
    ];

    /// The column name, which `truncated_fields` stores.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::SystemInstructions => "system_instructions",
            Self::Metadata => "metadata",
            Self::TraceMetadata => "trace_metadata",
            Self::Attributes => "attributes",
            Self::ResourceAttributes => "resource_attributes",
            Self::Events => "events",
            Self::Links => "links",
            Self::ToolDefinitions => "tool_definitions",
            Self::ToolCalls => "tool_calls",
            Self::ModelParameters => "model_parameters",
            Self::TraceState => "trace_state",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u8
    }
}

/// A set of payload fields: the `truncated_fields` column. It iterates in declaration order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct PayloadFields(u16);

impl PayloadFields {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// Adds a field.
    pub const fn insert(&mut self, field: PayloadField) {
        self.0 |= field.bit();
    }

    /// Whether the set holds `field`.
    pub const fn contains(self, field: PayloadField) -> bool {
        self.0 & field.bit() != 0
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The fields in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = PayloadField> {
        PayloadField::ALL
            .into_iter()
            .filter(move |&f| self.contains(f))
    }
}

/// The payload caps in force for a project (`ingest.max_field_bytes` and `ingest.max_span_bytes`), pinned by its
/// snapshot in the normalizer context.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PayloadCaps {
    /// Per-field cap.
    pub max_field_bytes: usize,
    /// Per-span cap over the sum of the payload fields.
    pub max_span_bytes: usize,
}

impl PayloadCaps {
    /// The deployment defaults: 8 MiB per field, 16 MiB per span.
    pub const DEFAULT: Self = Self {
        max_field_bytes: caps::MAX_FIELD_BYTES,
        max_span_bytes: caps::MAX_SPAN_BYTES,
    };

    /// The cap of one field: the per-field cap, or the field's own smaller cap (`model_parameters`,
    /// `trace_state`).
    pub const fn field_cap(&self, field: PayloadField) -> usize {
        let own = match field {
            PayloadField::ModelParameters => caps::MODEL_PARAMETERS_BYTES,
            PayloadField::TraceState => caps::TRACE_STATE_BYTES,
            _ => usize::MAX,
        };
        if own < self.max_field_bytes {
            own
        } else {
            self.max_field_bytes
        }
    }
}

impl SpanRecord {
    /// Applies the payload caps and the truncation rule (contract 3.1) to a record whose payload still holds the
    /// original values. Call it once, after the payload is built and before anything reads it.
    ///
    /// - The previews, `input_bytes` and `output_bytes` are taken from the original input and output.
    /// - Each field above its cap ([`PayloadCaps::field_cap`]) is cut to the longest prefix at or below the cap
    ///   that ends on a UTF-8 character boundary.
    /// - While the sum of the fields exceeds the per-span cap, fields are cut further in
    ///   [`PayloadField::SPAN_CUT_ORDER`], each by as much of the remaining excess as it holds (possibly to
    ///   empty), on character boundaries. If the fields outside that order alone exceed the cap, the cut stops
    ///   with every field in the order empty.
    /// - Every cut field is added to `truncated_fields`. For `input` and `output` the core tier also records
    ///   `cap_exceeded` and the SHA-256 of the original bytes.
    pub fn enforce_payload_caps(&mut self, payload_caps: &PayloadCaps) {
        let Self { core, payload } = self;
        core.input_preview = utf8_prefix(&payload.input, caps::PREVIEW_BYTES).into();
        core.output_preview = utf8_prefix(&payload.output, caps::PREVIEW_BYTES).into();
        core.input_bytes = payload.input.len() as u64;
        core.output_bytes = payload.output.len() as u64;
        for field in PayloadField::ALL {
            cut(core, payload, field, payload_caps.field_cap(field));
        }
        let mut excess = payload
            .stored_bytes()
            .saturating_sub(payload_caps.max_span_bytes);
        for field in PayloadField::SPAN_CUT_ORDER {
            if excess == 0 {
                break;
            }
            let len = payload.field(field).len();
            let kept = cut(core, payload, field, len.saturating_sub(excess));
            excess = excess.saturating_sub(len - kept);
        }
    }
}

/// Cuts one field to `cap` bytes on a character boundary and records the cut. Returns the stored length.
fn cut(core: &mut SpanCore, payload: &mut SpanPayload, field: PayloadField, cap: usize) -> usize {
    let value = payload.field_mut(field);
    if value.len() <= cap {
        return value.len();
    }
    let kept = utf8_floor(value, cap);
    // The first cut of a body sees the original bytes, so that is where its hash is taken.
    match field {
        PayloadField::Input if core.input_sha256.is_none() => {
            core.input_sha256 = Some(Sha256::digest(&value[..]).into());
            core.input_truncation = Truncation::CapExceeded;
        }
        PayloadField::Output if core.output_sha256.is_none() => {
            core.output_sha256 = Some(Sha256::digest(&value[..]).into());
            core.output_truncation = Truncation::CapExceeded;
        }
        _ => {}
    }
    value.truncate(kept);
    payload.truncated_fields.insert(field);
    kept
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::id::WalEpoch;
    use crate::lsn::Lsn;

    pub(crate) fn record() -> SpanRecord {
        let core = SpanCore {
            project_id: ProjectId::from_bytes([1; 16]),
            trace_id: TraceId::from_bytes([2; 16]),
            span_id: SpanId::from_bytes([3; 8]),
            parent_span_id: SpanId::ZERO,
            is_root: true,
            is_app_root: false,
            kind: SpanKind::Internal,
            name: "span".into(),
            start_time: Timestamp::from_micros(1_794_907_800_000_000),
            end_time: Timestamp::from_micros(1_794_907_800_250_000),
            completion_start_time: None,
            status_code: StatusCode::Unset,
            status_message: "".into(),
            service_name: "svc".into(),
            service_version: "".into(),
            scope_name: "".into(),
            scope_version: "".into(),
            telemetry_sdk_name: "".into(),
            telemetry_sdk_language: "".into(),
            telemetry_sdk_version: "".into(),
            sdk_name: "".into(),
            sdk_version: "".into(),
            observation_type: ObservationType::Span,
            level: Level::Default,
            environment: "default".into(),
            release: "".into(),
            version: "".into(),
            trace_name: "".into(),
            user_id: "".into(),
            session_id: "".into(),
            trace_tags: Vec::new(),
            trace_public: TracePublic::Unset,
            tags: Vec::new(),
            model_requested: "".into(),
            model_response: "".into(),
            provider: "".into(),
            finish_reasons: Vec::new(),
            usage: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            total_cost_nusd: NanoUsd::ZERO,
            cost_source: CostSource::None,
            price_model_id: None,
            price_tier_id: None,
            prompt_name: "".into(),
            prompt_version: "".into(),
            tool_names: Vec::new(),
            tool_call_count: 0,
            mcp_side: McpSide::None,
            io_shape: IoShape::Raw,
            input_preview: "".into(),
            output_preview: "".into(),
            input_bytes: 0,
            output_bytes: 0,
            input_truncation: Truncation::None,
            output_truncation: Truncation::None,
            input_sha256: None,
            output_sha256: None,
            source_format: SourceFormat::Otel,
            compat_dialect: 0,
            normalizer_version: 1,
            clock_skew: false,
            ingestion: Ingestion {
                wal_epoch: WalEpoch::from_bytes([4; 16]),
                batch_seq: Lsn::new(10),
                lsn: Lsn::new(12),
                received_at: Timestamp::from_micros(1_794_907_801_000_000),
            },
            api_key_id: ApiKeyId::from_bytes([5; 16]),
            row_digest: 0,
            event_bytes: 0,
        };
        SpanRecord {
            core,
            payload: SpanPayload::default(),
        }
    }

    fn sha(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    #[test]
    fn derived_columns() {
        let mut r = record();
        assert_eq!(r.core.duration_us(), 250_000);
        assert_eq!(r.core.start_minute(), 1_794_907_800);
        assert_eq!(r.core.trace_hash(), r.core.trace_id.trace_hash());
        r.core.end_time = Timestamp::from_micros(0);
        assert_eq!(r.core.duration_us(), 0);
    }

    #[test]
    fn per_field_cap_cuts_on_a_character_boundary_and_hashes_the_original() {
        let mut r = record();
        let input = "ab\u{e9}cd".repeat(3);
        r.payload.input = Bytes::from(input.clone());
        r.payload.output = Bytes::from_static(b"ok");
        r.payload.model_parameters = Bytes::from(vec![b'x'; caps::MODEL_PARAMETERS_BYTES + 1]);
        r.enforce_payload_caps(&PayloadCaps {
            max_field_bytes: 3,
            max_span_bytes: 1 << 20,
        });
        // "ab" then the two-byte "é" would end at byte 4, past the cap of 3.
        assert_eq!(&r.payload.input[..], b"ab");
        assert_eq!(r.core.input_bytes, input.len() as u64);
        assert_eq!(r.core.input_truncation, Truncation::CapExceeded);
        assert_eq!(r.core.input_sha256, Some(sha(input.as_bytes())));
        assert_eq!(&*r.core.input_preview, input.as_str());
        assert_eq!(
            (r.core.output_truncation, r.core.output_sha256),
            (Truncation::None, None)
        );
        assert_eq!(r.payload.model_parameters.len(), 3);
        let names: Vec<_> = r
            .payload
            .truncated_fields
            .iter()
            .map(PayloadField::name)
            .collect();
        assert_eq!(names, ["input", "model_parameters"]);
    }

    #[test]
    fn per_span_cap_cuts_in_the_fixed_order() {
        let mut r = record();
        r.payload.input = Bytes::from(vec![b'i'; 10]);
        r.payload.output = Bytes::from(vec![b'o'; 10]);
        r.payload.events = Bytes::from(vec![b'e'; 4]);
        r.payload.attributes = Bytes::from(vec![b'a'; 6]);
        r.payload.trace_state = Bytes::from(vec![b't'; 2]);
        // 32 bytes stored, cap 20: events go first (4), then attributes (6), then 2 bytes of output.
        r.enforce_payload_caps(&PayloadCaps {
            max_field_bytes: 1 << 20,
            max_span_bytes: 20,
        });
        assert_eq!(r.payload.stored_bytes(), 20);
        assert!(r.payload.events.is_empty() && r.payload.attributes.is_empty());
        assert_eq!(r.payload.output.len(), 8);
        assert_eq!(r.payload.input.len(), 10);
        assert_eq!(r.core.output_sha256, Some(sha(&[b'o'; 10])));
        assert_eq!(r.core.output_bytes, 10);
        assert_eq!(r.core.input_truncation, Truncation::None);
        let names: Vec<_> = r
            .payload
            .truncated_fields
            .iter()
            .map(PayloadField::name)
            .collect();
        assert_eq!(names, ["output", "attributes", "events"]);
    }

    fn arb_body() -> impl Strategy<Value = String> {
        "[a-z\u{e9}\u{4e2d}\u{1f600}]{0,24}"
    }

    proptest! {
        #[test]
        fn truncation_rule_holds(
            bodies in proptest::collection::vec(arb_body(), 13),
            max_field_bytes in 0usize..40,
            max_span_bytes in 0usize..160,
        ) {
            let mut r = record();
            for (field, body) in PayloadField::ALL.into_iter().zip(&bodies) {
                *r.payload.field_mut(field) = Bytes::from(body.clone());
            }
            let original = r.payload.clone();
            let caps = PayloadCaps { max_field_bytes, max_span_bytes };
            r.enforce_payload_caps(&caps);
            let p = &r.payload;
            for field in PayloadField::ALL {
                let (now, before) = (p.field(field), original.field(field));
                prop_assert!(before.starts_with(now));
                prop_assert!(core::str::from_utf8(now).is_ok(), "cut inside a character");
                prop_assert!(now.len() <= caps.field_cap(field));
                prop_assert_eq!(p.truncated_fields.contains(field), now.len() < before.len());
            }
            // Within the span cap, unless every field in the cut order is already empty.
            let order_empty = PayloadField::SPAN_CUT_ORDER.iter().all(|&f| p.field(f).is_empty());
            prop_assert!(p.stored_bytes() <= max_span_bytes || order_empty);
            // A field the span cap cut means every field before it in the order is empty.
            for (i, &field) in PayloadField::SPAN_CUT_ORDER.iter().enumerate() {
                let field_capped = original.field(field).len().min(caps.field_cap(field));
                let field_capped = utf8_floor(original.field(field), field_capped);
                if p.field(field).len() < field_capped {
                    for &earlier in &PayloadField::SPAN_CUT_ORDER[..i] {
                        prop_assert!(p.field(earlier).is_empty(), "{:?} before {:?}", earlier, field);
                    }
                }
            }
            for (now, before, bytes, truncation, hash, preview) in [
                (&p.input, &original.input, r.core.input_bytes, r.core.input_truncation, r.core.input_sha256, &r.core.input_preview),
                (&p.output, &original.output, r.core.output_bytes, r.core.output_truncation, r.core.output_sha256, &r.core.output_preview),
            ] {
                let cut = now.len() < before.len();
                prop_assert_eq!(bytes, before.len() as u64);
                prop_assert_eq!(truncation == Truncation::CapExceeded, cut);
                prop_assert_eq!(hash, cut.then(|| sha(before)));
                prop_assert_eq!(&**preview, utf8_prefix(before, caps::PREVIEW_BYTES));
            }
        }
    }
}
