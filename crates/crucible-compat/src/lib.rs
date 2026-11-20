//! The compat dialect registry (contract 10): the dialect ids, the data-table shape that the mapping engine in
//! `crucible-normalize` consumes, one table per dialect, the compat ingest routes, the compat media endpoint and
//! the fixtures.
//!
//! A table lists, for each record field it fills, an ordered list of sources (where to read, which key, how to
//! decode). It also lists the observation-type rules (contract 10.4) and the rules that detect the dialect. Tables
//! are data: the engine holds no dialect-specific string.
//!
//! The tables are **layers** ([`LAYERS`]) in the order of contract 10.3. The engine maps every span with all of
//! them: a candidate's rank is (layer position, source position) and the lowest rank wins, so a field's effective
//! source list is the 10.3 list. A source is scoped [`Scope::Any`] (read on every span) or [`Scope::Own`] (read
//! only when its table's dialect is the span's chosen dialect; used for bare, un-namespaced keys). The chosen
//! dialect decides `compat_dialect`, which `Own` sources are live, and the value encoding.
//!
//! This crate is pure: no `Env`, no IO.

mod d1;
mod d2;
mod d3;
mod d4;
mod d5;
mod d6;
mod d7;
mod fixtures;
pub mod media;
mod native;
pub mod routes;

use crucible_records::{IoShape, Level, ObservationType};

pub use d1::DIALECT_1;
pub use d2::DIALECT_2;
pub use d3::DIALECT_3;
pub use d4::DIALECT_4;
pub use d5::DIALECT_5;
pub use d6::DIALECT_6;
pub use d7::DIALECT_7;
pub use fixtures::{FIXTURES, Fixture};
pub use native::{CRUCIBLE_KEYS, DIALECT_0};

/// One registered dialect (contract 10.1). Ids are stable and never reused; a new dialect appends an id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dialect {
    /// The `compat_dialect` value.
    pub id: u8,
    /// What traffic the dialect covers.
    pub description: &'static str,
    /// How a span of the dialect is recognized.
    pub detected_by: &'static str,
}

/// Every registered dialect, indexed by id.
pub const DIALECTS: [Dialect; 8] = [
    Dialect {
        id: 0,
        description: "Native: the OTel GenAI and MCP semantic conventions and crucible.* keys",
        detected_by: "Default",
    },
    Dialect {
        id: 1,
        description: "A third-party tracing SDK's OTLP traffic on its compatibility path, with Basic auth \
                      carrying Crucible keys",
        detected_by: "The compatibility path (request hint), its instrumentation scope name, its type key, or \
                      its attribute key prefix",
    },
    Dialect {
        id: 2,
        description: "An open instrumentation convention identified by a span-kind attribute",
        detected_by: "That attribute",
    },
    Dialect {
        id: 3,
        description: "An open instrumentation library family identified by its attribute namespace",
        detected_by: "Keys in that namespace",
    },
    Dialect {
        id: 4,
        description: "A web framework's AI SDK telemetry identified by an operation-id attribute",
        detected_by: "That attribute",
    },
    Dialect {
        id: 5,
        description: "An ML lifecycle tool's tracing attributes identified by their namespace",
        detected_by: "Keys in that namespace",
    },
    Dialect {
        id: 6,
        description: "An agent framework's instrumentation identified by its scope name",
        detected_by: "Scope name",
    },
    Dialect {
        id: 7,
        description: "A third-party evaluation product's OTLP attribute namespace, on an aliased path with \
                      Bearer auth",
        detected_by: "The alias path or routing header (request hint), or one of its content keys",
    },
];

/// The registered dialect with this id.
pub fn dialect(id: u8) -> Option<&'static Dialect> {
    DIALECTS.get(usize::from(id))
}

/// How a source or detection key matches an attribute key. Matching is exact and case-sensitive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyMatch {
    /// This key.
    Exact(&'static str),
    /// Any key that starts with this prefix.
    Prefix(&'static str),
}

impl KeyMatch {
    /// Whether `key` matches.
    pub fn matches(&self, key: &str) -> bool {
        match *self {
            Self::Exact(exact) => key == exact,
            Self::Prefix(prefix) => key.starts_with(prefix),
        }
    }

    /// The key or prefix text.
    pub const fn text(&self) -> &'static str {
        match *self {
            Self::Exact(text) | Self::Prefix(text) => text,
        }
    }
}

/// Whether a source is read on every span or only on spans of its own dialect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Read on every span: the key is namespaced, or a semantic-convention key.
    Any,
    /// Read only when the table's dialect is the span's chosen dialect: a bare, generic key.
    Own,
}

/// Where a source is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Location {
    /// The span's attributes.
    Span,
    /// The span's resource attributes.
    Resource,
    /// The attributes of the span's events with this name, in wire order.
    Event(&'static str),
    /// The timestamp of the first event with this name, in wire order. Offered under the key
    /// [`EVENT_TIME_KEY`].
    EventTime(&'static str),
}

/// The key under which [`Location::EventTime`] offers an event's timestamp.
pub const EVENT_TIME_KEY: &str = "time";

/// The unit of a start-relative duration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeUnit {
    /// Milliseconds.
    Millis,
    /// Seconds.
    Seconds,
}

/// The values that name a level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LevelMap {
    /// Names, compared after trimming ASCII whitespace and ASCII case folding. Listed in lower case.
    pub names: &'static [(&'static str, Level)],
    /// Numbers, compared exactly.
    pub numbers: &'static [(i64, Level)],
}

/// How the members of a usage or cost object map onto usage keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UsageMap {
    /// Member name to usage key. The first listed alias of a key that is present wins for that key.
    pub aliases: &'static [(&'static str, &'static str)],
    /// Members that are never usage (they stay residual or in the blob).
    pub ignore: &'static [&'static str],
    /// Keep unlisted non-negative integer members under their own name.
    pub keep_other: bool,
    /// `(target key, addends)`: when the target key is absent, it is the sum of the addends that are present.
    pub sums: &'static [(&'static str, &'static [&'static str])],
    /// Base member names that exclude their parts (disjoint buckets). Empty for inclusive dialects.
    pub exclusive_bases: &'static [&'static str],
}

/// How the nested members of a usage object are flattened before its counts are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UsageDetails {
    /// A member whose name ends with this suffix and whose value is an object lists the parts of its base.
    pub suffix: &'static str,
    /// Base name prefixes whose parts count on the input side: a part `<entry>` becomes `input_<entry>`.
    pub input_bases: &'static [&'static str],
    /// Base name prefixes whose parts count on the output side: a part `<entry>` becomes `output_<entry>`.
    pub output_bases: &'static [&'static str],
    /// Objects flattened by name, as `(object, [(member, flattened name)])`.
    pub objects: &'static [(&'static str, &'static [(&'static str, &'static str)])],
}

/// The flattening every usage object gets: provider `*_details` objects and the cache-creation breakdown.
pub static USAGE_DETAILS: UsageDetails = UsageDetails {
    suffix: "_details",
    input_bases: &["prompt", "input"],
    output_bases: &["completion", "output"],
    objects: &[(
        "cache_creation",
        &[
            ("ephemeral_5m_input_tokens", "input_cache_creation_5m"),
            ("ephemeral_1h_input_tokens", "input_cache_creation_1h"),
        ],
    )],
};

/// How a prefix source builds a map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PrefixSpec {
    /// How member values are read.
    pub values: MapValues,
    /// Full keys under the prefix that are not members.
    pub except: &'static [&'static str],
}

/// How the member values of a prefix map are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapValues {
    /// The attribute value as JSON (string, number, boolean, array).
    Native,
    /// As `Native`, but a string that is JSON text is replaced by the parsed value when the gate holds.
    JsonWhen(&'static SdkGate),
}

/// The SDK versions that JSON-encode every value of a prefix map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SdkGate {
    /// The SDK's scope name prefix; on such a scope the scope version decides.
    pub scope_prefix: &'static str,
    /// SDK name spellings to canonical name (the frame's `sdk_name`, else the resource's SDK language).
    pub names: &'static [(&'static str, &'static str)],
    /// Canonical name to the first major version that JSON-encodes every value.
    pub min_major: &'static [(&'static str, u32)],
}

/// How an indexed family of flattened keys (`<prefix><i>.<rest>`) is rebuilt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexedLayout {
    /// Wrapper segments dropped when they directly follow an index (`<i>.message.role` gives `role`).
    pub wrappers: &'static [&'static str],
    /// Leaf names whose string value is parsed as JSON when it parses.
    pub json_leaves: &'static [&'static str],
    /// The member of an element that lists its tool calls, and their shape; `None` when not derived.
    pub tool_calls: Option<(&'static str, ToolCallShape)>,
    /// The member of an element that holds its finish reason; `None` when not derived.
    pub finish_reason: Option<&'static str>,
}

/// Where the id, name and arguments of a tool call live inside its JSON object, as member paths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ToolCallShape {
    /// Path of the call id.
    pub id: &'static [&'static str],
    /// Path of the tool name.
    pub name: &'static [&'static str],
    /// Path of the arguments.
    pub arguments: &'static [&'static str],
}

/// Where tool calls live inside the messages of a `genai_messages` body: each message lists parts, and a part of
/// the tool-call kind is one call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartsShape {
    /// The member of a message that lists its parts.
    pub parts: &'static str,
    /// The member of a part that names its kind.
    pub kind: &'static str,
    /// The kind of a tool-call part.
    pub tool_call: &'static str,
    /// The shape of a tool-call part.
    pub shape: ToolCallShape,
}

/// The tool-call parts of the GenAI semantic conventions' message format (dialect 0).
pub static GENAI_PARTS: PartsShape = PartsShape {
    parts: "parts",
    kind: "type",
    tool_call: "tool_call",
    shape: ToolCallShape {
        id: &["id"],
        name: &["name"],
        arguments: &["arguments"],
    },
};

/// How a source value is decoded (contract 10.3). A value that does not decode, or decodes to an empty value, is
/// not a candidate, and its attribute stays in the residual attributes. No decoder panics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoder {
    /// A non-empty string. A list target also accepts an array whose elements are all strings.
    String,
    /// A non-negative integer.
    Integer,
    /// A JSON body: a structured or scalar value rendered as JSON, or a string that is JSON text, kept verbatim.
    /// Any other string does not decode.
    Json,
    /// A body that is JSON or text: as [`Decoder::Json`], except that any non-empty string is kept verbatim. It
    /// carries the shape the stored body has (`io_shape`), for `input` and `output`.
    JsonOrString(IoShape),
    /// A non-empty string, or an integer rendered in decimal.
    StringOrInt,
    /// A boolean, or the strings `true` and `false`.
    Bool,
    /// A level by name (a string) or by number (an integer).
    Level(&'static LevelMap),
    /// A tag list: an array of strings, or a string that is JSON text of one; any other string is one tag, or with
    /// `split_commas` is split on `,`. Tags are trimmed; empty and repeated tags are dropped.
    Tags {
        /// Whether a plain string is split on `,`.
        split_commas: bool,
    },
    /// An instant: an RFC 3339 string, a JSON string literal holding one, a non-negative integer of Unix
    /// nanoseconds, or (with [`Location::EventTime`]) the event's own time.
    Timestamp,
    /// A non-negative duration since the span's start, clamped to its end.
    OffsetFromStart(TimeUnit),
    /// A USD amount: a double, an integer, or a string holding a decimal number.
    Usd,
    /// A JSON object: a string that is JSON text of an object, or a key-value list. Its members join a map.
    JsonObject,
    /// A usage object: as [`Decoder::JsonObject`], its members mapped onto usage keys.
    UsageObject(&'static UsageMap),
    /// Every attribute under the prefix as one usage object, the member name being the key suffix.
    PrefixUsage(&'static UsageMap),
    /// A cost object: members holding USD amounts, mapped onto usage keys.
    CostObject(&'static UsageMap),
    /// Every attribute under the prefix as one member of a map, the member name being the key suffix.
    PrefixMap(&'static PrefixSpec),
    /// Any scalar or array as the named member of a map. A string that is JSON text of an object or array is
    /// parsed.
    MapMember(&'static str),
    /// Indexed flattened messages (`<prefix><i>.<rest>`) rebuilt as a JSON array; `io_shape = indexed_messages`.
    Indexed(&'static IndexedLayout),
    /// As [`Decoder::Indexed`], for a JSON body that is not messages.
    IndexedList(&'static IndexedLayout),
    /// An array of strings, each parsed as JSON when it parses, as one JSON array.
    JsonStrings,
    /// A JSON array of tool calls, or a string that is JSON text of one, reshaped to `{id, name, arguments}`.
    ToolCalls(&'static ToolCallShape),
}

impl Decoder {
    /// Whether the decoder reads the keys under a prefix as one family.
    pub const fn is_family(self) -> bool {
        matches!(
            self,
            Self::Indexed(_) | Self::IndexedList(_) | Self::PrefixUsage(_) | Self::PrefixMap(_)
        )
    }
}

/// A record field that a source fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// `service_name`.
    ServiceName,
    /// `service_version`.
    ServiceVersion,
    /// `telemetry_sdk_name`.
    TelemetrySdkName,
    /// `telemetry_sdk_language`.
    TelemetrySdkLanguage,
    /// `telemetry_sdk_version`.
    TelemetrySdkVersion,
    /// `user_id`, a trace-level attribute.
    UserId,
    /// `session_id`, a trace-level attribute.
    SessionId,
    /// `model_requested`.
    ModelRequested,
    /// `model_response`.
    ModelResponse,
    /// `provider`.
    Provider,
    /// `prompt_name`.
    PromptName,
    /// `finish_reasons`.
    FinishReasons,
    /// One `usage_provided` key: a normalized usage key, or another key under the name given.
    Usage(&'static str),
    /// The `input` body.
    Input,
    /// The `output` body.
    Output,
    /// The `system_instructions` body.
    SystemInstructions,
    /// The `tool_definitions` body.
    ToolDefinitions,
    /// `trace_name`, a trace-level attribute.
    TraceName,
    /// `trace_tags`, a trace-level attribute.
    TraceTags,
    /// `trace_public`, a trace-level attribute.
    TracePublic,
    /// `trace_metadata`, a map and a trace-level attribute.
    TraceMetadata,
    /// `tags`.
    Tags,
    /// `metadata`, a map.
    Metadata,
    /// The explicit `level`.
    Level,
    /// `status_message`, in place of the OTLP status message.
    StatusMessage,
    /// `environment`, a trace-level attribute.
    Environment,
    /// `release`, a trace-level attribute.
    Release,
    /// `version`, a trace-level attribute.
    Version,
    /// `prompt_version`.
    PromptVersion,
    /// `completion_start_time`.
    CompletionStartTime,
    /// `model_parameters`, a map.
    ModelParameters,
    /// The dialect's root flag: `true` sets `is_root`.
    IsRoot,
    /// The dialect's application-root flag: `true` sets `is_app_root`.
    IsAppRoot,
    /// The `tool_calls` body.
    ToolCalls,
    /// `tool_names`.
    ToolNames,
    /// Many `usage_provided` keys from one source.
    UsageAll,
    /// One `cost_provided` key: `total` or a usage key.
    Cost(&'static str),
    /// Many `cost_provided` keys from one source.
    CostAll,
}

impl Target {
    /// Whether the target is a map that merges the members of every source that decodes: on a member clash the
    /// lower-rank source wins, and members are stored in rank order, then wire order.
    pub const fn is_map(self) -> bool {
        matches!(
            self,
            Self::Metadata | Self::TraceMetadata | Self::ModelParameters
        )
    }

    /// Whether a source of this target may use `decoder`.
    pub const fn accepts(self, decoder: Decoder) -> bool {
        match self {
            Self::ServiceName
            | Self::ServiceVersion
            | Self::TelemetrySdkName
            | Self::TelemetrySdkLanguage
            | Self::TelemetrySdkVersion
            | Self::ModelRequested
            | Self::ModelResponse
            | Self::Provider
            | Self::PromptName
            | Self::FinishReasons
            | Self::TraceName
            | Self::Environment
            | Self::Release
            | Self::Version
            | Self::StatusMessage => matches!(decoder, Decoder::String),
            Self::SessionId | Self::UserId | Self::PromptVersion => {
                matches!(decoder, Decoder::String | Decoder::StringOrInt)
            }
            Self::Tags | Self::TraceTags | Self::ToolNames => {
                matches!(decoder, Decoder::Tags { .. } | Decoder::String)
            }
            Self::TracePublic | Self::IsRoot | Self::IsAppRoot => matches!(decoder, Decoder::Bool),
            Self::Level => matches!(decoder, Decoder::Level(_)),
            Self::CompletionStartTime => {
                matches!(decoder, Decoder::Timestamp | Decoder::OffsetFromStart(_))
            }
            Self::Metadata | Self::TraceMetadata => {
                matches!(decoder, Decoder::JsonObject | Decoder::PrefixMap(_))
            }
            Self::ModelParameters => matches!(
                decoder,
                Decoder::JsonObject | Decoder::PrefixMap(_) | Decoder::MapMember(_)
            ),
            // `input` and `output` are JSON or text; the other bodies are JSON (contract 3.1).
            Self::Input | Self::Output => matches!(
                decoder,
                Decoder::JsonOrString(_) | Decoder::Indexed(_) | Decoder::JsonStrings
            ),
            Self::SystemInstructions => matches!(decoder, Decoder::Json),
            Self::ToolDefinitions => matches!(
                decoder,
                Decoder::Json | Decoder::IndexedList(_) | Decoder::JsonStrings
            ),
            Self::ToolCalls => matches!(decoder, Decoder::ToolCalls(_)),
            Self::Usage(_) => matches!(decoder, Decoder::Integer),
            Self::UsageAll => matches!(decoder, Decoder::UsageObject(_) | Decoder::PrefixUsage(_)),
            Self::Cost(_) => matches!(decoder, Decoder::Usd),
            // With `Usd`, a prefix source reads one cost per key under the prefix, named by the key suffix.
            Self::CostAll => matches!(decoder, Decoder::CostObject(_) | Decoder::Usd),
        }
    }
}

/// One source of a field: where to read, which key, how to decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Source {
    /// Where the value is read.
    pub location: Location,
    /// Which attribute key.
    pub key: KeyMatch,
    /// JSON member path inside the value; empty for the value itself. A step of decimal digits indexes an array.
    /// A source with a path never consumes its attribute.
    pub path: &'static [&'static str],
    /// How the value is decoded.
    pub decoder: Decoder,
    /// On which spans the source is read.
    pub scope: Scope,
}

/// The sources of one field, in priority order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldRule {
    /// The field.
    pub target: Target,
    /// The sources, highest priority first.
    pub sources: &'static [Source],
}

/// One observation-type rule (contract 10.4): a string value of the span attribute `key` (or of a member of the
/// JSON object it holds) listed in `values` yields that type. Rules apply in the order of (`rank`, layer position,
/// position in the table), and the first one that yields a type wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeRule {
    /// The rule number in contract 10.4, which orders rules across tables.
    pub rank: u8,
    /// The span attribute.
    pub key: &'static str,
    /// JSON member path inside the value; empty for the value itself.
    pub path: &'static [&'static str],
    /// The values that yield a type, compared exactly.
    pub values: &'static [(&'static str, ObservationType)],
    /// Whether the winning attribute leaves the residual attributes. A rule whose key carries more than the type
    /// keeps it there.
    pub consume: bool,
}

/// How sure a detection rule is that a span belongs to its dialect; higher wins. One scale for every dialect: 100
/// the dialect's own scope name, 90 a key only that dialect writes, 70 likely, 60 the request hint, 40 weak.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Confidence(pub u8);

impl Confidence {
    /// The confidence of a request-level hint (contract 10.2).
    pub const HINT: Self = Self(60);
}

/// What a detection rule looks at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DetectOn {
    /// The instrumentation scope name.
    ScopeName(KeyMatch),
    /// The keys of the span's attributes.
    SpanKey(KeyMatch),
}

/// One detection rule of a dialect (contract 10.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DetectRule {
    /// What the rule looks at.
    pub on: DetectOn,
    /// The confidence it yields when it matches.
    pub confidence: Confidence,
}

/// How a dialect's SDK encodes attribute values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueEncoding {
    /// Values are sent as they are.
    Native,
    /// Every value is sent as JSON text: on a span of the dialect, a string attribute that is JSON text of one
    /// scalar is unwrapped before a scalar decoder or a type rule reads it. Bodies and maps are never unwrapped.
    JsonText,
}

/// The delimiters of a dialect's in-value media reference token. A token is the shortest substring that starts
/// with `prefix` and ends with `suffix`; with an empty `suffix` it ends at the first `"`, whitespace or end of
/// text. Stored fields keep the token; previews show [`MEDIA_PLACEHOLDER`] in its place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MediaToken {
    /// How a token starts.
    pub prefix: &'static str,
    /// How a token ends; empty when it has no closing delimiter.
    pub suffix: &'static str,
}

/// What a preview shows in place of a media reference token.
pub const MEDIA_PLACEHOLDER: &str = "[media]";

/// One dialect's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DialectTable {
    /// The dialect id.
    pub dialect: u8,
    /// How the dialect's SDK encodes values.
    pub encoding: ValueEncoding,
    /// Detection rules; dialect 0 has none because it is the default.
    pub detect: &'static [DetectRule],
    /// Field rules, at most one per target.
    pub fields: &'static [FieldRule],
    /// Observation-type rules.
    pub observation_types: &'static [TypeRule],
    /// Media reference tokens.
    pub media: &'static [MediaToken],
}

/// A span key, read on every span.
pub(crate) const fn span(key: &'static str, decoder: Decoder) -> Source {
    Source {
        location: Location::Span,
        key: KeyMatch::Exact(key),
        path: &[],
        decoder,
        scope: Scope::Any,
    }
}

/// A bare span key, read only on spans of the table's dialect.
pub(crate) const fn own(key: &'static str, decoder: Decoder) -> Source {
    Source {
        scope: Scope::Own,
        ..span(key, decoder)
    }
}

/// Every span key under a prefix.
pub(crate) const fn prefix(prefix: &'static str, decoder: Decoder) -> Source {
    Source {
        key: KeyMatch::Prefix(prefix),
        ..span(prefix, decoder)
    }
}

/// A resource key.
pub(crate) const fn resource(key: &'static str, decoder: Decoder) -> Source {
    Source {
        location: Location::Resource,
        ..span(key, decoder)
    }
}

/// A member of the JSON object a span key holds.
pub(crate) const fn member(
    key: &'static str,
    path: &'static [&'static str],
    decoder: Decoder,
) -> Source {
    Source {
        path,
        ..span(key, decoder)
    }
}

/// The time of the first event with this name.
pub(crate) const fn event_time(name: &'static str) -> Source {
    Source {
        location: Location::EventTime(name),
        ..span(EVENT_TIME_KEY, Decoder::Timestamp)
    }
}

pub(crate) const fn field(target: Target, sources: &'static [Source]) -> FieldRule {
    FieldRule { target, sources }
}

pub(crate) const fn rule(
    rank: u8,
    key: &'static str,
    path: &'static [&'static str],
    consume: bool,
    values: &'static [(&'static str, ObservationType)],
) -> TypeRule {
    TypeRule {
        rank,
        key,
        path,
        values,
        consume,
    }
}

pub(crate) const fn detect_scope(name: KeyMatch, confidence: u8) -> DetectRule {
    DetectRule {
        on: DetectOn::ScopeName(name),
        confidence: Confidence(confidence),
    }
}

pub(crate) const fn detect_key(key: KeyMatch, confidence: u8) -> DetectRule {
    DetectRule {
        on: DetectOn::SpanKey(key),
        confidence: Confidence(confidence),
    }
}

/// A raw body.
pub(crate) const RAW: Decoder = Decoder::JsonOrString(IoShape::Raw);

/// A body in the GenAI semantic conventions' message format.
pub(crate) const GENAI: Decoder = Decoder::JsonOrString(IoShape::GenaiMessages);

/// A prefix map whose values are read as they are.
pub(crate) static NATIVE: PrefixSpec = PrefixSpec {
    values: MapValues::Native,
    except: &[],
};

/// The keys that feed two fields of one table, as (location, key). Every other key feeds at most one field per
/// location and path, except that an exact key may also lie under a prefix source (fan-out). `service.version` is
/// `service_version` and the source of `release` (contract 10.6).
pub const SHARED_KEYS: &[(Location, &str)] = &[(Location::Resource, "service.version")];

/// The layers in contract 10.3 order: the `crucible.*` keys, then dialects 1, 7, 0, 2, 4, 3, 5 and 6.
pub static LAYERS: [&DialectTable; 9] = [
    &CRUCIBLE_KEYS,
    &DIALECT_1,
    &DIALECT_7,
    &DIALECT_0,
    &DIALECT_2,
    &DIALECT_4,
    &DIALECT_3,
    &DIALECT_5,
    &DIALECT_6,
];

/// The table of every detectable dialect, indexed by id. [`CRUCIBLE_KEYS`] is a layer of dialect 0, not a dialect.
pub static TABLES: [&DialectTable; 8] = [
    &DIALECT_0, &DIALECT_1, &DIALECT_2, &DIALECT_3, &DIALECT_4, &DIALECT_5, &DIALECT_6, &DIALECT_7,
];

/// One compat ingest route (contract 12.1); path, header and product strings live only here. Every route accepts
/// Crucible keys as Basic or Bearer (contract 12.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CompatRoute {
    /// The dialect the route hints.
    pub dialect: u8,
    /// The request path.
    pub path: &'static str,
    /// A routing header that also yields the hint on `/v1/traces`.
    pub hint_header: Option<&'static str>,
    /// A header that fills the envelope's `sdk_name` when present.
    pub sdk_name_header: Option<&'static str>,
    /// A header that fills the envelope's `sdk_version` when present.
    pub sdk_version_header: Option<&'static str>,
}

/// The compat ingest routes.
pub static ROUTES: [CompatRoute; 2] = [
    CompatRoute {
        dialect: 1,
        path: "/api/public/otel/v1/traces",
        hint_header: None,
        sdk_name_header: Some("x-langfuse-sdk-name"),
        sdk_version_header: Some("x-langfuse-sdk-version"),
    },
    CompatRoute {
        dialect: 7,
        path: "/otel/v1/traces",
        hint_header: Some("x-bt-parent"),
        sdk_name_header: None,
        sdk_version_header: None,
    },
];

#[cfg(test)]
mod tests;
