# Langfuse SDKs

Crucible takes the OTLP export of the Langfuse SDKs on `/api/public/otel/v1/traces`, with a Crucible key pair in
place of the Langfuse keys. Observation types, levels, models, token usage, cost, prompts and the trace attributes
map to Crucible's fields. The SDK calls that go to Langfuse's REST API (scores, prompts, datasets) are not served;
[moving from Langfuse](../migration/langfuse.md) lists each one and what it does against Crucible.

Verified with langfuse 5.2.0 (Python), and @langfuse/otel and @langfuse/tracing 6.1.1 with @opentelemetry/sdk-node
0.208.0 (JavaScript).

## Pointing it at Crucible

Set the SDK's base URL to Crucible, without a trailing slash or a path, and its keys to a Crucible key pair (made in
**Settings**, **API keys**, or with `crucible admin create-key`):

```sh
export LANGFUSE_BASE_URL=http://127.0.0.1:4318
export LANGFUSE_PUBLIC_KEY=cpk_...
export LANGFUSE_SECRET_KEY=csk_...
```

The SDK appends `/api/public/otel/v1/traces` and sends the pair as Basic auth; Crucible checks that the two keys
belong together. Port 8080 works as well as 4318. `LANGFUSE_TRACING_ENVIRONMENT` and `LANGFUSE_RELEASE` keep working
and become the trace's environment and release.

In Python, nothing else changes:

```python
from langfuse import get_client, propagate_attributes

langfuse = get_client()
with langfuse.start_as_current_observation(as_type="span", name="handle-request", input={"question": "ping"}) as root:
    with propagate_attributes(user_id="user-17", session_id="session-4", tags=["blue"], trace_name="nightly-eval"):
        with langfuse.start_as_current_observation(
            as_type="generation", name="my-generation", model="atlas-large",
            usage_details={"input": 210, "output": 34}, cost_details={"input": 0.0003, "output": 0.0011},
        ) as generation:
            generation.update(output={"role": "assistant", "content": "pong"})
    root.update(output={"answer": "pong"})
langfuse.flush()
```

In JavaScript, register the Langfuse span processor with the OpenTelemetry Node SDK as usual; it reads the same
variables:

```js
import { NodeSDK } from "@opentelemetry/sdk-node";
import { LangfuseSpanProcessor } from "@langfuse/otel";

const sdk = new NodeSDK({ spanProcessors: [new LangfuseSpanProcessor()] });
sdk.start();
```

An OpenTelemetry exporter that sent spans to a Langfuse OTLP endpoint can send them to the same path on Crucible,
with either form of [authentication](../guides/sending-traces.md#authentication).

## What maps

| Crucible field | Langfuse attribute |
|---|---|
| Observation type | `langfuse.observation.type`: `span`, `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`, `evaluator`, `guardrail` |
| Level | `langfuse.observation.level`: `DEBUG` to `debug`, `DEFAULT` to `default`, `WARNING` to `warning`, `ERROR` to `error` |
| Status message | `langfuse.observation.status_message` |
| Input, output | `langfuse.observation.input`, `langfuse.observation.output` |
| Metadata | `langfuse.observation.metadata.*`, `langfuse.observation.metadata` |
| Trace metadata | `langfuse.trace.metadata.*`, `langfuse.trace.metadata` |
| Model | `langfuse.observation.model.name` |
| Model parameters | `langfuse.observation.model.parameters` |
| Token usage | `langfuse.observation.usage_details` |
| Cost | `langfuse.observation.cost_details`, in USD |
| Completion start | `langfuse.observation.completion_start_time` |
| Prompt name and version | `langfuse.observation.prompt.name`, `langfuse.observation.prompt.version` |
| Trace name | `langfuse.trace.name` |
| User | `user.id`, `langfuse.user.id` |
| Session | `session.id`, `langfuse.session.id` |
| Trace tags | `langfuse.trace.tags` |
| Public flag | `langfuse.trace.public` |
| Environment, release, version | `langfuse.environment`, `langfuse.release`, `langfuse.version` |

- `propagate_attributes` (Python) and `propagateAttributes` (JavaScript) write the user, session, tags, trace name,
  version and trace metadata onto every span in their scope, so each span carries them.
- Usage details in Langfuse's buckets, where `input` excludes cached tokens, become Crucible's counts, where cache
  reads and writes count inside input.
- The Python v5 and JavaScript v6 SDKs JSON-encode each metadata value; Crucible decodes them, so `{"suite":
  "smoke"}` is stored as `suite = smoke`.
- The trace list shows the trace name, user, session, environment, release, tags, tokens and cost. Prompt name and
  version, status message, version and the public flag are columns of `crucible_sql.spans`; metadata and trace
  metadata are in `crucible_sql.span_payloads`.

## Media

The SDKs move images, audio and files out of a payload and upload them separately, leaving a reference such as
`@@@langfuseMedia:type=image/png|id=...|source=base64_data_uri@@@` in its place. Crucible serves their media
endpoint, `/api/public/media`, on both HTTP listeners:

- With object storage configured ([media](../self-hosting/media.md)), the uploads go to the bucket, and the
  **Media** tab of the span panel reads the references in the span's input, output and metadata and shows the media
  each one names, with where it appears ([media](../guides/media.md)).
- Without it, the SDK has already put the reference in the payload when it asks for the upload, so the media are
  lost. The media endpoint answers `501 media_not_supported` with the two ways to keep them, which the SDK logs as
  a media upload error. Crucible names the SDK setting in its startup log, and logs the same warning the first
  time it refuses an upload. Set
  `LANGFUSE_MEDIA_UPLOAD_ENABLED=false` where the SDK runs to keep media inline in the payload, under the 8 MiB cap
  of one field, or configure object storage.

## Known limits

- Trace-level input and output (`langfuse.trace.input`, `langfuse.trace.output`) are not mapped and stay among the
  span's `attributes`, as do experiment keys (`langfuse.experiment.*`).
- A span sent on this path that carries another library's keys is read as that library: the path is a hint, and a
  stronger rule wins over it.
- Calls to Langfuse's REST API answer 404. In Python, `auth_check()` raises, `get_prompt()` raises unless it is
  given a `fallback`, and `create_score()` logs an error and drops the score. See
  [moving from Langfuse](../migration/langfuse.md).
