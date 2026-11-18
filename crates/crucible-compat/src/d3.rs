//! The table of dialect 3 (contract 10.1).

use crucible_records::{ObservationType, UsageEntry};

use crate::*;

pub static DIALECT_3: DialectTable = DialectTable {
    dialect: 3,
    encoding: ValueEncoding::Native,
    detect: &[
        detect_scope(KeyMatch::Exact("traceloop.tracer"), 100),
        detect_key(KeyMatch::Prefix("traceloop."), 90),
        detect_key(KeyMatch::Exact("llm.request.type"), 70),
        detect_key(KeyMatch::Exact("llm.usage.total_tokens"), 70),
        detect_key(KeyMatch::Exact("llm.is_streaming"), 70),
        detect_key(KeyMatch::Exact("llm.headers"), 70),
        detect_key(KeyMatch::Exact("llm.user"), 70),
        detect_key(KeyMatch::Exact("gen_ai.usage.total_tokens"), 40),
        detect_key(KeyMatch::Exact("gen_ai.is_streaming"), 40),
        detect_key(KeyMatch::Exact("gen_ai.usage.reasoning_tokens"), 40),
        detect_key(KeyMatch::Prefix("gen_ai.prompt."), 40),
        detect_key(KeyMatch::Prefix("gen_ai.completion."), 40),
        detect_key(KeyMatch::Exact("gen_ai.usage.cache_read_input_tokens"), 40),
        detect_key(
            KeyMatch::Exact("gen_ai.usage.cache_creation_input_tokens"),
            40,
        ),
    ],
    observation_types: &[
        rule(
            8,
            "traceloop.span.kind",
            &[],
            true,
            &[
                ("workflow", ObservationType::Chain),
                ("task", ObservationType::Chain),
                ("agent", ObservationType::Agent),
                ("tool", ObservationType::Tool),
                ("unknown", ObservationType::Span),
                ("session", ObservationType::Span),
                ("server", ObservationType::Span),
            ],
        ),
        rule(
            8,
            "gen_ai.operation.name",
            &[],
            false,
            &[
                ("execute_task", ObservationType::Chain),
                ("vector_db_retrieve", ObservationType::Retriever),
                ("guardrail.run", ObservationType::Guardrail),
                ("guard", ObservationType::Guardrail),
            ],
        ),
        rule(
            8,
            "llm.request.type",
            &[],
            true,
            &[
                ("chat", ObservationType::Generation),
                ("completion", ObservationType::Generation),
                ("embedding", ObservationType::Embedding),
                ("rerank", ObservationType::Retriever),
                ("unknown", ObservationType::Span),
            ],
        ),
    ],
    fields: &[
        field(
            Target::Input,
            &[
                prefix("gen_ai.prompt.", Decoder::Indexed(&OL_MESSAGES)),
                span("traceloop.entity.input", RAW),
                span("gen_ai.task.input", RAW),
            ],
        ),
        field(
            Target::Output,
            &[
                prefix("gen_ai.completion.", Decoder::Indexed(&OL_MESSAGES)),
                span("traceloop.entity.output", RAW),
                span("gen_ai.task.output", RAW),
            ],
        ),
        field(
            Target::ToolDefinitions,
            &[prefix(
                "llm.request.functions.",
                Decoder::IndexedList(&OL_FUNCTIONS),
            )],
        ),
        field(
            Target::FinishReasons,
            &[span("llm.response.stop_reason", Decoder::String)],
        ),
        field(
            Target::Usage(UsageEntry::TOTAL),
            &[
                span("gen_ai.usage.total_tokens", Decoder::Integer),
                span("llm.usage.total_tokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::REASONING),
            &[
                span("gen_ai.usage.reasoning_tokens", Decoder::Integer),
                span("llm.usage.reasoning_tokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_READ),
            &[span(
                "gen_ai.usage.cache_read_input_tokens",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_WRITE),
            &[span(
                "gen_ai.usage.cache_creation_input_tokens",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::INPUT),
            &[span("gen_ai.usage.prompt_tokens", Decoder::Integer)],
        ),
        field(
            Target::Usage(UsageEntry::OUTPUT),
            &[span("gen_ai.usage.completion_tokens", Decoder::Integer)],
        ),
        field(
            Target::SessionId,
            &[span(
                "traceloop.association.properties.session_id",
                Decoder::StringOrInt,
            )],
        ),
        field(
            Target::UserId,
            &[span(
                "traceloop.association.properties.user_id",
                Decoder::StringOrInt,
            )],
        ),
        field(
            Target::Metadata,
            &[prefix(
                "traceloop.association.properties.",
                Decoder::PrefixMap(&NATIVE),
            )],
        ),
        field(
            Target::TraceName,
            &[span("traceloop.workflow.name", Decoder::String)],
        ),
        field(
            Target::PromptName,
            &[span("traceloop.prompt.key", Decoder::String)],
        ),
        field(
            Target::PromptVersion,
            &[span("traceloop.prompt.version", Decoder::StringOrInt)],
        ),
        field(
            Target::ModelParameters,
            &[
                span(
                    "llm.frequency_penalty",
                    Decoder::MapMember("frequency_penalty"),
                ),
                span(
                    "llm.presence_penalty",
                    Decoder::MapMember("presence_penalty"),
                ),
                span("llm.top_k", Decoder::MapMember("top_k")),
                span(
                    "llm.chat.stop_sequences",
                    Decoder::MapMember("stop_sequences"),
                ),
                span(
                    "llm.request.repetition_penalty",
                    Decoder::MapMember("repetition_penalty"),
                ),
                span(
                    "llm.request.reasoning_effort",
                    Decoder::MapMember("reasoning_effort"),
                ),
                span("gen_ai.is_streaming", Decoder::MapMember("stream")),
                span("llm.is_streaming", Decoder::MapMember("stream")),
            ],
        ),
        field(
            Target::CompletionStartTime,
            &[
                event_time("gen_ai.content.completion.chunk"),
                event_time("llm.content.completion.chunk"),
            ],
        ),
    ],
    media: &[],
};

static OL_MESSAGES: IndexedLayout = IndexedLayout {
    wrappers: &[],
    json_leaves: &[],
    tool_calls: Some((
        "tool_calls",
        ToolCallShape {
            id: &["id"],
            name: &["name"],
            arguments: &["arguments"],
        },
    )),
    finish_reason: Some("finish_reason"),
};
static OL_FUNCTIONS: IndexedLayout = IndexedLayout {
    wrappers: &[],
    json_leaves: &["parameters"],
    tool_calls: None,
    finish_reason: None,
};
