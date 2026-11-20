# OpenLLMetry

The Traceloop SDK and the OpenLLMetry instrumentations export OTLP to `/v1/traces`. Crucible reads their span kinds,
workflow names, association properties, prompts and token counts. Current instrumentations of the major model
providers also write OpenTelemetry GenAI keys (`gen_ai.input.messages`, `gen_ai.usage.*`), which map as native
fields on the same spans.

Verified with traceloop-sdk 0.63.1 (Python).

## Pointing it at Crucible

Give `Traceloop.init` Crucible's address, without a path, and a secret key:

```python
from traceloop.sdk import Traceloop

Traceloop.init(app_name="my-app", api_endpoint="http://127.0.0.1:4318", api_key="csk_...")
```

Or set them in the environment and call `Traceloop.init(app_name="my-app")`:

```sh
export TRACELOOP_BASE_URL=http://127.0.0.1:4318
export TRACELOOP_API_KEY=csk_...
export TRACELOOP_METRICS_ENABLED=false
```

The SDK appends `/v1/traces` and sends the key as `Authorization: Bearer <key>`. Set `TRACELOOP_METRICS_ENABLED`
to `false`: Crucible ingests traces only, and `/v1/metrics` answers 404.

The decorators and association properties work as usual:

```python
from traceloop.sdk.decorators import task, workflow


@task(name="lookup")
def lookup(ticket: int) -> str:
    return f"ticket {ticket}"


@workflow(name="nightly_workflow")
def nightly(ticket: int) -> str:
    Traceloop.set_association_properties({"user_id": "user-42", "session_id": "session-7", "team": "search"})
    return lookup(ticket)


nightly(7)
```

## What maps

| Crucible field | OpenLLMetry attribute |
|---|---|
| Observation type | `traceloop.span.kind`: `workflow` and `task` to chain, `agent` to agent, `tool` to tool, `unknown` to span; `llm.request.type`: `chat` and `completion` to generation, `embedding` to embedding, `rerank` to retriever |
| Input, output | `gen_ai.prompt.*`, `gen_ai.completion.*` as indexed messages; `traceloop.entity.input`, `traceloop.entity.output`; `gen_ai.task.input`, `gen_ai.task.output` |
| Trace name | `traceloop.workflow.name` |
| User, session | `traceloop.association.properties.user_id`, `traceloop.association.properties.session_id` |
| Metadata | Every `traceloop.association.properties.*` key |
| Prompt name and version | `traceloop.prompt.key`, `traceloop.prompt.version` |
| Tool definitions | `llm.request.functions.*` |
| Finish reasons | `llm.response.stop_reason`, and the `finish_reason` of indexed completions |
| Token usage | `gen_ai.usage.total_tokens`, `llm.usage.total_tokens`, `gen_ai.usage.reasoning_tokens`, `gen_ai.usage.cache_read_input_tokens`, `gen_ai.usage.cache_creation_input_tokens`, `gen_ai.usage.prompt_tokens`, `gen_ai.usage.completion_tokens` |
| Model parameters | `llm.frequency_penalty`, `llm.presence_penalty`, `llm.top_k`, `llm.chat.stop_sequences`, `llm.request.repetition_penalty`, `llm.request.reasoning_effort`, and `llm.is_streaming` as `stream` |
| Completion start | The first `gen_ai.content.completion.chunk` or `llm.content.completion.chunk` event |

Models, providers and input and output token counts come from the OpenTelemetry GenAI keys the instrumentations
write (`gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.system`, `gen_ai.usage.input_tokens`). The trace list
shows the workflow name, user, session, tokens and cost; metadata is in `crucible_sql.span_payloads`.

## Known limits

- Association properties become metadata of each span, not trace metadata.
- `traceloop.entity.name`, `traceloop.entity.path` and `traceloop.entity.version` are not mapped and stay among the
  span's `attributes`, as do prompt templates (`traceloop.prompt.template`, `traceloop.prompt.template_variables`)
  and the agent name an `@agent` decorator sets (`gen_ai.agent.name`).
