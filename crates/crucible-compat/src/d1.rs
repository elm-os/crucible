//! The table of dialect 1 (contract 10.1).

use crucible_records::{Level, ObservationType};

use crate::*;

pub static DIALECT_1: DialectTable = DialectTable {
    dialect: 1,
    encoding: ValueEncoding::Native,
    detect: &[
        detect_scope(KeyMatch::Prefix("langfuse-sdk"), 100),
        detect_key(KeyMatch::Exact("langfuse.observation.type"), 90),
        detect_key(KeyMatch::Prefix("langfuse."), 40),
    ],
    observation_types: &[rule(
        2,
        "langfuse.observation.type",
        &[],
        true,
        &[
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
        ],
    )],
    fields: &[
        field(
            Target::Level,
            &[span(
                "langfuse.observation.level",
                Decoder::Level(&LF_LEVELS),
            )],
        ),
        field(
            Target::StatusMessage,
            &[span("langfuse.observation.status_message", Decoder::String)],
        ),
        field(Target::Input, &[span("langfuse.observation.input", RAW)]),
        field(Target::Output, &[span("langfuse.observation.output", RAW)]),
        field(
            Target::Metadata,
            &[
                prefix(
                    "langfuse.observation.metadata.",
                    Decoder::PrefixMap(&LF_GATED),
                ),
                span("langfuse.observation.metadata", Decoder::JsonObject),
                span("langfuse.metadata", Decoder::JsonObject),
                prefix("langfuse.metadata.", Decoder::PrefixMap(&NATIVE)),
            ],
        ),
        field(
            Target::TraceMetadata,
            &[
                prefix("langfuse.trace.metadata.", Decoder::PrefixMap(&LF_GATED)),
                span("langfuse.trace.metadata", Decoder::JsonObject),
            ],
        ),
        field(
            Target::ModelResponse,
            &[span("langfuse.observation.model.name", Decoder::String)],
        ),
        field(
            Target::ModelParameters,
            &[span(
                "langfuse.observation.model.parameters",
                Decoder::JsonObject,
            )],
        ),
        field(
            Target::UsageAll,
            &[span(
                "langfuse.observation.usage_details",
                Decoder::UsageObject(&LF_USAGE),
            )],
        ),
        field(
            Target::CostAll,
            &[span(
                "langfuse.observation.cost_details",
                Decoder::CostObject(&LF_USAGE),
            )],
        ),
        field(
            Target::CompletionStartTime,
            &[span(
                "langfuse.observation.completion_start_time",
                Decoder::Timestamp,
            )],
        ),
        field(
            Target::PromptName,
            &[
                span("langfuse.observation.prompt.name", Decoder::String),
                span("langfuse.prompt.name", Decoder::String),
            ],
        ),
        field(
            Target::PromptVersion,
            &[
                span("langfuse.observation.prompt.version", Decoder::StringOrInt),
                span("langfuse.prompt.version", Decoder::StringOrInt),
            ],
        ),
        field(
            Target::TraceName,
            &[span("langfuse.trace.name", Decoder::String)],
        ),
        field(
            Target::UserId,
            &[
                span("langfuse.user.id", Decoder::String),
                span("user.id", Decoder::String),
                span(
                    "langfuse.observation.metadata.langfuse_user_id",
                    Decoder::String,
                ),
                span("langfuse.trace.metadata.langfuse_user_id", Decoder::String),
                resource("langfuse.user.id", Decoder::String),
                resource("user.id", Decoder::String),
            ],
        ),
        field(
            Target::SessionId,
            &[
                span("langfuse.session.id", Decoder::String),
                span("session.id", Decoder::String),
                span("gen_ai.conversation.id", Decoder::String),
                span(
                    "langfuse.observation.metadata.langfuse_session_id",
                    Decoder::String,
                ),
                span(
                    "langfuse.trace.metadata.langfuse_session_id",
                    Decoder::String,
                ),
            ],
        ),
        field(
            Target::TraceTags,
            &[
                span("langfuse.trace.tags", Decoder::Tags { split_commas: true }),
                span("langfuse.tags", Decoder::Tags { split_commas: true }),
                span(
                    "langfuse.observation.metadata.langfuse_tags",
                    Decoder::Tags { split_commas: true },
                ),
                span(
                    "langfuse.trace.metadata.langfuse_tags",
                    Decoder::Tags { split_commas: true },
                ),
            ],
        ),
        field(
            Target::TracePublic,
            &[
                span("langfuse.trace.public", Decoder::Bool),
                span("langfuse.public", Decoder::Bool),
            ],
        ),
        field(
            Target::Environment,
            &[
                span("langfuse.environment", Decoder::String),
                resource("langfuse.environment", Decoder::String),
            ],
        ),
        field(
            Target::Release,
            &[
                span("langfuse.release", Decoder::String),
                resource("langfuse.release", Decoder::String),
            ],
        ),
        field(
            Target::Version,
            &[span("langfuse.version", Decoder::String)],
        ),
        field(
            Target::IsRoot,
            &[span("langfuse.internal.as_root", Decoder::Bool)],
        ),
        field(
            Target::IsAppRoot,
            &[span("langfuse.internal.is_app_root", Decoder::Bool)],
        ),
    ],
    media: &[MediaToken {
        prefix: "@@@langfuseMedia:",
        suffix: "@@@",
    }],
};

static LF_LEVELS: LevelMap = LevelMap {
    numbers: &[],
    names: &[
        ("debug", Level::Debug),
        ("trace", Level::Debug),
        ("verbose", Level::Debug),
        ("default", Level::Default),
        ("info", Level::Default),
        ("log", Level::Default),
        ("notice", Level::Default),
        ("ok", Level::Default),
        ("success", Level::Default),
        ("warning", Level::Warning),
        ("warn", Level::Warning),
        ("error", Level::Error),
        ("fatal", Level::Error),
        ("critical", Level::Error),
    ],
};
static LF_GATE: SdkGate = SdkGate {
    scope_prefix: "langfuse-sdk",
    names: &[
        ("python", "python"),
        ("javascript", "javascript"),
        ("nodejs", "javascript"),
        ("webjs", "javascript"),
    ],
    min_major: &[("python", 5), ("javascript", 6)],
};
static LF_GATED: PrefixSpec = PrefixSpec {
    values: MapValues::JsonWhen(&LF_GATE),
    except: &[],
};
static LF_USAGE: UsageMap = UsageMap {
    aliases: &[
        ("input", "input"),
        ("input_tokens", "input"),
        ("prompt_tokens", "input"),
        ("promptTokens", "input"),
        ("prompt_token_count", "input"),
        ("promptTokenCount", "input"),
        ("output", "output"),
        ("output_tokens", "output"),
        ("completion_tokens", "output"),
        ("completionTokens", "output"),
        ("candidates_token_count", "output"),
        ("candidatesTokenCount", "output"),
        ("total", "total"),
        ("total_tokens", "total"),
        ("totalTokens", "total"),
        ("input_cached_tokens", "cache_read"),
        ("input_cache_read", "cache_read"),
        ("cache_read_input_tokens", "cache_read"),
        ("cached_content_token_count", "cache_read"),
        ("input_cache_creation", "cache_write"),
        ("cache_creation_input_tokens", "cache_write"),
        ("input_cache_write_tokens", "cache_write"),
        ("cache_write_tokens", "cache_write"),
        ("output_reasoning_tokens", "reasoning"),
        ("output_reasoning", "reasoning"),
        ("reasoning_tokens", "reasoning"),
        ("thoughts_token_count", "reasoning"),
        ("thoughtsTokenCount", "reasoning"),
    ],
    ignore: &[],
    keep_other: true,
    sums: &[(
        "cache_write",
        &["input_cache_creation_5m", "input_cache_creation_1h"],
    )],
    exclusive_bases: &["input", "input_tokens", "output", "output_tokens"],
};
