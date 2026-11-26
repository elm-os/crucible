# Pydantic AI and Logfire

Pydantic AI instruments agent runs, model requests and tool calls with the OpenTelemetry GenAI conventions, under
the instrumentation scope `pydantic-ai`. Crucible maps them as native GenAI spans and reads Pydantic AI's own keys
on top: the run's messages and final result, its reported cost, reasoning tokens, and Logfire's level. Logfire, or a
plain OpenTelemetry tracer provider, exports them to `/v1/traces`.

Verified with pydantic-ai-slim 2.71.0, with logfire 6.0.0 and with opentelemetry-sdk 1.45.2 (Python).

## Pointing it at Crucible

Point the OTLP trace exporter at Crucible with the standard variables:

```sh
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20csk_..."
```

Use the variables for traces rather than `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS`, which
apply to metrics and logs too: Crucible ingests traces only, `/v1/metrics` and `/v1/logs` answer 404, and the
headers variable would send your key to wherever metrics and logs go.

With Logfire, keep its own backend off and instrument Pydantic AI:

```python
import logfire

logfire.configure(send_to_logfire=False)
logfire.instrument_pydantic_ai()
```

Without Logfire, give Pydantic AI a tracer provider with an OTLP exporter:

```python
from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from pydantic_ai import Agent

provider = TracerProvider()
provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
trace.set_tracer_provider(provider)
Agent.instrument_all()
```

## What maps

Spans of an agent run arrive as an agent span (`invoke_agent`), a generation per model request (`chat`) and a tool
span per tool call (`execute_tool`). The OpenTelemetry GenAI keys give the models, provider, input and output
messages, system instructions, tool names, arguments and results, the tool calls a model requested, token counts,
and the session (`gen_ai.conversation.id`). Pydantic AI's own keys add:

| Crucible field | Attribute |
|---|---|
| Input of the agent span | `pydantic_ai.all_messages`, the run's whole conversation |
| Output of the agent span | `final_result` |
| Cost | `operation.cost`, in USD |
| Reasoning tokens | `gen_ai.usage.details.reasoning_tokens`, `gen_ai.usage.details.thinking_tokens` |
| Level | `logfire.level_num`: trace and debug to debug, info and notice to default, warn to warning, error and fatal to error |
| Metadata | `metadata`, a JSON object |
| Completion start | `gen_ai.client.operation.time_to_first_chunk`, seconds after the span's start |
| Observation type | `gen_ai.operation.name` `image_generation` and `decide` to generation |

Without a cost on the span, Crucible prices the token counts with the bundled price table. The trace list shows the
session, tokens and cost; the provider, models and tool names are columns of `crucible_sql.spans`.

## Known limits

- The run span's totals (`gen_ai.aggregated_usage.*`) are not read as token usage and stay among its `attributes`.
  The run span is flagged as a usage aggregate, so the trace's tokens are the sums of its model requests.
- The agent's name (`gen_ai.agent.name`, `agent_name`) and the other `logfire.*` keys are not mapped and stay among
  the `attributes`.
