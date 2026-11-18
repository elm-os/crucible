//! The table of dialect 7 (contract 10.1).

use crucible_records::{Level, ObservationType};

use crate::*;

pub static DIALECT_7: DialectTable = DialectTable {
    dialect: 7,
    encoding: ValueEncoding::Native,
    detect: &[
        detect_key(KeyMatch::Exact("braintrust.input_json"), 90),
        detect_key(KeyMatch::Exact("braintrust.output_json"), 90),
        detect_key(KeyMatch::Exact("braintrust.input"), 90),
        detect_key(KeyMatch::Exact("braintrust.output"), 90),
        detect_key(KeyMatch::Exact("braintrust.metadata"), 90),
        detect_key(KeyMatch::Exact("braintrust.metrics"), 90),
        detect_key(KeyMatch::Exact("braintrust.span_attributes"), 90),
        detect_key(KeyMatch::Exact("braintrust.tags"), 90),
        detect_key(KeyMatch::Exact("braintrust.scores"), 90),
        detect_key(KeyMatch::Exact("braintrust.expected"), 90),
        detect_key(KeyMatch::Exact("braintrust.expected_json"), 90),
        detect_key(KeyMatch::Prefix("braintrust.input."), 90),
        detect_key(KeyMatch::Prefix("braintrust.output."), 90),
        detect_key(KeyMatch::Prefix("braintrust.metadata."), 90),
        detect_key(KeyMatch::Prefix("braintrust.metrics."), 90),
        detect_key(KeyMatch::Prefix("braintrust.span_attributes."), 90),
        detect_scope(KeyMatch::Exact("braintrust-java"), 70),
        detect_scope(KeyMatch::Exact("braintrust-dotnet"), 70),
        detect_scope(KeyMatch::Prefix("braintrust.eval"), 70),
        detect_scope(KeyMatch::Exact("braintrust-eval"), 70),
        detect_scope(KeyMatch::Exact("braintrust.functions"), 70),
        detect_scope(KeyMatch::Exact("braintrust"), 70),
    ],
    observation_types: &[
        rule(3, "braintrust.span_attributes.type", &[], false, BT_TYPES),
        rule(3, "braintrust.span_attributes", &["type"], false, BT_TYPES),
        rule(
            3,
            "braintrust.span_attributes",
            &["purpose"],
            false,
            &[("scorer", ObservationType::Evaluator)],
        ),
    ],
    fields: &[
        field(
            Target::Input,
            &[
                span("braintrust.input_json", RAW),
                span("braintrust.input", RAW),
                prefix("braintrust.input.", Decoder::Indexed(&FLAT_MESSAGES)),
            ],
        ),
        field(
            Target::Output,
            &[
                span("braintrust.output_json", RAW),
                span("braintrust.output", RAW),
                prefix("braintrust.output.", Decoder::Indexed(&FLAT_MESSAGES)),
            ],
        ),
        field(
            Target::Metadata,
            &[
                span("braintrust.metadata", Decoder::JsonObject),
                prefix("braintrust.metadata.", Decoder::PrefixMap(&NATIVE)),
            ],
        ),
        field(
            Target::ModelRequested,
            &[
                span("gen_ai.request.model", Decoder::String),
                member("braintrust.metadata", &["model"], Decoder::String),
                span("braintrust.metadata.model", Decoder::String),
            ],
        ),
        field(
            Target::Provider,
            &[
                member("braintrust.metadata", &["provider"], Decoder::String),
                span("braintrust.metadata.provider", Decoder::String),
                own("provider", Decoder::String),
            ],
        ),
        field(
            Target::ToolDefinitions,
            &[member("braintrust.metadata", &["tools"], Decoder::Json)],
        ),
        field(
            Target::PromptVersion,
            &[member(
                "braintrust.metadata",
                &["prompt", "version"],
                Decoder::StringOrInt,
            )],
        ),
        field(
            Target::UsageAll,
            &[
                span("braintrust.metrics", Decoder::UsageObject(&BT_USAGE)),
                prefix("braintrust.metrics.", Decoder::PrefixUsage(&BT_USAGE)),
            ],
        ),
        field(
            Target::CompletionStartTime,
            &[
                member(
                    "braintrust.metrics",
                    &["time_to_first_token"],
                    Decoder::OffsetFromStart(TimeUnit::Seconds),
                ),
                span(
                    "braintrust.metrics.time_to_first_token",
                    Decoder::OffsetFromStart(TimeUnit::Seconds),
                ),
            ],
        ),
        field(
            Target::Level,
            &[
                member(
                    "braintrust.span_attributes",
                    &["log_level"],
                    Decoder::Level(&BT_LEVELS),
                ),
                span(
                    "braintrust.span_attributes.log_level",
                    Decoder::Level(&BT_LEVELS),
                ),
            ],
        ),
        field(
            Target::Tags,
            &[span(
                "braintrust.tags",
                Decoder::Tags {
                    split_commas: false,
                },
            )],
        ),
        field(
            Target::FinishReasons,
            &[
                span("gen_ai.finish_reason", Decoder::String),
                own("stop_reason", Decoder::String),
            ],
        ),
        field(Target::ToolNames, &[own("tool.name", Decoder::String)]),
        field(
            Target::SessionId,
            &[own("adk.agent.session_id", Decoder::String)],
        ),
    ],
    media: &[],
};

static BT_LEVELS: LevelMap = LevelMap {
    numbers: &[],
    names: &[
        ("trace", Level::Debug),
        ("debug", Level::Debug),
        ("info", Level::Default),
        ("warn", Level::Warning),
        ("error", Level::Error),
        ("fatal", Level::Error),
    ],
};
static BT_USAGE: UsageMap = UsageMap {
    aliases: &[
        ("prompt_tokens", "input"),
        ("completion_tokens", "output"),
        ("tokens", "total"),
        ("prompt_cached_tokens", "cache_read"),
        ("prompt_cache_creation_tokens", "cache_write"),
        ("completion_reasoning_tokens", "reasoning"),
    ],
    ignore: &[
        "time_to_first_token",
        "duration",
        "start",
        "end",
        "estimated_cost",
    ],
    keep_other: true,
    sums: &[(
        "cache_write",
        &[
            "prompt_cache_creation_5m_tokens",
            "prompt_cache_creation_1h_tokens",
        ],
    )],
    exclusive_bases: &[],
};
static FLAT_MESSAGES: IndexedLayout = IndexedLayout {
    wrappers: &[],
    json_leaves: &[],
    tool_calls: None,
    finish_reason: None,
};

/// The span types, shared by the flattened key and the member of the span-attributes object.
const BT_TYPES: &[(&str, ObservationType)] = &[
    ("llm", ObservationType::Generation),
    ("tool", ObservationType::Tool),
    ("function_call", ObservationType::Tool),
    ("agent", ObservationType::Agent),
    ("score", ObservationType::Evaluator),
    ("classifier", ObservationType::Evaluator),
    ("eval", ObservationType::Chain),
    ("task", ObservationType::Span),
    ("function", ObservationType::Span),
    ("log", ObservationType::Event),
];
