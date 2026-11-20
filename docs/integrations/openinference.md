# OpenInference

Spans from OpenInference instrumentation, identified by `openinference.span.kind`, are sent to `/v1/traces` like any
OTLP export. Their span kinds become Crucible's observation types, and their indexed messages, tool calls, models,
token counts, metadata, tags, sessions and users map to Crucible's fields.

Verified with arize-phoenix-otel 0.17.3 and openinference-instrumentation 0.1.81 (Python).

## Pointing it at Crucible

`phoenix.otel.register` sets up the exporter. Give it Crucible's full `/v1/traces` URL and a secret key:

```python
from phoenix.otel import register

tracer_provider = register(endpoint="http://127.0.0.1:4318/v1/traces", api_key="csk_...")
```

Or set the same two values in the environment and call `register()` without them:

```sh
export PHOENIX_COLLECTOR_ENDPOINT=http://127.0.0.1:4318/v1/traces
export PHOENIX_API_KEY=csk_...
```

Either way `register` exports over HTTP with protobuf and sends the key as `Authorization: Bearer <key>`.
OpenInference instrumentors take the `tracer_provider` that `register` returns. Its tracer also writes OpenInference
spans directly, and the context helpers set the session, user, metadata and tags of every span inside them:

```python
from openinference.instrumentation import using_metadata, using_session, using_tags, using_user

tracer = tracer_provider.get_tracer("weather")


@tracer.tool
def get_forecast(city: str) -> str:
    return "rainy"


@tracer.agent
def weather_agent(question: str) -> str:
    return f"{get_forecast('Lisbon')} in Lisbon"


with using_session("session-oi"), using_user("user-oi"), using_metadata({"region": "eu"}), using_tags(["billing"]):
    weather_agent("What is the forecast?")
tracer_provider.force_flush()
```

A plain OpenTelemetry exporter pointed at `/v1/traces` with a secret key works the same way
([sending traces](../guides/sending-traces.md)).

## What maps

| Crucible field | OpenInference attribute |
|---|---|
| Observation type | `openinference.span.kind`: `LLM` to generation, `EMBEDDING` to embedding, `CHAIN` and `PROMPT` to chain, `RETRIEVER` and `RERANKER` to retriever, `TOOL` to tool, `AGENT` to agent, `GUARDRAIL` to guardrail, `EVALUATOR` to evaluator, `UNKNOWN` to span |
| Input, output | `llm.input_messages.*`, `llm.output_messages.*` as indexed messages; otherwise `input.value`, `output.value` |
| Tool calls | The `tool_calls` of the output messages |
| Tool names | `tool.name`, and the names in the tool calls |
| Tool definitions | `llm.tools.*` |
| Model | `llm.response.model_name`, `llm.model_name`, `embedding.model_name` |
| Requested model | `llm.request.model_name` |
| Provider | `llm.provider`, `llm.system` |
| Model parameters | `llm.invocation_parameters` |
| Finish reasons | `llm.finish_reason` |
| Token usage | `llm.token_count.prompt`, `.completion`, `.total`, `.prompt_details.cache_read`, `.prompt_details.cache_write`, `.completion_details.reasoning`, and the audio counts |
| Cost | `llm.cost.total`, `llm.cost.prompt`, `llm.cost.completion` and their details |
| Metadata | `metadata`, a JSON object |
| Span tags | `tag.tags`, a list or a JSON array |
| Prompt version | `llm.prompt_template.version` |
| Session, user | `session.id`, `user.id` |
| Completion start | The time of the `First Token Stream Event` event |

Without a cost on the span, Crucible prices its token counts with the bundled price table. The trace list shows the
session, user, tokens and cost; the span tags, provider, finish reasons and tool names are columns of
`crucible_sql.spans`, and metadata and model parameters are in `crucible_sql.span_payloads`.

## Known limits

- `tag.tags` are tags of a span, not of the trace: they are in the span's `tags` column, and the trace list's tags
  stay empty.
- When a span carries both OpenTelemetry GenAI messages (`gen_ai.input.messages`) and OpenInference messages, the
  GenAI keys win and the OpenInference ones stay among the span's `attributes`. When messages are present,
  `input.value` and `output.value` stay there too.
- Prompt templates and variables (`llm.prompt_template.template`, `.variables`), retrieved documents
  (`retrieval.documents.*`), reranker keys (`reranker.*`), embedding vectors (`embedding.embeddings.*`) and the
  MIME types (`input.mime_type`, `output.mime_type`) are not mapped and stay among the `attributes`.
- `metadata` and `tool.name` are read only on spans decided as OpenInference, since other libraries use the same
  names.
