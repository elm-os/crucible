# Moving from Langfuse

The Langfuse SDKs export their spans over OTLP, and Crucible takes that export on `/api/public/otel/v1/traces`. Moving
an application is a change of base URL and keys; the tracing code stays. The SDK features that call Langfuse's REST
API instead of exporting spans (scores, prompts, datasets) are not served by Crucible.

Verified with langfuse 5.2.0 (Python) and @langfuse/otel and @langfuse/tracing 6.1.1 (JavaScript).

## What changes

1. **The base URL.** Set `LANGFUSE_BASE_URL` (`base_url` in Python, `baseUrl` in JavaScript) to Crucible's address,
   without a trailing slash or a path: `http://<host>:4318`, or port 8080.
2. **The keys.** Set `LANGFUSE_PUBLIC_KEY` and `LANGFUSE_SECRET_KEY` to a Crucible key pair, `cpk_...` and
   `csk_...`, made in **Settings**, **API keys** or with `crucible admin create-key`. The SDK sends them as Basic
   auth; the key needs the `ingest` scope, and it decides the project.
3. **REST calls.** Remove the calls listed under [what does not carry over](#what-does-not-carry-over), such as an
   `auth_check()` at startup, which raises against Crucible.

```sh
export LANGFUSE_BASE_URL=http://127.0.0.1:4318
export LANGFUSE_PUBLIC_KEY=cpk_...
export LANGFUSE_SECRET_KEY=csk_...
```

`LANGFUSE_TRACING_ENVIRONMENT` and `LANGFUSE_RELEASE` keep working. An OpenTelemetry exporter that sent spans to a
Langfuse OTLP endpoint sends them to the same path on Crucible, with the secret key as Bearer auth or the pair as
Basic auth ([sending traces](../guides/sending-traces.md#authentication)).

Only spans exported after the switch reach Crucible.

## What carries over

Everything the SDK records on spans:

- **Traces**: name, user, session, tags, metadata, public flag, environment, release and version, including the
  attributes `propagate_attributes` writes onto every span in its scope.
- **Observations**: all ten types (span, generation, event, embedding, agent, tool, chain, retriever, evaluator,
  guardrail), level, status message, input, output and metadata.
- **Generations**: model, model parameters, token usage, cost, completion start time, and the prompt's name and
  version.
- **Sessions and users**: Crucible's sessions and users pages are built from the spans' session and user.
- **Media**: with object storage configured, the SDK's media uploads go to Crucible's `/api/public/media`. Without
  it, set `LANGFUSE_MEDIA_UPLOAD_ENABLED=false`, or the uploads fail and the payload keeps only the media reference
  ([media](../integrations/langfuse.md#media)).

[The Langfuse integration page](../integrations/langfuse.md) maps each attribute to its Crucible field.

## What does not carry over

Calls that go to Langfuse's REST API answer 404 on Crucible. With the Python SDK:

| Call | Against Crucible |
|---|---|
| `langfuse.auth_check()` | Raises `NotFoundError` |
| `langfuse.create_score(...)`, `score_current_span`, `score_current_trace` | The SDK logs an error and drops the score; the call returns |
| `langfuse.get_prompt(name)` | Raises `NotFoundError` |
| `langfuse.get_prompt(name, fallback=...)` | Logs the 404 and returns the fallback prompt |
| Datasets, dataset runs, annotation queues, comments, and `langfuse.api.*` | Answer 404 |

- For feedback scores, use Crucible's feedback route, `POST /api/v1/projects/{project_id}/feedback`, or the trace
  page's feedback section ([tags, flags, scores and corrections](../guides/facts.md)).
- Trace-level input and output (`langfuse.trace.input`, `langfuse.trace.output`) and experiment keys
  (`langfuse.experiment.*`) are kept among each span's `attributes` rather than mapped.
