# Moving from Langfuse

The Langfuse SDKs export their spans over OTLP, and Crucible takes that export on `/api/public/otel/v1/traces`. Moving
an application is a change of base URL and keys; the tracing code stays. The SDK features that call Langfuse's REST
API instead of exporting spans (scores, prompts, datasets) are not served by Crucible.

Verified with langfuse 5.2.0 (Python) and @langfuse/otel and @langfuse/tracing 6.1.1 (JavaScript).

## What changes

The Python SDK has to export over OTLP and read the base URL variable:

| Python SDK | Against Crucible |
|---|---|
| 2.x, checked with 2.60.10 | Sends to an ingestion API that Crucible does not serve. Crucible answers 404, the SDK logs that only at debug level and reports a successful flush, and the traces are lost. Upgrade first |
| 3.0.0 to 3.8.0, checked with 3.0.0 and 3.8.0 | Exports over OTLP, but reads `LANGFUSE_HOST`, not `LANGFUSE_BASE_URL`: set `LANGFUSE_HOST` to the address of step 1. The [integration page's example](../integrations/langfuse.md#pointing-it-at-crucible) does not run, as these versions have no `propagate_attributes` |
| 3.8.1 to 3.10.x, checked with 3.8.1, 3.9.0 and 3.10.7 | Works with `LANGFUSE_BASE_URL`. The [integration page's example](../integrations/langfuse.md#pointing-it-at-crucible) calls `propagate_attributes(trace_name=...)`, which these versions lack |
| 3.11.0 to 5.2.0, checked with 3.11.0, 3.15.0, 4.18.0 and 5.2.0 | Works as shown |

1. **The base URL.** Set `LANGFUSE_BASE_URL` (`base_url` in Python, `baseUrl` in JavaScript) to Crucible's address,
   without a trailing slash or a path: `http://<host>:4318`, or port 8080.
2. **The keys.** Set `LANGFUSE_PUBLIC_KEY` and `LANGFUSE_SECRET_KEY` to a Crucible key pair, `cpk_...` and
   `csk_...`, made in **Settings**, **API keys** or with `crucible admin create-key`. The SDK sends them as Basic
   auth; the key needs the `ingest` scope, and it decides the project.
3. **REST calls.** Remove the calls listed under [what does not carry over](#what-does-not-carry-over), such as an
   `auth_check()` at startup, which raises against Crucible.
4. **Media.** With no object storage, set `LANGFUSE_MEDIA_UPLOAD_ENABLED=false` where the SDK runs
   ([media](../integrations/langfuse.md#media)).

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

- **Traces**: name, user, session, tags, metadata, environment, release and version, including the attributes
  `propagate_attributes` writes onto every span in its scope. The public flag is stored as `trace_public`, which the
  SQL workspace reads; it does not set the trace's Public flag in Crucible.
- **Observations**: all ten types (span, generation, event, embedding, agent, tool, chain, retriever, evaluator,
  guardrail), level, status message, input, output and metadata.
- **Generations**: model, model parameters, token usage, cost, completion start time, and the prompt's name and
  version, except for fallback prompts ([what does not carry over](#what-does-not-carry-over)).
- **Sessions and users**: Crucible's sessions and users pages are built from the spans' session and user.
- **Media**: with object storage configured, the SDK's media uploads go to Crucible's `/api/public/media`. Without
  it, set `LANGFUSE_MEDIA_UPLOAD_ENABLED=false`, or the uploads fail and the payload keeps only the media reference
  ([media](../integrations/langfuse.md#media)).

[The Langfuse integration page](../integrations/langfuse.md) maps each attribute to its Crucible field.

## What does not carry over

Calls that go to Langfuse's REST API answer 404 on Crucible. With the Python SDK, checked with langfuse 5.2.0:

| Call | Against Crucible |
|---|---|
| `langfuse.auth_check()` | Raises `NotFoundError` |
| `langfuse.create_score(...)`, `score_current_span`, `score_current_trace` | The call returns and the SDK drops the score. For `create_score`, the only log line, at flush, is `Internal error occurred. This is an unusual occurrence and we are monitoring it closely. For help, please contact support: https://langfuse.com/support.`, with no status or path |
| `langfuse.get_prompt(name)` | Raises `NotFoundError` |
| `langfuse.get_prompt(name, fallback=...)` | Logs the 404 and returns the fallback prompt. Each call retries the 404 first, which took 0.3 to 1.7 s per call with langfuse 5.2.0, and the fallback is not cached, so an app that fetches its prompt for each request waits on every request. Move prompts into code, or pass `max_retries=0`, which returns the fallback at once, before switching |
| `langfuse.get_trace_url(trace_id=...)` | Raises `NotFoundError`. A trace's page in Crucible is `<server>/trace/?project=<project id>&trace=<trace id>` |
| Datasets, dataset runs, annotation queues, comments, and `langfuse.api.*` | Answer 404 |

- Against Crucible, `get_prompt` returns only fallback prompts, and the SDK links no fallback prompt to a
  generation, whether it is passed as `prompt=` or to `propagate_attributes`. The prompt's name and version then
  arrive empty. To keep them, pass them as a mapping,
  `propagate_attributes(prompt={"name": "polish-answer", "version": 3})`, which writes them on the current span and
  on every span started inside it (checked with langfuse 5.2.0).
- For feedback scores, use Crucible's feedback route, `POST /api/v1/projects/{project_id}/feedback`, or the trace
  page's feedback section ([tags, flags, scores and corrections](../guides/facts.md)).
- Trace-level input and output (`langfuse.trace.input`, `langfuse.trace.output`) and experiment keys
  (`langfuse.experiment.*`) are kept among each span's `attributes` rather than mapped.
