# Braintrust OTel attributes

Spans that carry Braintrust's OpenTelemetry attributes (`braintrust.input_json`, `braintrust.metrics`,
`braintrust.span_attributes` and the rest) are sent to `/otel/v1/traces`, from a plain OTLP exporter or from a
Braintrust SDK's OpenTelemetry exporter. Crucible reads their span types, inputs and outputs, metadata, token
metrics, models, levels and tags. The Braintrust features that call its REST API are not served.

Verified with braintrust 0.52.0 (Python, `BraintrustSpanProcessor`), braintrust-sdk-go 0.20.0, and the OpenTelemetry
Python OTLP exporter 1.45.2.

## Pointing it at Crucible

From an OTLP exporter, set the base endpoint to Crucible's `/otel` path; the exporter appends `/v1/traces`:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318/otel
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer%20csk_...,x-bt-parent=project_name:my-project"
```

`x-bt-parent` can stay or go: Crucible reads it only as a hint about the spans' attributes, and the project comes
from the key.

With the Python SDK, add its span processor to your tracer provider and point it at Crucible without a path:

```sh
export BRAINTRUST_API_URL=http://127.0.0.1:4318
export BRAINTRUST_API_KEY=csk_...
export BRAINTRUST_PARENT=project_name:my-project
```

```python
from braintrust.otel import BraintrustSpanProcessor
from opentelemetry.sdk.trace import TracerProvider

provider = TracerProvider()
provider.add_span_processor(BraintrustSpanProcessor())
```

It sends to `/otel/v1/traces` with the key as `Authorization: Bearer <key>`, and in the verification run it made no
other connection.

With the Go SDK, set the app URL to Crucible as well as the API URL:

```sh
export BRAINTRUST_API_URL=http://127.0.0.1:4318
export BRAINTRUST_APP_URL=http://127.0.0.1:4318
export BRAINTRUST_API_KEY=csk_...
```

`braintrust.New` logs in to the app URL when it starts, sending the API key, and the app URL defaults to
`https://www.braintrust.dev`. With only `BRAINTRUST_API_URL` changed, the SDK sends your Crucible secret key there.
With both set, the login answers 404, the SDK logs `login failed` as a warning, and its spans export to Crucible.

## What maps

| Crucible field | Braintrust attribute |
|---|---|
| Observation type | `braintrust.span_attributes.type`, or the `type` member of `braintrust.span_attributes`: `llm` to generation, `tool` and `function_call` to tool, `agent` to agent, `score` and `classifier` to evaluator, `eval` to chain, `task` and `function` to span, `log` to event; `purpose` `scorer` to evaluator |
| Input, output | `braintrust.input_json`, `braintrust.input`, `braintrust.input.*`; `braintrust.output_json`, `braintrust.output`, `braintrust.output.*` |
| Metadata | `braintrust.metadata`, a JSON object, and `braintrust.metadata.*` |
| Requested model, provider | `gen_ai.request.model` or the metadata's `model`; the metadata's `provider` |
| Model | `gen_ai.response.model` |
| Tool definitions, prompt version | The metadata's `tools`, and its `prompt.version` |
| Token usage | `braintrust.metrics` or `braintrust.metrics.*`: `prompt_tokens` to input, `completion_tokens` to output, `tokens` to total, `prompt_cached_tokens` to cache reads, `prompt_cache_creation_tokens` to cache writes, `completion_reasoning_tokens` to reasoning |
| Completion start | The metrics' `time_to_first_token`, seconds after the span's start |
| Level | The `log_level` of `braintrust.span_attributes` |
| Span tags | `braintrust.tags` |
| Finish reasons | `gen_ai.finish_reason`, and `stop_reason` on spans decided as Braintrust |

Without a cost on the span, Crucible prices the token counts with the bundled price table. The trace list shows the
tokens and cost; the requested model, provider, span tags and completion start are columns of `crucible_sql.spans`,
and metadata is in `crucible_sql.span_payloads`.

## Known limits

- `braintrust.scores` is not turned into Crucible scores and stays among the span's `attributes`, as do
  `braintrust.expected`, `braintrust.expected_json`, `braintrust.parent`, `braintrust.context_json` and
  `braintrust.span_attributes`. Use Crucible's [scores](../guides/facts.md) for feedback.
- The metrics' `estimated_cost` is not read, so cost comes from the price table.
- `braintrust.tags` are tags of a span: they are in the span's `tags` column, and the trace list's tags stay empty.
- The REST `model` shows only when a span carries `gen_ai.response.model`.
- No Braintrust attribute names a user or a session.
- OTLP logs are not ingested: `/otel/v1/logs` answers 404.
