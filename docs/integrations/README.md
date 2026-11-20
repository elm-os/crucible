# Integrations

Crucible maps the spans of several instrumentation libraries as they arrive, next to the OpenTelemetry GenAI
conventions and its own `crucible.*` keys. Your code stays as it is: point the library's OTLP export at Crucible
with a project key, and its spans show in the trace views, the REST API and the SQL workspace with their types,
models, tokens, cost, inputs and outputs.

| Library | Page | Path | Verified with |
|---|---|---|---|
| OpenInference | [OpenInference](openinference.md) | `/v1/traces` | arize-phoenix-otel 0.17.3, openinference-instrumentation 0.1.81 |
| OpenLLMetry | [OpenLLMetry](openllmetry.md) | `/v1/traces` | traceloop-sdk 0.63.1 |
| Vercel AI SDK | [Vercel AI SDK](vercel-ai-sdk.md) | `/v1/traces` | ai 6.0.333; ai 7.0.177 with @ai-sdk/otel 1.0.177 |
| MLflow Tracing | [MLflow](mlflow.md) | `/v1/traces` | mlflow-skinny 3.18.0 |
| Pydantic AI and Logfire | [Pydantic AI](pydantic-ai.md) | `/v1/traces` | pydantic-ai-slim 2.71.0, logfire 6.0.0 |
| Langfuse SDKs | [Langfuse](langfuse.md) | `/api/public/otel/v1/traces` | langfuse 5.2.0 (Python); @langfuse/otel and @langfuse/tracing 6.1.1 |
| Braintrust OTel attributes | [Braintrust](braintrust.md) | `/otel/v1/traces` | braintrust 0.52.0 (Python), braintrust-sdk-go 0.20.0 |

Each library's own setup in the "Verified with" column was run against a Crucible server built from main, and the
traces were read back through the REST API and the SQL workspace. Moving from Langfuse or Braintrust, including what
the calls to their REST APIs do against Crucible, is covered in [migration](../migration/README.md).

## Endpoints and keys

- Both HTTP listeners, OTLP/HTTP on port 4318 and the main listener on 8080, serve every path in the table. They
  take the same OTLP bodies (protobuf or JSON, plain or gzip), keys, size caps and acknowledgement as `/v1/traces`
  ([sending traces](../guides/sending-traces.md)).
- `/api/public/otel/v1/traces` and `/otel/v1/traces` also tell Crucible which attributes to expect: a span sent
  there that matches no stronger rule is read as that library's. On `/v1/traces`, a request with an `x-bt-parent`
  header gets the same hint as `/otel/v1/traces`.
- Every path takes the project's secret key as `Authorization: Bearer <secret key>` or its key pair as
  `Authorization: Basic <base64 of public key:secret key>`. The key needs the `ingest` scope, and it alone decides
  the project.
- OTLP/gRPC on port 4317 takes the same spans, without the hint of a path.
- Traces are the only signal Crucible ingests: `/v1/logs`, `/v1/metrics` and `/otel/v1/logs` answer 404. A library
  that exports metrics or logs by default reports failed exports; its page says how to turn them off.
- Other paths under `/api/` and `/otel/` answer a problem 404, so a library call to a vendor's REST API fails with
  404 rather than reaching the UI.

## How a span's library is decided

Each library has detection rules over the instrumentation scope name and the attribute keys, each with a weight.
The strongest match decides which library a span is read as, ties going to the lower dialect number, and a span
that matches none is read with the OpenTelemetry GenAI conventions. One trace can hold spans of several libraries.
In the SQL workspace, `crucible_sql.spans` shows the outcome per span: `source_format` is `compat` with the
library's `compat_dialect`, or `otel` for spans read as native.

| `compat_dialect` | Library |
|---|---|
| 1 | Langfuse SDKs |
| 2 | OpenInference |
| 3 | OpenLLMetry |
| 4 | Vercel AI SDK |
| 5 | MLflow Tracing |
| 6 | Pydantic AI and Logfire |
| 7 | Braintrust OTel attributes |

Keys under a library's own prefix (such as `llm.token_count.*` or `ai.usage.*`) are read on every span. Keys with
generic names (`metadata`, `tool.name`, `provider`) are read only on spans decided as the library that uses them.
When several sources set one field, the first in this order wins: `crucible.*`, Langfuse, Braintrust, the
OpenTelemetry GenAI conventions, OpenInference, the Vercel AI SDK, OpenLLMetry, MLflow, Pydantic AI. Metadata,
trace metadata and model parameters merge every source instead, and token counts and costs are resolved key by key.

## Where the mapped fields show

| Where | Fields |
|---|---|
| Trace list, `GET traces` | Trace name, user, session, environment, release, trace tags, tokens and cost |
| Trace detail, `GET traces/{trace_id}` | For each span: observation type, level, status, model, tokens, cost and the shape of its input |
| Payloads, `GET spans/{trace_id}/{span_id}/payload` | `input`, `output`, `system_instructions`, `tool_definitions`, `tool_calls`, `attributes` (the keys no rule mapped), `resource_attributes`, `events` and `links` |
| `crucible_sql.spans` | All of the above, plus `compat_dialect`, `source_format`, `model_requested`, `provider`, `prompt_name`, `prompt_version`, span `tags`, `tool_names`, `finish_reasons`, `completion_start_time`, `status_message`, `version`, `sdk_name`, `sdk_version`, `usage_keys` and `usage_values`, and `cost_source` |
| `crucible_sql.span_payloads` | Span metadata (`metadata_kv`), `trace_metadata` and `model_parameters` |

- The REST `model` is the model that answered. A span that names only the requested model shows an empty `model` in
  REST and its `model_requested` in SQL.
- Span tags and trace tags differ: the trace list shows trace tags, and tags a library sets on a single span are
  in the span's `tags` column.
- Cache reads and writes count inside input tokens and reasoning inside output tokens. A span's own cost wins
  (`cost_source` `provided`); otherwise Crucible prices the tokens with its bundled price table
  ([price table](../architecture/price-table.md)), and a model missing from it costs 0.
- A parent span whose descendants carry tokens or cost is flagged as an aggregate (`usage_aggregate`), so a trace's
  sums count each model call once.
- Trace-level fields (name, user, session, tags, environment, release) set on a span are carried to the spans after
  it in the same export request, and the trace shows its root span's value, else the latest one.

## What no library mapping does

Crucible maps each span's own keys. It does not move token counts between spans, and it does not read tools, system
instructions or models out of a raw input or output body: a body is stored as sent.
