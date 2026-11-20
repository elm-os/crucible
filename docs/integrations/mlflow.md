# MLflow Tracing

MLflow exports its traces over OTLP when the standard OpenTelemetry exporter variables are set. Crucible reads the
`mlflow.*` span attributes: span types, inputs and outputs, token usage, cost, models, log levels and linked
prompts.

Verified with mlflow-skinny 3.18.0 and opentelemetry-exporter-otlp 1.45.2 (Python).

## Pointing it at Crucible

Set the variables before the first trace, with the HTTP protocol named:

```sh
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20csk_..."
```

MLflow's own default protocol is gRPC: without `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` it sends gRPC to port 4318
and the export fails with `StatusCode.UNIMPLEMENTED`. With the endpoint set, MLflow exports its traces over OTLP
instead of logging them to a tracking server. The exporter package must be installed beside MLflow
(`pip install opentelemetry-exporter-otlp`).

Tracing code stays as it is:

```python
import mlflow
from mlflow.entities import SpanType


@mlflow.trace(span_type=SpanType.TOOL)
def get_forecast(zip_code: str) -> str:
    return "rainy"


@mlflow.trace(span_type=SpanType.CHAIN)
def answer(question: str) -> str:
    with mlflow.start_span(name="chat", span_type=SpanType.CHAT_MODEL) as span:
        span.set_inputs({"messages": [{"role": "user", "content": question}]})
        span.set_attribute("mlflow.chat.tokenUsage", {"input_tokens": 80, "output_tokens": 25, "total_tokens": 105})
        span.set_outputs({"choices": [{"message": {"role": "assistant", "content": "rainy"}}]})
    return get_forecast("1000")


answer("forecast for Lisbon?")
```

## What maps

| Crucible field | MLflow attribute |
|---|---|
| Observation type | `mlflow.spanType`: `LLM` and `CHAT_MODEL` to generation, `EMBEDDING` to embedding, `TOOL` to tool, `AGENT` to agent, `CHAIN` and `WORKFLOW` to chain, `RETRIEVER` and `RERANKER` to retriever, `GUARDRAIL` to guardrail, `EVALUATOR` to evaluator, `TASK`, `MEMORY`, `PARSER` and `UNKNOWN` to span |
| Input, output | `mlflow.spanInputs`, `mlflow.spanOutputs`, as sent |
| Token usage | `mlflow.chat.tokenUsage`: `input_tokens`, `output_tokens`, `total_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` |
| Cost | `mlflow.llm.cost`: `input_cost`, `output_cost`, `total_cost` |
| Requested model, provider | `mlflow.llm.model`, `mlflow.llm.provider` |
| Tool definitions | `mlflow.chat.tools` |
| Level | `mlflow.spanLogLevel`: 10 to debug, 20 to default, 30 to warning, 40 and 50 to error |
| Prompt name and version | The first entry of `mlflow.linkedPrompts` |
| Trace metadata | `mlflow.traceTag.*` |
| Completion start | The `mlflow.chunk.item.0` event |

MLflow stores each attribute as JSON text. On MLflow spans Crucible reads a JSON string or number as its value
(`"\"CHAIN\""` is `CHAIN`), and keeps inputs and outputs as sent, so an output of `"rainy"` keeps its quotes. MLflow
gives a span without a log level the default for its type, and Crucible shows it: a `CHAIN` span arrives as `debug`.

## Known limits

- MLflow names the requested model only, so the REST `model` is empty on MLflow spans; the model is in the
  `model_requested` column of `crucible_sql.spans`, and Crucible prices tokens by it when the span carries no cost.
- No MLflow attribute names a user or a session. A `session.id` or `user.id` attribute that your code sets on a span
  is read, its JSON quotes removed.
- `mlflow.traceRequestId`, `mlflow.spanFunctionName` and `mlflow.message.format` are not mapped and stay among the
  span's `attributes`. Tool calls inside raw input or output bodies are not extracted.
- With MLflow 3.18.0 exporting over OTLP, after `mlflow.update_current_trace(...)` inside a traced function the root
  span of that trace is not exported: MLflow logs `Failed to end span <id>: Cannot mutate immutable
  BoundedAttributes`.
