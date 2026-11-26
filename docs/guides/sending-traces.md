# Sending traces

Crucible takes traces in OTLP, the OpenTelemetry protocol: protobuf or JSON over HTTP, and protobuf over gRPC. Any
OpenTelemetry SDK, and the OpenTelemetry Collector, can send them without Crucible code. The Crucible SDKs for
[TypeScript](sdk-typescript.md), [Python](sdk-python.md), [Go](sdk-go.md) and [Rust](../../sdk/rust/README.md) send
the same OTLP and add helpers for agents and MCP servers. Crucible ingests traces only; it does not take logs or
metrics.

## Endpoints

| Protocol | Default address | Path or method | Setting (flag) |
|---|---|---|---|
| OTLP/HTTP | `0.0.0.0:4318` | `POST /v1/traces` | `server.ingest_listen` (`--ingest-listen`) |
| OTLP/HTTP | `0.0.0.0:8080`, the main listener, which also serves the API and the UI | `POST /v1/traces` | `server.listen` (`--listen`) |
| OTLP/gRPC | `0.0.0.0:4317` | `opentelemetry.proto.collector.trace.v1.TraceService/Export` | `server.grpc_listen` (`--grpc-listen`) |

- HTTP bodies are `application/x-protobuf` or `application/json`, plain or with `Content-Encoding: gzip`. The
  response comes in the request's format. Any other media type or content coding answers 415.
- The gRPC listener speaks HTTP/2 without TLS and accepts gzip-compressed messages. A request on it that is not
  gRPC answers 415.
- The server speaks plain HTTP only. On a network you do not trust, put a TLS-terminating proxy in front of it.
- Every other path on the OTLP/HTTP listener, `/v1/logs` and `/v1/metrics` included, answers a problem 404.
- Both HTTP listeners also serve compatibility routes that take other tracing SDKs' OTLP exports, with the same
  bodies, keys, caps and acknowledgement ([integrations](../integrations/README.md)).
- `deploy/compose/compose.yaml` publishes ports 8080 and 4318, not 4317. To send gRPC from outside the Compose
  network, add `- "${CRUCIBLE_BIND_ADDRESS:-0.0.0.0}:4317:4317"` to the `ports` of the `crucible` service.

## Authentication

Every export carries a project key in `Authorization`, in one of two forms:

| Form | Header |
|---|---|
| Secret key | `Authorization: Bearer <secret key>` |
| Key pair | `Authorization: Basic <base64 of public key:secret key>`, which `curl -u <public key>:<secret key>` sends |

A secret key is `csk_` followed by 43 characters from `0-9A-Za-z`; a public key is `cpk_` in the same form. The
key decides the project: no header, path or attribute selects one. With Postgres, as in the
[quickstart](../self-hosting/quickstart.md), keys are made in Settings or with `crucible admin create-key`, and a key
sends traces with the `ingest` scope, which new keys have. Without Postgres the server reads its keys from the TOML
file named by `server.keys_file` (`--keys-file`):

```toml
[[project_key]]
project_id = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
public_key = "cpk_..."
secret = "csk_..."
```

[Without Postgres](../self-hosting/README.md#without-postgres-keys-from-a-file) shows how to make a key pair. Without
Postgres or a keys file every export answers 401. A key in the file has the scopes of a new key (`ingest`, `read` and
`write`), so it can both send traces and read them through the [REST API](../api/README.md).

A refused credential answers 401 with `WWW-Authenticate: Bearer, Basic realm="crucible"` and a message that says
how to fix the header, in the OTLP status body; gRPC answers `UNAUTHENTICATED` with the same message. No refusal
echoes a key. A public key on its own is refused, and so is a public key sent with the secret key of another
project.

```console
$ curl -s -X POST http://127.0.0.1:4318/v1/traces -H 'Content-Type: application/json' -d '{}'
{"code":16,"message":"send the project's secret key as `Authorization: Bearer <secret key>`, or its public and secret key pair as `Authorization: Basic base64(<public key>:<secret key>)`"}
```

## From an OpenTelemetry SDK

OpenTelemetry SDKs read their exporter settings from standard environment variables. Set the ones for traces, which
have `TRACES` in their names. Over HTTP:

```sh
export OTEL_SERVICE_NAME=support-agent
export OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://crucible.internal:4318/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20$CRUCIBLE_SECRET_KEY"
```

Over gRPC:

```sh
export OTEL_SERVICE_NAME=support-agent
export OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=grpc
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://crucible.internal:4317
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20$CRUCIBLE_SECRET_KEY"
```

Crucible takes traces only. Keep metrics and logs on their current exporter, or turn them off when Crucible is your
only backend:

```sh
export OTEL_METRICS_EXPORTER=none
export OTEL_LOGS_EXPORTER=none
```

- The same variables without `TRACES` in their names, such as `OTEL_EXPORTER_OTLP_ENDPOINT` and
  `OTEL_EXPORTER_OTLP_HEADERS`, apply to metrics and logs too. They would send metrics and logs to Crucible, which
  refuses them, or send your Crucible key to the backend that receives your metrics and logs.
- `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is used as it is over HTTP, so it ends in `/v1/traces`. Over gRPC it takes no
  path.
- `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` picks the exporter, and SDKs differ in their default: the Node.js SDK sends
  `http/protobuf`, and Python's automatic setup (`opentelemetry-instrument`) sends gRPC. So both blocks set it.
- `%20` is the space after `Bearer`: SDKs percent-decode the headers variable. The Node.js and Python SDKs also
  accept a literal space.
- The `http://` scheme tells a gRPC exporter to connect without TLS.

Both blocks were checked against a running server with the OpenTelemetry SDKs for Node.js (`@opentelemetry/sdk-node`
0.301) and Python (1.45), the Python one through `opentelemetry-instrument` and through its HTTP and gRPC exporters.

## From the OpenTelemetry Collector

A Collector in front of Crucible receives from your services and forwards to Crucible. With the HTTP exporter:

```yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:4318

exporters:
  otlp_http/crucible:
    endpoint: http://crucible.internal:4318
    headers:
      Authorization: "Bearer ${env:CRUCIBLE_SECRET_KEY}"

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlp_http/crucible]
```

Or with the gRPC exporter:

```yaml
exporters:
  otlp_grpc/crucible:
    endpoint: crucible.internal:4317
    tls:
      insecure: true
    headers:
      Authorization: "Bearer ${env:CRUCIBLE_SECRET_KEY}"
```

These were checked with Collector 0.165.0, which names the exporters `otlp_http` and `otlp_grpc`. Older releases
call them `otlphttp` and `otlp`; 0.165.0 still accepts those names and logs that they are deprecated.

## By hand

One span as OTLP/JSON with curl:

```sh
NOW=$(date +%s)
curl -s -X POST http://127.0.0.1:4318/v1/traces \
  -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  -d '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"curl-demo"}}]},
       "scopeSpans":[{"spans":[{"traceId":"5b8efff798038103d269b633813fc60c","spanId":"eee19b7ec3c1b174",
       "name":"hello crucible","kind":1,"startTimeUnixNano":"'$NOW'000000000","endTimeUnixNano":"'$NOW'250000000"}]}]}]}'
```

The answer is `{}`: every span was accepted.

## What an acknowledgement means

The server answers 200 (gRPC `OK`) only after the request is durable in its write-ahead log on disk. It then
copies the spans to ClickHouse in the background; a server that stops after the acknowledgement ships them when it
starts again. Reads also consult an in-memory index of recent spans, so a trace can be read back through the API as
soon as its request is acknowledged. In one local run the export and a read of its trace took 47 ms together.

| Answer (HTTP; gRPC) | When | What to do |
|---|---|---|
| 200 with an empty body, `{}` in JSON; `OK` | Every span was accepted | Nothing |
| 200 with `partialSuccess`; `OK` with partial success | Some spans were rejected, the rest accepted. A span is rejected when its trace or span id is missing or all zero, when it started more than 400 days ago, or when it started before its project's retention cut-off (`retention_days`) | Nothing: a retry would be rejected the same way |
| 400; `INVALID_ARGUMENT` | The body is not valid OTLP | Fix the sender |
| 401; `UNAUTHENTICATED` | No usable key | Fix the key |
| 413; `INVALID_ARGUMENT` | The body is above 67,043,328 bytes, measured after gzip is inflated, or it holds more than 50,000 spans | Send smaller batches |
| 415 (HTTP only) | Another media type or content coding | Send protobuf or JSON, plain or gzip |
| 429 with `Retry-After` of 1 to 5 s; `RESOURCE_EXHAUSTED` with `RetryInfo` | The server holds as many request bytes in flight as its budget allows | Retry after the delay |
| 503 with `Retry-After` of 5 to 30 s; `UNAVAILABLE` with `RetryInfo` | The write-ahead log is above its high-water mark, the disk is nearly full, or a disk sync failed | Retry after the delay |
| 503 with `Retry-After: 5`; `UNAVAILABLE` with `RetryInfo` | The server is still starting; `GET /readyz` says what it waits for | Retry after the delay |

The OTLP specification treats 429 and 503 as retryable and asks exporters to wait for the delay the server names.
A partial success looks like this:

```json
{"partialSuccess":{"errorMessage":"rejected 2 of 3 spans: 1 with a missing or all-zero span_id, 1 older than the accepted event age","rejectedSpans":"2"}}
```

**Retries and duplicates.** A retry after a lost answer can deliver a span twice. The server drops a repeat that
is identical to a span it still remembers, so two identical exports leave one stored row. A repeat whose content
differs, for example with another attribute, is stored as well, and the trace view shows the newest copy of each
span id. OTLP ingest does not read an `Idempotency-Key` header.

**Batching.** Exporters batch spans before they send them; the OpenTelemetry batch span processor sends at most 512
spans per request by default, far below the per-request caps above.

## Size caps

Large values are cut, never refused. Each payload field of a span (`input`, `output`, `system_instructions`,
`metadata`, `attributes`, `resource_attributes`, `tool_definitions`, `tool_calls` and the rest) keeps at most
8 MiB, and a span keeps at most 16 MiB over all of its payload fields. A span above the per-span cap is cut further
in a fixed order, `events`, `links`, `resource_attributes`, `attributes`, `tool_definitions`, `tool_calls`,
`trace_metadata`, `metadata`, `system_instructions`, `output` and last `input`, until it fits.

- A field is cut at a UTF-8 character boundary. A cut JSON field is stored as the text prefix, which is no longer
  valid JSON, and the span lists the field in its `truncated_fields`.
- For `input` and `output` the original size and the SHA-256 of the original bytes are kept, and the 1,024-byte
  preview is taken from the original value.
- Short fields have their own caps, for example 1,024 bytes for a span name, 8 KiB for a status message, 512 bytes
  for a session or user id and 64 tags of 256 bytes each. [Contract 3.1](../architecture/contracts.md#31-spanrecord)
  lists every cap.
- MCP arguments and results are captured only when an SDK or the proxy is told to capture them, and each cuts
  them at 16 KiB by default before it sends them. On the server they become the tool span's `input` and `output`, under the
  caps above, and the arguments and result of its MCP operation, cut at 8 MiB each
  ([contract 3.5](../architecture/contracts.md#35-mcpoprecord)).

In one local run a span with a 9 MiB `crucible.input` was acknowledged with 200 and stored with its first
8,388,608 bytes, its original size of 9,437,184 bytes and `input` in `truncated_fields`.

The caps are fixed; no setting changes them.

The caps per request are separate: a body above 67,043,328 bytes, or with more than 50,000 spans, is refused whole
with 413, and single spans are rejected through partial success as described above.

## Attributes the trace views read

The traces table, the trace page, the payload viewer, search and the REST API read these attributes, except
`service.name`, which only the SQL workspace shows. Where a row lists several keys, the first one present wins.
`crucible.*` keys come first, and each falls back to an OpenTelemetry semantic-convention key where one exists, so
an application instrumented with plain OpenTelemetry fills most fields with no change. Resource attributes are
marked; every other key is a span attribute.

| Shown as (API field) | Keys | Notes |
|---|---|---|
| Trace name (`trace_name` in the trace list) | `crucible.trace.name` | The root span's value wins, else the latest. Without one, the UI shows the name of the trace's head span: the root span, or the earliest span while the root has not arrived |
| Observation type (`observation_type`) | `crucible.observation.type`; else `mcp.method.name`; else `gen_ai.operation.name` | Values: `span`, `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`, `evaluator`, `guardrail`. An MCP `tools/call` span is `tool`. `gen_ai.operation.name` maps `chat`, `text_completion` and `generate_content` to `generation`, `embeddings` to `embedding`, `execute_tool` to `tool`, and `create_agent` and `invoke_agent` to `agent`. Its other values, `retrieval` and `invoke_workflow` among them, give `span`; set `crucible.observation.type` to `retriever` or `chain` to type such spans. Default `span`. The agent graph draws the `agent`, `tool` and `generation` spans |
| Status (`status_code`) | The span status | `unset`, `ok` or `error` |
| Level (`level`) | `crucible.level` | `debug`, `default`, `warning` or `error`. Without it, a span whose status is error has level `error`, every other span `default`. The trace list's `error_count` counts spans of level `error` |
| Model (`model`) | `gen_ai.response.model` | Only the model the response reports is shown. `gen_ai.request.model` prices a span that has no response model, but it is not shown: it is the `model_requested` column of `crucible_sql.spans` in the [SQL workspace](sql-workspace.md) |
| Tokens (`input_tokens`, `output_tokens`, `total_tokens`) | `gen_ai.usage.input_tokens`, else the deprecated `gen_ai.usage.prompt_tokens`; `gen_ai.usage.output_tokens`, else the deprecated `gen_ai.usage.completion_tokens` | The total is input plus output. `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_creation.input_tokens` and `gen_ai.usage.reasoning.output_tokens` are parts of the input and output counts, not additions. The trace list's totals leave out a span that has a descendant carrying usage or cost, because it repeats what its descendants report; every span still shows its own counts |
| Cost (`total_cost_usd`) | `crucible.cost.total`, `crucible.cost.<usage key>` | USD as a decimal string or a double. A provided cost wins, so `crucible.cost.total` prices a model the price table does not list. Otherwise the server prices the token usage with its bundled price table, by the response model, else the request model; [priced models](priced-models.md) lists the models it prices. A model the table does not list gets no cost: the traces and spans lists show $0, the REST API returns 0, and `cost_source` in `crucible_sql.spans` is `none` |
| Session and user (`session_id`, `user_id` in the trace list) | `crucible.session.id`, `session.id`, `gen_ai.conversation.id`; `crucible.user.id`, `user.id` | Set them on every span at span start. Within one request, a span without a value takes the value of the request's root span for its trace, else of the earliest span that has one |
| Service (no view or API field) | Resource `service.name` | Stored as the `service_name` column of `crucible_sql.spans`, which the SQL workspace reads. The UI, the REST API and the MCP server do not show it, and it is not repeated among the resource attributes |
| Environment (`environment` in the trace list) | `crucible.environment`, resource `deployment.environment.name`, resource `deployment.environment` | Default `default` |
| Release (`release` in the trace list) | `crucible.release`, resource `service.version` | |
| Trace tags (`tags` in the trace list) | `crucible.trace.tags`, a string array | |
| Input and output (the payload route's `input` and `output`, search) | `crucible.input`, `gen_ai.input.messages`, `gen_ai.tool.call.arguments`; `crucible.output`, `gen_ai.output.messages`, `gen_ai.tool.call.result` | A `crucible.*` string is kept as is, other JSON as JSON (`io_shape: raw`). The messages are JSON (`io_shape: genai_messages`). A span that sends both a `crucible.*` key and messages keeps the `crucible.*` value, and the messages stay among its attributes. A tool span's captured MCP arguments and results become its input and output |
| System instructions (the payload route's `system_instructions`) | `gen_ai.system_instructions` | JSON |
| Tool definitions (the payload route's `tool_definitions`) | `gen_ai.tool.definitions` | JSON |
| Metadata (search's `metadata` field) | `crucible.metadata.<key>` | Each key becomes one entry of the span's metadata, searchable as `key=value`. Search and the Metadata filter of the traces and spans lists match it; the span panel does not show it, and `metadata_kv` in `crucible_sql.span_payloads` holds it |
| Attributes (the payload route's `attributes` and `resource_attributes`) | Every other key | Keys that a field above consumed are not repeated there |

The trace list's `release` and `tags` fill in once the spans reach ClickHouse; until then they are empty.

The server also reads and stores `crucible.tags` (span tags), `crucible.version`, `crucible.trace.metadata.<key>`,
`crucible.trace.public`, `crucible.prompt.name`, `crucible.prompt.version`, `crucible.completion_start_time`,
`crucible.model_parameters.<key>`, the `gen_ai.request.*` parameters, `gen_ai.provider.name` (or the deprecated
`gen_ai.system`), `gen_ai.tool.name` and `gen_ai.response.finish_reasons`. The SQL workspace's
`crucible_sql.spans` and `crucible_sql.span_payloads` views and `crucible-jsonl` exports carry them. The full list
of keys is in [contract 10.6](../architecture/contracts.md#106-source-keys-for-fields-without-a-semantic-convention).

The trace page and the REST API's span rows do not show these. In `crucible_sql.spans` they are the columns `tags`,
`version`, `trace_public`, `prompt_name`, `prompt_version`, `completion_start_time`, `provider`, `tool_names` and
`finish_reasons`; in `crucible_sql.span_payloads` they are `trace_metadata` and `model_parameters`, which holds the
request parameters and the `crucible.model_parameters.<key>` values. The traces and spans lists can filter on the
version. `crucible.trace.public` does not set the trace's Public flag.

Spans that carry `mcp.method.name`, which the Crucible SDKs and `crucible proxy` send with the keys of
[contract 9.1](../architecture/contracts.md#91-wire-attribute-contract), are stored as spans like any other: a
`tools/call` span is typed `tool`, a tool error that a Crucible SDK reports has status `error`, and the MCP keys stay
among the span's `attributes`, while captured arguments and results become the span's `input` and `output`. Each
such span is also recorded as an MCP operation, on the client or the server side, with its method, target, outcome,
error class, client, session and sizes; the SQL workspace's `crucible_sql.mcp_operations` and
`crucible_sql.mcp_contents` views read them.

Spans written with other instrumentation libraries' conventions are mapped too; this page covers the native keys,
and [integrations](../integrations/README.md) covers each library: how to point it at Crucible, what maps and the
known limits.
