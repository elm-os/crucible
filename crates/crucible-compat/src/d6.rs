//! The table of dialect 6 (contract 10.1).

use crucible_records::{Level, ObservationType};

use crate::*;

pub static DIALECT_6: DialectTable = DialectTable {
    dialect: 6,
    encoding: ValueEncoding::Native,
    detect: &[
        detect_scope(KeyMatch::Exact("pydantic-ai"), 100),
        detect_key(KeyMatch::Prefix("pydantic_ai."), 70),
        detect_key(KeyMatch::Prefix("gen_ai.aggregated_usage."), 70),
    ],
    observation_types: &[rule(
        10,
        "gen_ai.operation.name",
        &[],
        false,
        &[
            ("image_generation", ObservationType::Generation),
            ("decide", ObservationType::Generation),
        ],
    )],
    fields: &[
        field(
            Target::Cost("total"),
            &[span("operation.cost", Decoder::Usd)],
        ),
        field(
            Target::UsageAll,
            &[prefix(
                "gen_ai.usage.details.",
                Decoder::PrefixUsage(&PAI_DETAILS),
            )],
        ),
        field(
            Target::Input,
            &[
                span("pydantic_ai.all_messages", GENAI),
                own("tool_arguments", RAW),
                own("inputs", RAW),
                own("prompt", RAW),
            ],
        ),
        field(
            Target::Output,
            &[own("final_result", RAW), own("tool_response", RAW)],
        ),
        field(Target::Metadata, &[own("metadata", Decoder::JsonObject)]),
        field(
            Target::CompletionStartTime,
            &[span(
                "gen_ai.client.operation.time_to_first_chunk",
                Decoder::OffsetFromStart(TimeUnit::Seconds),
            )],
        ),
        field(
            Target::Level,
            &[span("logfire.level_num", Decoder::Level(&LOGFIRE_LEVELS))],
        ),
    ],
    media: &[],
};

static PAI_DETAILS: UsageMap = UsageMap {
    aliases: &[
        ("reasoning_tokens", "reasoning"),
        ("thinking_tokens", "reasoning"),
    ],
    ignore: &["cache_write_tokens", "cache_read_tokens"],
    keep_other: true,
    sums: &[],
    exclusive_bases: &[],
};
static LOGFIRE_LEVELS: LevelMap = LevelMap {
    names: &[],
    numbers: &[
        (1, Level::Debug),
        (5, Level::Debug),
        (9, Level::Default),
        (10, Level::Default),
        (13, Level::Warning),
        (17, Level::Error),
        (21, Level::Error),
    ],
};
