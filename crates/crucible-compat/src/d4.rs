//! The table of dialect 4 (contract 10.1).

use crucible_records::{ObservationType, UsageEntry};

use crate::*;

pub static DIALECT_4: DialectTable = DialectTable {
    dialect: 4,
    encoding: ValueEncoding::Native,
    detect: &[
        detect_key(KeyMatch::Exact("ai.operationId"), 90),
        detect_scope(KeyMatch::Exact("ai"), 40),
    ],
    observation_types: &[rule(
        7,
        "ai.operationId",
        &[],
        false,
        &[
            ("ai.generateText", ObservationType::Agent),
            ("ai.streamText", ObservationType::Agent),
            ("ai.generateObject", ObservationType::Agent),
            ("ai.streamObject", ObservationType::Agent),
            ("ai.workflowAgent.stream", ObservationType::Agent),
            ("ai.harness", ObservationType::Agent),
            ("ai.generateText.doGenerate", ObservationType::Generation),
            ("ai.streamText.doStream", ObservationType::Generation),
            ("ai.generateObject.doGenerate", ObservationType::Generation),
            ("ai.streamObject.doStream", ObservationType::Generation),
            ("ai.toolCall", ObservationType::Tool),
            ("ai.embed", ObservationType::Embedding),
            ("ai.embedMany", ObservationType::Embedding),
            ("ai.embed.doEmbed", ObservationType::Embedding),
            ("ai.embedMany.doEmbed", ObservationType::Embedding),
            ("ai.rerank", ObservationType::Retriever),
            ("ai.rerank.doRerank", ObservationType::Retriever),
            ("ai.decide.doDecide", ObservationType::Generation),
            ("ai.generateSpeech", ObservationType::Generation),
            ("ai.transcribe", ObservationType::Generation),
            ("ai.streamTranscribe", ObservationType::Generation),
        ],
    )],
    fields: &[
        field(
            Target::TraceName,
            &[span("ai.telemetry.functionId", Decoder::String)],
        ),
        field(
            Target::Metadata,
            &[
                prefix("ai.telemetry.metadata.", Decoder::PrefixMap(&NATIVE)),
                prefix("ai.settings.context.", Decoder::PrefixMap(&NATIVE)),
            ],
        ),
        field(
            Target::UserId,
            &[span("ai.telemetry.metadata.userId", Decoder::StringOrInt)],
        ),
        field(
            Target::SessionId,
            &[span(
                "ai.telemetry.metadata.sessionId",
                Decoder::StringOrInt,
            )],
        ),
        field(
            Target::TraceTags,
            &[span(
                "ai.telemetry.metadata.tags",
                Decoder::Tags {
                    split_commas: false,
                },
            )],
        ),
        field(
            Target::ModelRequested,
            &[span("ai.model.id", Decoder::String)],
        ),
        field(
            Target::Provider,
            &[span("ai.model.provider", Decoder::String)],
        ),
        field(
            Target::ModelResponse,
            &[span("ai.response.model", Decoder::String)],
        ),
        field(
            Target::FinishReasons,
            &[span("ai.response.finishReason", Decoder::String)],
        ),
        field(
            Target::Usage(UsageEntry::INPUT),
            &[
                span("ai.usage.inputTokens", Decoder::Integer),
                span("ai.usage.promptTokens", Decoder::Integer),
                span("ai.usage.tokens", Decoder::Integer),
                span("ai.usage.input_tokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::OUTPUT),
            &[
                span("ai.usage.outputTokens", Decoder::Integer),
                span("ai.usage.completionTokens", Decoder::Integer),
                span("ai.usage.output_tokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::TOTAL),
            &[
                span("ai.usage.totalTokens", Decoder::Integer),
                span("ai.usage.total_tokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_READ),
            &[
                span(
                    "ai.usage.inputTokenDetails.cacheReadTokens",
                    Decoder::Integer,
                ),
                span("ai.usage.cachedInputTokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Usage(UsageEntry::CACHE_WRITE),
            &[span(
                "ai.usage.inputTokenDetails.cacheWriteTokens",
                Decoder::Integer,
            )],
        ),
        field(
            Target::Usage(UsageEntry::REASONING),
            &[
                span(
                    "ai.usage.outputTokenDetails.reasoningTokens",
                    Decoder::Integer,
                ),
                span("ai.usage.reasoningTokens", Decoder::Integer),
            ],
        ),
        field(
            Target::Input,
            &[
                span("ai.prompt.messages", RAW),
                span("ai.prompt", RAW),
                span("ai.toolCall.args", RAW),
                span("ai.value", RAW),
                span("ai.values", Decoder::JsonStrings),
                span("ai.documents", Decoder::JsonStrings),
                span("ai.request.text", RAW),
                span("ai.decision.state", RAW),
            ],
        ),
        field(
            Target::Output,
            &[
                span("ai.response.text", RAW),
                span("ai.response.object", RAW),
                span("ai.toolCall.result", RAW),
                span("ai.embedding", RAW),
                span("ai.embeddings", Decoder::JsonStrings),
                span("ai.ranking", Decoder::JsonStrings),
                span("ai.decision.answers", RAW),
            ],
        ),
        field(
            Target::ToolCalls,
            &[span(
                "ai.response.toolCalls",
                Decoder::ToolCalls(&AI_TOOL_CALL),
            )],
        ),
        field(
            Target::ToolNames,
            &[span("ai.toolCall.name", Decoder::String)],
        ),
        field(
            Target::ToolDefinitions,
            &[span("ai.prompt.tools", Decoder::JsonStrings)],
        ),
        field(
            Target::ModelParameters,
            &[
                span(
                    "ai.settings.maxOutputTokens",
                    Decoder::MapMember("max_tokens"),
                ),
                span("ai.settings.temperature", Decoder::MapMember("temperature")),
                span("ai.settings.topP", Decoder::MapMember("top_p")),
                span("ai.settings.topK", Decoder::MapMember("top_k")),
                span(
                    "ai.settings.presencePenalty",
                    Decoder::MapMember("presence_penalty"),
                ),
                span(
                    "ai.settings.frequencyPenalty",
                    Decoder::MapMember("frequency_penalty"),
                ),
                span(
                    "ai.settings.stopSequences",
                    Decoder::MapMember("stop_sequences"),
                ),
                span("ai.settings.seed", Decoder::MapMember("seed")),
                span("ai.settings.maxRetries", Decoder::MapMember("max_retries")),
                span("ai.settings.output", Decoder::MapMember("output")),
                span("ai.prompt.toolChoice", Decoder::MapMember("tool_choice")),
            ],
        ),
        field(
            Target::CompletionStartTime,
            &[
                span(
                    "ai.response.msToFirstChunk",
                    Decoder::OffsetFromStart(TimeUnit::Millis),
                ),
                span(
                    "ai.stream.msToFirstChunk",
                    Decoder::OffsetFromStart(TimeUnit::Millis),
                ),
            ],
        ),
    ],
    media: &[],
};

static AI_TOOL_CALL: ToolCallShape = ToolCallShape {
    id: &["toolCallId"],
    name: &["toolName"],
    arguments: &["input"],
};
