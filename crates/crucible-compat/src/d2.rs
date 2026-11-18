//! The table of dialect 2 (contract 10.1).

use crucible_records::{ObservationType, UsageEntry};

use crate::*;

pub static DIALECT_2: DialectTable = DialectTable {
    dialect: 2,
    encoding: ValueEncoding::Native,
    detect: &[
        detect_key(KeyMatch::Exact("openinference.span.kind"), 90),
        detect_key(KeyMatch::Prefix("llm.input_messages."), 70),
        detect_key(KeyMatch::Prefix("llm.output_messages."), 70),
        detect_key(KeyMatch::Prefix("llm.token_count."), 70),
        detect_key(KeyMatch::Exact("llm.invocation_parameters"), 70),
        detect_scope(KeyMatch::Prefix("openinference.instrumentation."), 70),
        detect_scope(KeyMatch::Prefix("@arizeai/openinference-"), 70),
        detect_scope(KeyMatch::Exact("openinference-core"), 70),
    ],
    observation_types: &[rule(
        5,
        "openinference.span.kind",
        &[],
        true,
        &[
            ("LLM", ObservationType::Generation),
            ("EMBEDDING", ObservationType::Embedding),
            ("CHAIN", ObservationType::Chain),
            ("RETRIEVER", ObservationType::Retriever),
            ("TOOL", ObservationType::Tool),
            ("AGENT", ObservationType::Agent),
            ("GUARDRAIL", ObservationType::Guardrail),
            ("EVALUATOR", ObservationType::Evaluator),
            ("RERANKER", ObservationType::Retriever),
            ("PROMPT", ObservationType::Chain),
            ("DECISION", ObservationType::Generation),
            ("UNKNOWN", ObservationType::Span),
        ],
    )],
    fields: &[
        field(
            Target::Input,
            &[
                prefix("llm.input_messages.", Decoder::Indexed(&OI_MESSAGES)),
                span("input.value", RAW),
            ],
        ),
        field(
            Target::Output,
            &[
                prefix("llm.output_messages.", Decoder::Indexed(&OI_MESSAGES)),
                span("output.value", RAW),
            ],
        ),
        field(
            Target::ToolDefinitions,
            &[prefix("llm.tools.", Decoder::IndexedList(&OI_TOOLS))],
        ),
        field(Target::ToolNames, &[own("tool.name", Decoder::String)]),
        field(
            Target::ModelResponse,
            &[
                span("llm.response.model_name", Decoder::String),
                span("llm.model_name", Decoder::String),
                span("decision.response.model_name", Decoder::String),
                span("decision.model_name", Decoder::String),
                span("embedding.model_name", Decoder::String),
            ],
        ),
        field(
            Target::ModelRequested,
            &[
                span("llm.request.model_name", Decoder::String),
                span("decision.request.model_name", Decoder::String),
            ],
        ),
        field(
            Target::Provider,
            &[
                span("llm.provider", Decoder::String),
                span("llm.system", Decoder::String),
                span("decision.provider", Decoder::String),
                span("decision.system", Decoder::String),
            ],
        ),
        field(
            Target::ModelParameters,
            &[
                span("llm.invocation_parameters", Decoder::JsonObject),
                span("embedding.invocation_parameters", Decoder::JsonObject),
            ],
        ),
        field(
            Target::FinishReasons,
            &[span("llm.finish_reason", Decoder::String)],
        ),
        field(
            Target::Usage(UsageEntry::INPUT),
            &[
                span("llm.token_count.prompt", Decoder::Integer),
                span("decision.token_count.input", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::OUTPUT),
            &[
                span("llm.token_count.completion", Decoder::Integer),
                span("decision.token_count.output", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::TOTAL),
            &[span("llm.token_count.total", Decoder::Integer)],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_READ),
            &[span(
                "llm.token_count.prompt_details.cache_read",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_WRITE),
            &[span(
                "llm.token_count.prompt_details.cache_write",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::REASONING),
            &[span(
                "llm.token_count.completion_details.reasoning",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage("input_audio_tokens"),
            &[span(
                "llm.token_count.prompt_details.audio",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage("output_audio_tokens"),
            &[span(
                "llm.token_count.completion_details.audio",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Cost("total"),
            &[span("llm.cost.total", Decoder::Usd)],
        ),
        field(
            Target::Cost(UsageEntry::INPUT),
            &[span("llm.cost.prompt", Decoder::Usd)],
        ),
        field(
            Target::Cost(UsageEntry::OUTPUT),
            &[span("llm.cost.completion", Decoder::Usd)],
        ),
        field(
            Target::Cost(UsageEntry::CACHE_READ),
            &[span("llm.cost.prompt_details.cache_read", Decoder::Usd)],
        ),
        field(
            Target::Cost(UsageEntry::CACHE_WRITE),
            &[span("llm.cost.prompt_details.cache_write", Decoder::Usd)],
        ),
        field(
            Target::Cost(UsageEntry::REASONING),
            &[span("llm.cost.completion_details.reasoning", Decoder::Usd)],
        ),
        field(
            Target::Cost("input_audio_tokens"),
            &[span("llm.cost.prompt_details.audio", Decoder::Usd)],
        ),
        field(
            Target::Cost("output_audio_tokens"),
            &[span("llm.cost.completion_details.audio", Decoder::Usd)],
        ),
        field(Target::Metadata, &[own("metadata", Decoder::JsonObject)]),
        field(
            Target::Tags,
            &[span(
                "tag.tags",
                Decoder::Tags {
                    split_commas: false,
                },
            )],
        ),
        field(
            Target::PromptVersion,
            &[span("llm.prompt_template.version", Decoder::StringOrInt)],
        ),
        field(
            Target::CompletionStartTime,
            &[event_time("First Token Stream Event")],
        ),
    ],
    media: &[],
};

static OI_MESSAGES: IndexedLayout = IndexedLayout {
    wrappers: &["message", "message_content", "tool_call"],
    json_leaves: &[],
    tool_calls: Some((
        "tool_calls",
        ToolCallShape {
            id: &["id"],
            name: &["function", "name"],
            arguments: &["function", "arguments"],
        },
    )),
    finish_reason: None,
};
static OI_TOOLS: IndexedLayout = IndexedLayout {
    wrappers: &["tool"],
    json_leaves: &["json_schema"],
    tool_calls: None,
    finish_reason: None,
};
