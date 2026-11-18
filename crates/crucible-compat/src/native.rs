//! The native tables (contract 10.6): the `crucible.*` keys, which form the first layer, and dialect 0, the
//! OpenTelemetry semantic-convention keys.

use crucible_records::{Level, ObservationType, UsageEntry};

use crate::*;

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

/// The level names of `crucible.level`.
static CRUCIBLE_LEVELS: LevelMap = LevelMap {
    names: &[
        ("debug", Level::Debug),
        ("default", Level::Default),
        ("warning", Level::Warning),
        ("error", Level::Error),
    ],
    numbers: &[],
};

/// Every request parameter except the model, which `model_requested` reads.
static GENAI_REQUEST: PrefixSpec = PrefixSpec {
    values: MapValues::Native,
    except: &["gen_ai.request.model"],
};

/// The `crucible.*` keys of contract 10.6: the first layer, read on every span whatever dialect is chosen. It has
/// the id of dialect 0, no detection rules and no `Own` sources.
pub static CRUCIBLE_KEYS: DialectTable = DialectTable {
    dialect: 0,
    encoding: ValueEncoding::Native,
    detect: &[],
    observation_types: &[rule(
        1,
        "crucible.observation.type",
        &[],
        true,
        OBSERVATION_TYPE_NAMES,
    )],
    fields: &[
        field(
            Target::TraceName,
            &[span("crucible.trace.name", Decoder::String)],
        ),
        field(
            Target::SessionId,
            &[span("crucible.session.id", Decoder::StringOrInt)],
        ),
        field(
            Target::UserId,
            &[span("crucible.user.id", Decoder::StringOrInt)],
        ),
        field(
            Target::Environment,
            &[span("crucible.environment", Decoder::String)],
        ),
        field(
            Target::Release,
            &[span("crucible.release", Decoder::String)],
        ),
        field(
            Target::Version,
            &[span("crucible.version", Decoder::String)],
        ),
        field(
            Target::Tags,
            &[span(
                "crucible.tags",
                Decoder::Tags {
                    split_commas: false,
                },
            )],
        ),
        field(
            Target::TraceTags,
            &[span(
                "crucible.trace.tags",
                Decoder::Tags {
                    split_commas: false,
                },
            )],
        ),
        field(
            Target::Metadata,
            &[prefix("crucible.metadata.", Decoder::PrefixMap(&NATIVE))],
        ),
        field(
            Target::TraceMetadata,
            &[prefix(
                "crucible.trace.metadata.",
                Decoder::PrefixMap(&NATIVE),
            )],
        ),
        field(
            Target::Level,
            &[span("crucible.level", Decoder::Level(&CRUCIBLE_LEVELS))],
        ),
        field(
            Target::TracePublic,
            &[span("crucible.trace.public", Decoder::Bool)],
        ),
        field(Target::Input, &[span("crucible.input", RAW)]),
        field(Target::Output, &[span("crucible.output", RAW)]),
        field(
            Target::PromptName,
            &[span("crucible.prompt.name", Decoder::String)],
        ),
        field(
            Target::PromptVersion,
            &[span("crucible.prompt.version", Decoder::StringOrInt)],
        ),
        // `crucible.cost.total` and `crucible.cost.<usage key>`.
        field(Target::CostAll, &[prefix("crucible.cost.", Decoder::Usd)]),
        field(
            Target::CompletionStartTime,
            &[span("crucible.completion_start_time", Decoder::Timestamp)],
        ),
        field(
            Target::ModelParameters,
            &[prefix(
                "crucible.model_parameters.",
                Decoder::PrefixMap(&NATIVE),
            )],
        ),
    ],
    media: &[],
};

/// Dialect 0, native (contract 10.1): the semantic-convention keys of each field concept the contract names. It
/// owns every semantic-convention key that several dialects write, so every instrumentation benefits.
///
/// `release`: the resource key `service.version` is its source and stays the source of `service_version`. It is
/// the one key that feeds two fields ([`SHARED_KEYS`]).
pub static DIALECT_0: DialectTable = DialectTable {
    dialect: 0,
    encoding: ValueEncoding::Native,
    detect: &[],
    observation_types: &[
        rule(
            4,
            "mcp.method.name",
            &[],
            false,
            &[("tools/call", ObservationType::Tool)],
        ),
        rule(
            6,
            "gen_ai.operation.name",
            &[],
            false,
            &[
                ("chat", ObservationType::Generation),
                ("text_completion", ObservationType::Generation),
                ("generate_content", ObservationType::Generation),
                ("embeddings", ObservationType::Embedding),
                ("execute_tool", ObservationType::Tool),
                ("create_agent", ObservationType::Agent),
                ("invoke_agent", ObservationType::Agent),
            ],
        ),
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
            Target::SessionId,
            &[
                span("session.id", Decoder::StringOrInt),
                span("gen_ai.conversation.id", Decoder::String),
            ],
        ),
        field(Target::UserId, &[span("user.id", Decoder::String)]),
        field(
            Target::Environment,
            &[
                resource("deployment.environment.name", Decoder::String),
                resource("deployment.environment", Decoder::String),
            ],
        ),
        field(
            Target::Release,
            &[resource("service.version", Decoder::String)],
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
            &[
                span("gen_ai.provider.name", Decoder::String),
                span("gen_ai.system", Decoder::String),
            ],
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
                span("gen_ai.input.messages", GENAI),
                span("gen_ai.tool.call.arguments", RAW),
            ],
        ),
        field(
            Target::Output,
            &[
                span("gen_ai.output.messages", GENAI),
                span("gen_ai.tool.call.result", RAW),
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
        field(
            Target::ToolNames,
            &[span("gen_ai.tool.name", Decoder::String)],
        ),
        field(
            Target::ModelParameters,
            &[
                prefix("gen_ai.request.", Decoder::PrefixMap(&GENAI_REQUEST)),
                span(
                    "openai.request.service_tier",
                    Decoder::MapMember("service_tier"),
                ),
            ],
        ),
    ],
    media: &[],
};
