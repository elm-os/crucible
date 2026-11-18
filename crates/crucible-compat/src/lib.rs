//! The compat dialect registry (contract 10): the dialect ids, the data-table shape that the mapping engine in
//! `crucible-normalize` consumes, and one table per dialect.
//!
//! A table lists, for each record field it fills, an ordered list of sources (where to read, which key, how to
//! decode); the first source that yields a non-empty value wins (contract 10.3). It also lists the observation-type
//! rules in priority order (contract 10.4) and the rules that detect the dialect. Tables are data: the engine holds
//! no dialect-specific string.
//!
//! This build ships dialect 0's table only. It holds the keys that the contract names (section 10.6 for the fields
//! without a semantic convention), plus, where the contract names a field's concept, the semantic-convention key
//! that exists for it. The tables of dialects 1 to 7, and the decoders and targets only they need, arrive with
//! their mappers.
//!
//! This crate is pure: no `Env`, no IO.

use crucible_records::{IoShape, ObservationType, UsageEntry};

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
        detected_by: "The compatibility path (request hint), instrumentation scope name, or its attribute key \
                      prefix",
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
        detected_by: "The alias path or routing header (request hint), or its attribute key prefix",
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

/// Where a source is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Location {
    /// The span's attributes.
    Span,
    /// The span's resource attributes.
    Resource,
    /// The attributes of the span's events with this name, in wire order.
    Event(&'static str),
}

/// How a source value is decoded (contract 10.3). A value that does not decode, or decodes to an empty value, is
/// not a candidate, and its attribute stays in the residual attributes.
///
/// Dialect 0 uses these decoders; the others that contract 10.3 lists join with the tables that use them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoder {
    /// A boolean.
    Bool,
    /// A level by its wire name: `debug`, `default`, `warning` or `error`.
    Level,
    /// An instant: an RFC 3339 string, or a non-negative integer of Unix nanoseconds, inside the valid time range.
    Time,
    /// One member of a map (prefix-flatten to map): any value that is set and is not the empty string. See
    /// [`Target::is_map`] for how members are named and combined.
    Member,
    /// One member of a cost map: a USD amount as a decimal string, a double or an integer.
    Usd,
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
    /// `trace_name`, a trace-level attribute.
    TraceName,
    /// `environment`, a trace-level attribute.
    Environment,
    /// `release`, a trace-level attribute.
    Release,
    /// `version`, a trace-level attribute.
    Version,
    /// `tags`.
    Tags,
    /// `trace_tags`, a trace-level attribute.
    TraceTags,
    /// `trace_public`, a trace-level attribute.
    TracePublic,
    /// The explicit `level`.
    Level,
    /// `prompt_version`.
    PromptVersion,
    /// `completion_start_time`.
    CompletionStartTime,
    /// `metadata`, a map.
    Metadata,
    /// `trace_metadata`, a map and a trace-level attribute.
    TraceMetadata,
    /// `model_parameters`, a map.
    ModelParameters,
    /// `cost_provided`, a map from a usage key (or `total`) to a USD amount.
    CostProvided,
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
    /// One `usage_provided` key, a normalized usage key ([`UsageEntry::INPUT`] and the others).
    Usage(&'static str),
    /// The `input` body.
    Input,
    /// The `output` body.
    Output,
    /// The `system_instructions` body.
    SystemInstructions,
    /// The `tool_definitions` body.
    ToolDefinitions,
}

impl Target {
    /// Whether the target is a map that collects one member per matching attribute, instead of taking one value.
    ///
    /// - A [`KeyMatch::Prefix`] source names the member by the key with the prefix removed; an attribute whose
    ///   key equals the prefix has no member name and is not read. A [`KeyMatch::Exact`] source names the member
    ///   by the key's last dot-separated segment.
    /// - A prefix source does not read a key that an exact source of a non-map field of the same table reads at
    ///   the same location: that key belongs to the other field.
    /// - Per member name, the highest-priority source wins, and among occurrences of one source the first in wire
    ///   order. Members are stored in ascending byte order of their names.
    pub const fn is_map(self) -> bool {
        matches!(
            self,
            Self::Metadata | Self::TraceMetadata | Self::ModelParameters | Self::CostProvided
        )
    }

    /// Whether a source of this target may use `decoder`.
    pub const fn accepts(self, decoder: Decoder) -> bool {
        match self {
            Self::Metadata | Self::TraceMetadata | Self::ModelParameters => {
                matches!(decoder, Decoder::Member)
            }
            Self::CostProvided => matches!(decoder, Decoder::Usd),
            Self::TracePublic => matches!(decoder, Decoder::Bool),
            Self::Level => matches!(decoder, Decoder::Level),
            Self::CompletionStartTime => matches!(decoder, Decoder::Time),
            Self::Usage(_) => matches!(decoder, Decoder::Integer),
            // `input` and `output` are JSON or text; the other bodies are JSON (contract 3.1).
            Self::Input | Self::Output => matches!(decoder, Decoder::JsonOrString(_)),
            Self::SystemInstructions | Self::ToolDefinitions => matches!(decoder, Decoder::Json),
            Self::ServiceName
            | Self::ServiceVersion
            | Self::TelemetrySdkName
            | Self::TelemetrySdkLanguage
            | Self::TelemetrySdkVersion
            | Self::UserId
            | Self::SessionId
            | Self::TraceName
            | Self::Environment
            | Self::Release
            | Self::Version
            | Self::Tags
            | Self::TraceTags
            | Self::PromptVersion
            | Self::ModelRequested
            | Self::ModelResponse
            | Self::Provider
            | Self::PromptName
            | Self::FinishReasons => matches!(decoder, Decoder::String),
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
    /// How the value is decoded.
    pub decoder: Decoder,
}

/// The sources of one field, in priority order: the first non-empty one wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldRule {
    /// The field.
    pub target: Target,
    /// The sources, highest priority first.
    pub sources: &'static [Source],
}

/// One observation-type rule (contract 10.4): a string value of the span attribute `key` listed in `values` yields
/// that type. Rules are listed in priority order and the first one that yields a type wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeRule {
    /// The span attribute.
    pub key: &'static str,
    /// The values that yield a type, compared exactly.
    pub values: &'static [(&'static str, ObservationType)],
    /// Whether the winning attribute leaves the residual attributes. A rule whose key carries more than the type
    /// keeps it there.
    pub consume: bool,
}

/// How sure a detection rule is that a span belongs to its dialect; higher wins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Confidence(pub u8);

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

/// One dialect's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DialectTable {
    /// The dialect id.
    pub dialect: u8,
    /// Detection rules; dialect 0 has none because it is the default.
    pub detect: &'static [DetectRule],
    /// Field rules, at most one per target.
    pub fields: &'static [FieldRule],
    /// Observation-type rules, highest priority first.
    pub observation_types: &'static [TypeRule],
}

const fn span(key: &'static str, decoder: Decoder) -> Source {
    Source {
        location: Location::Span,
        key: KeyMatch::Exact(key),
        decoder,
    }
}

const fn resource(key: &'static str, decoder: Decoder) -> Source {
    Source {
        location: Location::Resource,
        key: KeyMatch::Exact(key),
        decoder,
    }
}

const fn span_prefix(prefix: &'static str, decoder: Decoder) -> Source {
    Source {
        location: Location::Span,
        key: KeyMatch::Prefix(prefix),
        decoder,
    }
}

const fn field(target: Target, sources: &'static [Source]) -> FieldRule {
    FieldRule { target, sources }
}

/// Every observation type under its wire name: the values of the explicit `crucible.observation.type` key.
const OBSERVATION_TYPE_NAMES: &[(&str, ObservationType)] = &[
    ("span", ObservationType::Span),
    ("generation", ObservationType::Generation),
    ("event", ObservationType::Event),
    ("embedding", ObservationType::Embedding),
    ("agent", ObservationType::Agent),
    ("tool", ObservationType::Tool),
    ("chain", ObservationType::Chain),
    ("retriever", ObservationType::Retriever),
    ("evaluator", ObservationType::Evaluator),
    ("guardrail", ObservationType::Guardrail),
];

/// Dialect 0, native (contract 10.1): the `crucible.*` keys of contract 10.6, which come first in each field's
/// sources (priority 1 of contract 10.3), and the semantic-convention keys of each field concept the contract
/// names.
///
/// Two readings of contract 10.6 are settled here:
///
/// - `input` and `output`: the row reads "non-GenAI spans" and "GenAI spans keep the message keys", so the GenAI
///   message keys rank before `crucible.input` and `crucible.output`. A span that carries both keeps its messages
///   and leaves the `crucible.*` body in the residual attributes.
/// - `release`: the resource key `service.version` is its second source and stays the source of
///   `service_version`. It is the one key that feeds two fields ([`SHARED_KEYS`]); it leaves the residual
///   resource attributes as before, because `service_version` consumes it.
pub static DIALECT_0: DialectTable = DialectTable {
    dialect: 0,
    detect: &[],
    observation_types: &[
        TypeRule {
            key: "crucible.observation.type",
            values: OBSERVATION_TYPE_NAMES,
            consume: true,
        },
        TypeRule {
            key: "mcp.method.name",
            values: &[("tools/call", ObservationType::Tool)],
            consume: false,
        },
        TypeRule {
            key: "gen_ai.operation.name",
            values: &[
                ("chat", ObservationType::Generation),
                ("text_completion", ObservationType::Generation),
                ("generate_content", ObservationType::Generation),
                ("embeddings", ObservationType::Embedding),
                ("execute_tool", ObservationType::Tool),
                ("create_agent", ObservationType::Agent),
                ("invoke_agent", ObservationType::Agent),
            ],
            consume: false,
        },
    ],
    fields: &[
        field(
            Target::ServiceName,
            &[resource("service.name", Decoder::String)],
        ),
        field(
            Target::ServiceVersion,
            &[resource("service.version", Decoder::String)],
        ),
        field(
            Target::TelemetrySdkName,
            &[resource("telemetry.sdk.name", Decoder::String)],
        ),
        field(
            Target::TelemetrySdkLanguage,
            &[resource("telemetry.sdk.language", Decoder::String)],
        ),
        field(
            Target::TelemetrySdkVersion,
            &[resource("telemetry.sdk.version", Decoder::String)],
        ),
        field(
            Target::TraceName,
            &[span("crucible.trace.name", Decoder::String)],
        ),
        field(
            Target::SessionId,
            &[
                span("crucible.session.id", Decoder::String),
                span("session.id", Decoder::String),
                span("gen_ai.conversation.id", Decoder::String),
            ],
        ),
        field(
            Target::UserId,
            &[
                span("crucible.user.id", Decoder::String),
                span("user.id", Decoder::String),
            ],
        ),
        field(
            Target::Environment,
            &[
                span("crucible.environment", Decoder::String),
                resource("deployment.environment.name", Decoder::String),
                resource("deployment.environment", Decoder::String),
            ],
        ),
        field(
            Target::Release,
            &[
                span("crucible.release", Decoder::String),
                resource("service.version", Decoder::String),
            ],
        ),
        field(
            Target::Version,
            &[span("crucible.version", Decoder::String)],
        ),
        field(Target::Tags, &[span("crucible.tags", Decoder::String)]),
        field(
            Target::TraceTags,
            &[span("crucible.trace.tags", Decoder::String)],
        ),
        field(
            Target::TracePublic,
            &[span("crucible.trace.public", Decoder::Bool)],
        ),
        field(Target::Level, &[span("crucible.level", Decoder::Level)]),
        field(
            Target::Metadata,
            &[span_prefix("crucible.metadata.", Decoder::Member)],
        ),
        field(
            Target::TraceMetadata,
            &[span_prefix("crucible.trace.metadata.", Decoder::Member)],
        ),
        field(
            Target::CostProvided,
            &[span_prefix("crucible.cost.", Decoder::Usd)],
        ),
        field(
            Target::CompletionStartTime,
            &[span("crucible.completion_start_time", Decoder::Time)],
        ),
        field(
            Target::ModelParameters,
            &[
                span_prefix("crucible.model_parameters.", Decoder::Member),
                // Every request parameter except the model, which `model_requested` reads.
                span_prefix("gen_ai.request.", Decoder::Member),
                // Read as the member `service_tier`.
                span("openai.request.service_tier", Decoder::Member),
            ],
        ),
        field(
            Target::PromptVersion,
            &[span("crucible.prompt.version", Decoder::String)],
        ),
        field(
            Target::ModelRequested,
            &[span("gen_ai.request.model", Decoder::String)],
        ),
        field(
            Target::ModelResponse,
            &[span("gen_ai.response.model", Decoder::String)],
        ),
        field(
            Target::Provider,
            &[span("gen_ai.provider.name", Decoder::String)],
        ),
        field(
            Target::PromptName,
            &[
                span("crucible.prompt.name", Decoder::String),
                span("gen_ai.prompt.name", Decoder::String),
            ],
        ),
        field(
            Target::FinishReasons,
            &[span("gen_ai.response.finish_reasons", Decoder::String)],
        ),
        field(
            Target::Usage(UsageEntry::INPUT),
            &[span("gen_ai.usage.input_tokens", Decoder::Integer)],
        ),
        field(
            Target::Usage(UsageEntry::OUTPUT),
            &[span("gen_ai.usage.output_tokens", Decoder::Integer)],
        ),
        // The semantic conventions count cache reads and writes inside the input tokens, and reasoning inside the
        // output tokens, so these three keys are parts of `input` and `output`, not additions to them.
        field(
            Target::Usage(UsageEntry::CACHE_READ),
            &[span(
                "gen_ai.usage.cache_read.input_tokens",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_WRITE),
            &[span(
                "gen_ai.usage.cache_creation.input_tokens",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::REASONING),
            &[span(
                "gen_ai.usage.reasoning.output_tokens",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Input,
            &[
                span(
                    "gen_ai.input.messages",
                    Decoder::JsonOrString(IoShape::GenaiMessages),
                ),
                span("crucible.input", Decoder::JsonOrString(IoShape::Raw)),
            ],
        ),
        field(
            Target::Output,
            &[
                span(
                    "gen_ai.output.messages",
                    Decoder::JsonOrString(IoShape::GenaiMessages),
                ),
                span("crucible.output", Decoder::JsonOrString(IoShape::Raw)),
            ],
        ),
        field(
            Target::SystemInstructions,
            &[span("gen_ai.system_instructions", Decoder::Json)],
        ),
        field(
            Target::ToolDefinitions,
            &[span("gen_ai.tool.definitions", Decoder::Json)],
        ),
    ],
};

/// The keys that feed two fields, as (location, key). Every other key feeds at most one field per location, so
/// that consuming an attribute is unambiguous. `service.version` is `service_version` and the second source of
/// `release` (contract 10.6).
pub const SHARED_KEYS: &[(Location, &str)] = &[(Location::Resource, "service.version")];

/// The table of every dialect with a mapper in this build: dialect 0 only.
pub static TABLES: [&DialectTable; 1] = [&DIALECT_0];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_dense_and_described() {
        for (i, d) in DIALECTS.iter().enumerate() {
            assert_eq!(usize::from(d.id), i);
            assert!(!d.description.is_empty() && !d.detected_by.is_empty());
            assert_eq!(dialect(d.id), Some(d));
        }
        assert_eq!(dialect(8), None);
        assert_eq!(dialect(u8::MAX), None);
    }

    #[test]
    fn key_matching_is_exact_or_prefix() {
        assert!(KeyMatch::Exact("a.b").matches("a.b"));
        assert!(!KeyMatch::Exact("a.b").matches("a.bc"));
        assert!(!KeyMatch::Exact("a.b").matches("A.B"));
        assert!(KeyMatch::Prefix("a.").matches("a.b"));
        assert!(!KeyMatch::Prefix("a.").matches("b.a"));
        assert_eq!(KeyMatch::Prefix("p").text(), "p");
    }

    /// Every table is well formed: each field has one rule, every source fits its target and has a key, a key
    /// feeds at most one field per location (so consuming an attribute is unambiguous) unless it is one of
    /// [`SHARED_KEYS`], only map targets read prefixes, no prefix of one map covers a key or prefix of another
    /// map, and type values are unique within a rule.
    #[test]
    fn tables_are_well_formed() {
        for (i, table) in TABLES.iter().enumerate() {
            assert!(dialect(table.dialect).is_some());
            assert!(TABLES[..i].iter().all(|t| t.dialect != table.dialect));
            let mut keys: Vec<(Location, &str)> = Vec::new();
            for (j, rule) in table.fields.iter().enumerate() {
                assert!(
                    table.fields[..j].iter().all(|r| r.target != rule.target),
                    "{:?} has two rules",
                    rule.target
                );
                assert!(!rule.sources.is_empty());
                for source in rule.sources {
                    assert!(rule.target.accepts(source.decoder), "{source:?}");
                    assert!(!source.key.text().is_empty());
                    let at = (source.location, source.key.text());
                    assert!(
                        !keys.contains(&at) || SHARED_KEYS.contains(&at),
                        "{at:?} feeds two fields"
                    );
                    keys.push(at);
                    if table.dialect == 0 {
                        // Dialect 0 reads prefixes only into maps; a member has no single winner to consume.
                        assert_eq!(
                            matches!(source.key, KeyMatch::Prefix(_)),
                            rule.target.is_map() && source.key.text().ends_with('.'),
                            "{source:?}"
                        );
                    }
                    for other in table.fields[..j].iter().filter(|r| r.target.is_map()) {
                        for theirs in other.sources {
                            let overlap = |a: &Source, b: &Source| {
                                a.location == b.location
                                    && matches!(a.key, KeyMatch::Prefix(p) if b.key.text().starts_with(p))
                            };
                            assert!(
                                !rule.target.is_map()
                                    || !(overlap(source, theirs) || overlap(theirs, source)),
                                "{source:?} overlaps {theirs:?}"
                            );
                        }
                    }
                }
            }
            for (j, rule) in table.observation_types.iter().enumerate() {
                assert!(!rule.key.is_empty() && !rule.values.is_empty());
                assert!(
                    table.observation_types[..j]
                        .iter()
                        .all(|r| r.key != rule.key)
                );
                for (k, (value, _)) in rule.values.iter().enumerate() {
                    assert!(rule.values[..k].iter().all(|(v, _)| v != value));
                }
            }
        }
    }

    #[test]
    fn the_explicit_type_key_names_every_type_by_its_wire_name() {
        let explicit = DIALECT_0.observation_types[0];
        assert_eq!(explicit.key, "crucible.observation.type");
        assert_eq!(explicit.values.len(), ObservationType::ALL.len());
        for t in ObservationType::ALL {
            assert!(explicit.values.contains(&(t.as_str(), *t)), "{t}");
        }
    }

    #[test]
    fn usage_targets_use_normalized_keys() {
        let normalized = [
            UsageEntry::INPUT,
            UsageEntry::OUTPUT,
            UsageEntry::TOTAL,
            UsageEntry::CACHE_READ,
            UsageEntry::CACHE_WRITE,
            UsageEntry::REASONING,
        ];
        for rule in DIALECT_0.fields {
            if let Target::Usage(key) = rule.target {
                assert!(normalized.contains(&key), "{key}");
            }
        }
    }
}
