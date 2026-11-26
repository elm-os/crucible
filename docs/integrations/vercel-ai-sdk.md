# Vercel AI SDK

The AI SDK records its calls as OpenTelemetry spans; an OTLP exporter sends them to `/v1/traces`. Which attributes
they carry depends on the version and the telemetry integration:

| AI SDK | Telemetry | Spans | Read as |
|---|---|---|---|
| 7 | `registerTelemetry(new LegacyOpenTelemetry())` from `@ai-sdk/otel` | `ai.*` attributes, tracer `ai` | Vercel AI SDK |
| 7 | `registerTelemetry(new OpenTelemetry())` from `@ai-sdk/otel` | OpenTelemetry GenAI conventions, tracer `gen_ai` | Native spans |
| 6 | `experimental_telemetry: { isEnabled: true }` on each call | `ai.*` attributes, tracer `ai` | Vercel AI SDK |

Verified with ai 6.0.333, and ai 7.0.177 with @ai-sdk/otel 1.0.177, exporting through @opentelemetry/sdk-node and
@opentelemetry/exporter-trace-otlp-proto 0.301.2.

## Pointing it at Crucible

Register an OTLP exporter with the OpenTelemetry Node SDK, before the first AI SDK call:

```js
import { NodeSDK } from "@opentelemetry/sdk-node";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";

const sdk = new NodeSDK({
  traceExporter: new OTLPTraceExporter({
    url: "http://127.0.0.1:4318/v1/traces",
    headers: { Authorization: `Bearer ${process.env.CRUCIBLE_SECRET_KEY}` },
  }),
});
sdk.start();
```

In a short-lived script, `await sdk.shutdown()` before it exits, so the last spans are sent.

`new OTLPTraceExporter()` without options reads the standard variables instead:

```sh
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20csk_..."
```

With AI SDK 7, register the integration once; telemetry is then on for every call, and `telemetry` names the call
(`model` is your language model):

```js
import { generateText, registerTelemetry } from "ai";
import { LegacyOpenTelemetry } from "@ai-sdk/otel";

registerTelemetry(new LegacyOpenTelemetry());
const { text } = await generateText({ model, prompt: "Summarize ticket 7", telemetry: { functionId: "summarize-ticket" } });
```

With AI SDK 6, turn it on per call. The `metadata` keys `userId`, `sessionId` and `tags` become the trace's user,
session and tags:

```js
import { generateText } from "ai";

const { text } = await generateText({
  model,
  prompt: "Summarize ticket 7",
  experimental_telemetry: {
    isEnabled: true,
    functionId: "summarize-ticket",
    metadata: { userId: "user-3", sessionId: "session-9", tags: ["support", "nightly"] },
  },
});
```

## What maps

For spans with the `ai.*` attributes:

| Crucible field | AI SDK attribute |
|---|---|
| Observation type | `ai.operationId`: `ai.generateText`, `ai.streamText`, `ai.generateObject` and `ai.streamObject` to agent; their `.doGenerate` and `.doStream` model calls to generation; `ai.toolCall` to tool; `ai.embed` and `ai.embedMany` to embedding; `ai.rerank` to retriever |
| Trace name | `ai.telemetry.functionId` |
| User, session, trace tags | `ai.telemetry.metadata.userId`, `ai.telemetry.metadata.sessionId`, `ai.telemetry.metadata.tags` (AI SDK 6) |
| Metadata | `ai.telemetry.metadata.*`, `ai.settings.context.*` |
| Requested model, provider | `ai.model.id`, `ai.model.provider` |
| Model | `ai.response.model` |
| Finish reasons | `ai.response.finishReason` |
| Token usage | `ai.usage.inputTokens`, `ai.usage.outputTokens`, `ai.usage.totalTokens`, `ai.usage.inputTokenDetails.cacheReadTokens`, `ai.usage.cachedInputTokens`, `ai.usage.inputTokenDetails.cacheWriteTokens`, `ai.usage.outputTokenDetails.reasoningTokens`, `ai.usage.reasoningTokens` |
| Input | `ai.prompt.messages`, `ai.prompt`, `ai.toolCall.args`, `ai.value`, `ai.values` |
| Output | `ai.response.text`, `ai.response.object`, `ai.toolCall.result`, `ai.embedding`, `ai.embeddings` |
| Tool calls, tool names, tool definitions | `ai.response.toolCalls`, `ai.toolCall.name`, `ai.prompt.tools` |
| Model parameters | `ai.settings.*` (temperature, top P and K, penalties, stop sequences, seed, maximum output tokens and retries), `ai.prompt.toolChoice` |
| Completion start | `ai.response.msToFirstChunk`, `ai.stream.msToFirstChunk` |

Spans from `new OpenTelemetry()` follow the OpenTelemetry GenAI conventions and map as native spans: the agent span
(`invoke_agent`) and the model call (`chat`) with its model, provider, messages and token counts.

## Known limits

- The operation span (`ai.generateText` and the like) names only the requested model, so its REST `model` is empty;
  its model call carries the model that answered. The operation span is flagged as a usage aggregate, so the trace's
  tokens count each model call once.
- On model-call spans the OpenTelemetry GenAI keys the AI SDK also writes win, and the `ai.*` duplicates stay among
  the span's `attributes`. Provider metadata (`ai.response.providerMetadata`), request headers
  (`ai.request.headers.*`), response ids and timestamps are not mapped and stay there too.
- AI SDK 7's telemetry options have no `metadata`, so `LegacyOpenTelemetry` sets no user, session or trace tags.
- With `new OpenTelemetry()`, `functionId` becomes `gen_ai.agent.name`, which Crucible does not map: the trace has
  no name, and its root span is named `invoke_agent <model>`. Step spans (`gen_ai.operation.name` `agent_step`)
  are typed `span`.
