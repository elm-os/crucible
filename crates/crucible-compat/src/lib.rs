//! The compat dialect registry (contract 10): the dialect ids, the data-table shape that the mapping engine in
//! `crucible-normalize` consumes, and one table per dialect.
//!
//! A table lists, for each record field it fills, an ordered list of sources (where to read, which key, how to
//! decode); the first source that yields a non-empty value wins (contract 10.3). It also lists the observation-type
//! rules in priority order (contract 10.4) and the rules that detect the dialect. Tables are data: the engine holds
//! no dialect-specific string.
//!
//! Milestone 1 ships dialect 0's table only. It holds the keys that the contract names, plus, where the contract
//! names a field's concept, the one semantic-convention key that exists for it. The tables of dialects 1 to 7, and
//! the decoders and targets only they need, arrive with their mappers.
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
/// Milestone 1 tables use these decoders; the others that contract 10.3 lists join with the tables that use them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoder {
    /// A non-empty string. A list target also accepts an array whose elements are all strings.
    String,
    /// A non-negative integer.
    Integer,
    /// A body kept as JSON text: a string value verbatim, any other value rendered as JSON. For `input` and
    /// `output`, the shape the stored body has (`io_shape`).
    Json(IoShape),
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
    /// Whether a source of this target may use `decoder`.
    pub const fn accepts(self, decoder: Decoder) -> bool {
        match self {
            Self::Usage(_) => matches!(decoder, Decoder::Integer),
            Self::Input | Self::Output | Self::SystemInstructions | Self::ToolDefinitions => {
                matches!(decoder, Decoder::Json(_))
            }
            Self::ServiceName
            | Self::ServiceVersion
            | Self::TelemetrySdkName
            | Self::TelemetrySdkLanguage
            | Self::TelemetrySdkVersion
            | Self::UserId
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

/// Dialect 0, native (contract 10.1): the keys the contract names, and the one semantic-convention key of each
/// field concept the contract names. Fields with neither (for example the trace-level `trace_name` and
/// `session_id`, or provided cost) have no source yet.
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
        field(Target::UserId, &[span("user.id", Decoder::String)]),
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
            &[span("gen_ai.prompt.name", Decoder::String)],
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
            &[span(
                "gen_ai.input.messages",
                Decoder::Json(IoShape::GenaiMessages),
            )],
        ),
        field(
            Target::Output,
            &[span(
                "gen_ai.output.messages",
                Decoder::Json(IoShape::GenaiMessages),
            )],
        ),
        field(
            Target::SystemInstructions,
            &[span(
                "gen_ai.system_instructions",
                Decoder::Json(IoShape::Raw),
            )],
        ),
        field(
            Target::ToolDefinitions,
            &[span("gen_ai.tool.definitions", Decoder::Json(IoShape::Raw))],
        ),
    ],
};

/// The table of every dialect with a mapper in this build: dialect 0 only, in milestone 1.
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
    /// feeds at most one field per location (so consuming an attribute is unambiguous), and type values are
    /// unique within a rule.
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
                    assert!(!keys.contains(&at), "{at:?} feeds two fields");
                    keys.push(at);
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
