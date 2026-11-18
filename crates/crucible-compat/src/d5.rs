//! The table of dialect 5 (contract 10.1).

use crucible_records::{Level, ObservationType};

use crate::*;

pub static DIALECT_5: DialectTable = DialectTable {
    dialect: 5,
    encoding: ValueEncoding::JsonText,
    detect: &[
        detect_scope(KeyMatch::Exact("mlflow.tracing.provider"), 100),
        detect_key(KeyMatch::Prefix("mlflow."), 70),
    ],
    observation_types: &[
        rule(
            9,
            "mlflow.spanType",
            &[],
            true,
            &[
                ("LLM", ObservationType::Generation),
                ("CHAT_MODEL", ObservationType::Generation),
                ("EMBEDDING", ObservationType::Embedding),
                ("TOOL", ObservationType::Tool),
                ("AGENT", ObservationType::Agent),
                ("CHAIN", ObservationType::Chain),
                ("RETRIEVER", ObservationType::Retriever),
                ("GUARDRAIL", ObservationType::Guardrail),
                ("EVALUATOR", ObservationType::Evaluator),
                ("RERANKER", ObservationType::Retriever),
                ("WORKFLOW", ObservationType::Chain),
                ("TASK", ObservationType::Span),
                ("MEMORY", ObservationType::Span),
                ("PARSER", ObservationType::Span),
                ("UNKNOWN", ObservationType::Span),
            ],
        ),
        rule(
            9,
            "gen_ai.operation.name",
            &[],
            false,
            &[
                ("CHAIN", ObservationType::Chain),
                ("RETRIEVER", ObservationType::Retriever),
                ("GUARDRAIL", ObservationType::Guardrail),
                ("EVALUATOR", ObservationType::Evaluator),
                ("RERANKER", ObservationType::Retriever),
                ("WORKFLOW", ObservationType::Chain),
            ],
        ),
    ],
    fields: &[
        field(Target::Input, &[span("mlflow.spanInputs", RAW)]),
        field(Target::Output, &[span("mlflow.spanOutputs", RAW)]),
        field(
            Target::UsageAll,
            &[span(
                "mlflow.chat.tokenUsage",
                Decoder::UsageObject(&MLFLOW_USAGE),
            )],
        ),
        field(
            Target::CostAll,
            &[span("mlflow.llm.cost", Decoder::CostObject(&MLFLOW_COST))],
        ),
        field(
            Target::ModelRequested,
            &[span("mlflow.llm.model", Decoder::String)],
        ),
        field(
            Target::Provider,
            &[span("mlflow.llm.provider", Decoder::String)],
        ),
        field(
            Target::ToolDefinitions,
            &[span("mlflow.chat.tools", Decoder::Json)],
        ),
        field(
            Target::Level,
            &[span("mlflow.spanLogLevel", Decoder::Level(&MLFLOW_LEVELS))],
        ),
        field(
            Target::PromptName,
            &[member(
                "mlflow.linkedPrompts",
                &["0", "name"],
                Decoder::String,
            )],
        ),
        field(
            Target::PromptVersion,
            &[member(
                "mlflow.linkedPrompts",
                &["0", "version"],
                Decoder::StringOrInt,
            )],
        ),
        field(
            Target::TraceMetadata,
            &[prefix("mlflow.traceTag.", Decoder::PrefixMap(&NATIVE))],
        ),
        field(
            Target::CompletionStartTime,
            &[event_time("mlflow.chunk.item.0")],
        ),
    ],
    media: &[MediaToken {
        prefix: "mlflow-attachment://",
        suffix: "",
    }],
};

static MLFLOW_USAGE: UsageMap = UsageMap {
    aliases: &[
        ("input_tokens", "input"),
        ("output_tokens", "output"),
        ("total_tokens", "total"),
        ("cache_read_input_tokens", "cache_read"),
        ("cache_creation_input_tokens", "cache_write"),
    ],
    ignore: &[],
    keep_other: true,
    sums: &[],
    exclusive_bases: &[],
};
static MLFLOW_COST: UsageMap = UsageMap {
    aliases: &[
        ("input_cost", "input"),
        ("output_cost", "output"),
        ("total_cost", "total"),
    ],
    ignore: &[],
    keep_other: false,
    sums: &[],
    exclusive_bases: &[],
};
static MLFLOW_LEVELS: LevelMap = LevelMap {
    names: &[],
    numbers: &[
        (10, Level::Debug),
        (20, Level::Default),
        (30, Level::Warning),
        (40, Level::Error),
        (50, Level::Error),
    ],
};
