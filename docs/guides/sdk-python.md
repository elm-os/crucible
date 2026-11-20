# Python SDK

`crucible-sdk` sends OpenTelemetry traces from Python agents and MCP servers to Crucible over OTLP/HTTP. Import it
as `crucible_sdk`.

| Module | Contents |
|---|---|
| `crucible_sdk` | `init`, `flush`, `shutdown` and `export_stats`; the agent helpers `observe`, `generation`, the `set_*` setters and the `session`, `user`, `tags` and `metadata` scopes |
| `crucible_sdk.mcp` | `instrument` for MCP servers, `instrument_client` for MCP clients, and `annotate_error`, `mark_validation`, `mark_not_found` and `identify` for tool handlers |

It supports Python 3.10 to 3.14. `crucible_sdk.mcp` works with the official `mcp` package from 2.3 up to but not
including 3.0.

## Install

Install the package, `crucible-sdk`, from the repository into your project's environment:

```sh
uv pip install "crucible-sdk[mcp] @ git+https://github.com/elm-os/crucible.git#subdirectory=sdk/python"
```

or from a checkout:

```sh
git clone https://github.com/elm-os/crucible.git
uv pip install "./crucible/sdk/python[mcp]"   # or: pip install "./crucible/sdk/python[mcp]"
```

The `mcp` extra adds `crucible_sdk.mcp` and the `mcp` package; agent tracing alone needs only the core package.
The examples on this page are complete files. Each was run with Python 3.12 and `mcp` 2.3.0, in an environment
installed this way, with only the endpoint changed to the SDK's test collector.

## Set up

```python
# hello.py
import os

import crucible_sdk

crucible_sdk.init(
    endpoint="http://localhost:4318",
    secret_key=os.environ["CRUCIBLE_SECRET_KEY"],
    service_name="hello-agent",
)

with crucible_sdk.observe("say hello"):
    print("hello")

crucible_sdk.flush()
print(crucible_sdk.export_stats())
crucible_sdk.shutdown()
```

It prints `hello` and then
`ExportStats(queued=0, dropped=0, exported=1, failed=0, queued_bytes=0, feedback_sent=0, feedback_dropped=0, feedback_failed=0)`.

`init` starts the exporter. Spans go to `<endpoint>/v1/traces` as OTLP/HTTP protobuf with
`Authorization: Bearer <secret_key>`; give the endpoint without the path.

| Option | Default | Meaning |
|---|---|---|
| `endpoint` | `CRUCIBLE_ENDPOINT`, then `http://localhost:4318` | Base URL of an OTLP listener |
| `secret_key` | `CRUCIBLE_SECRET_KEY` | The project's secret key, sent as a Bearer token. Spans need the `ingest` scope and feedback the `write` scope; new keys have both |
| `project_id` | `CRUCIBLE_PROJECT_ID` | The key's project id (a UUID), which `feedback` needs; see [Send feedback](#send-feedback) |
| `api_url` | `CRUCIBLE_API_URL`, then `endpoint` | Base URL of the API listener, where `feedback` posts |
| `service_name` | The OpenTelemetry resource's | Resource `service.name` |
| `environment` | | Resource `deployment.environment.name` |
| `release` | | Resource `service.version` |
| `deploy_id` | | Resource `crucible.deploy.id`; without it, Crucible uses the server version as the deploy id |
| `sample_rate` | `1.0` | Share of traces exported, decided by trace id, so a trace is kept or dropped whole |
| `queue_size` | `2048` | Spans waiting for export. A full queue drops new spans and counts them |
| `queue_bytes` | `8 << 20` | Bytes of spans waiting for export or being exported, estimated from their names, attributes, events and status messages. Beyond it new spans are dropped and counted too |
| `flush_interval` | `1.0` | Seconds between exports of a partial batch |

Without a secret key, `init` logs a warning that says how to set one and runs anyway; Crucible then refuses the
spans, and they count as failed. A `sample_rate` outside 0 to 1, a `queue_size` or `queue_bytes` below 1, a
`flush_interval` that is not positive, an `api_url` that is not an `http` or `https` URL or a `project_id` that is not
a UUID raises `ValueError`. Calling `init` again replaces the previous settings.

**With an existing OpenTelemetry setup.** When the global tracer provider is an OpenTelemetry SDK
`TracerProvider`, `init` adds one span processor to it and leaves its other processors and exporters alone.
Otherwise `init` creates a provider and sets it as the global one. Call `init` after your own provider is
configured. The `init` options and a per-process `service.instance.id` are added to the resource of the spans sent to
Crucible only. Because the SDK joins the global provider, spans that other instrumentation opens reach Crucible too.

- `flush(timeout=5.0)` waits until every span that ended and every score queued before the call has been sent. It
  returns `False` when the timeout passes first.
- `shutdown(timeout=5.0)` sends the queued spans and scores, waiting at most `timeout` seconds in all, and stops.
  Later spans and scores are not sent. It also runs at interpreter exit, with the default timeout, so a script that ends normally does not need to
  call it.
- A process forked after `init` starts its own export thread.

## Trace an agent

```python
# agent.py
from decimal import Decimal

import crucible_sdk
from opentelemetry import trace

crucible_sdk.init(service_name="trip-agent", environment="staging", release="1.4.0")


def call_model(question: str) -> str:
    """Stands in for your model client."""
    return f'For "{question}": Lisbon'


@crucible_sdk.observe(type="agent", trace_name="trip planner")
def plan_trip(question: str) -> str:
    trace.get_current_span().set_attribute("crucible.trace.tags", ["beta"])
    with crucible_sdk.generation("chat", model="model-large", provider="acme") as span:
        reply = call_model(question)
        span.set_attribute("gen_ai.response.model", "model-large-2026-05")
        crucible_sdk.set_messages(
            input=[{"role": "user", "parts": [{"type": "text", "content": question}]}],
            output=[{"role": "assistant", "parts": [{"type": "text", "content": reply}]}],
        )
        crucible_sdk.set_usage(input=1200, output=300, cache_read=1000)
        crucible_sdk.set_cost(total=Decimal("0.0042"))
    return reply


with (
    crucible_sdk.session("conv-42"),
    crucible_sdk.user("user-7"),
    crucible_sdk.tags("beta"),
    crucible_sdk.metadata(plan="pro"),
):
    print(plan_trip("Where should I go in May?"))
```

This `init` reads the endpoint and the key from `CRUCIBLE_ENDPOINT` and `CRUCIBLE_SECRET_KEY`. The decorator
records the call's arguments as the input, `{"question": "Where should I go in May?"}`, and the return value as the
output.

| Helper | What it sets |
|---|---|
| `observe(name, type=...)` | A span with `crucible.observation.type`: `span` (the default), `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`, `evaluator` or `guardrail`. Optional `input`, `metadata`, `tags` and `trace_name` (`crucible.trace.name`, for the root observation). As a context manager it yields the OpenTelemetry span. As a decorator of a sync or async function it records the arguments as `crucible.input` and a return value other than `None` as `crucible.output`; `capture_input=False` and `capture_output=False` turn that off, and `@observe` alone names the span after the function |
| `generation(name, model=..., provider=...)` | `observe` with type `generation`, plus `gen_ai.request.model` and `gen_ai.provider.name` |
| `set_input(value)`, `set_output(value)` | `crucible.input`, `crucible.output` on the current span: a string as it is, anything else as JSON |
| `set_messages(input=, output=)` | `gen_ai.input.messages` and `gen_ai.output.messages` as JSON, for generations |
| `set_finish_reasons(*reasons)` | `gen_ai.response.finish_reasons` |
| `set_usage(input=, output=, cache_read=, cache_write=, reasoning=)` | The `gen_ai.usage.*` token counts. Cache reads and writes count inside `input`, reasoning inside `output`. A negative or non-integer count raises `ValueError` |
| `set_cost(total=, input=, output=, cache_read=, cache_write=, reasoning=)` | `crucible.cost.<key>` in USD. A `Decimal` is sent exactly, as a string. A provided cost replaces the one Crucible would calculate |
| `session(id)`, `user(id)`, `tags(*values)`, `metadata(**values)` | `crucible.session.id`, `crucible.user.id`, `crucible.tags` and `crucible.metadata.<key>` on every span started inside, the spans of other libraries included. Context managers or decorators; nested `tags` and `metadata` add to the enclosing ones |

The setters act on the current span, so they also work inside spans that another instrumentation opened.

[Sending traces](sending-traces.md#attributes-the-trace-views-read) lists which keys the trace views show on main.
Two notes for agents: the views show the model from `gen_ai.response.model`, which no helper sets, so set it on the
span as the example does; and the trace list shows the trace tags, `crucible.trace.tags`, not the span tags that
`tags` sets.

## Send feedback

`crucible_sdk.feedback(name, value, ...)` records a score with source `feedback` on a trace, a span or a session: the
scores of [facts](facts.md), sent by the SDK.

```python
# feedback.py
import os

import crucible_sdk

crucible_sdk.init(
    endpoint="http://localhost:8080",  # the API listener, which also takes spans
    secret_key=os.environ["CRUCIBLE_SECRET_KEY"],  # new keys have the ingest and write scopes
    project_id=os.environ["CRUCIBLE_PROJECT_ID"],  # the key's project
    service_name="support-agent",
)

with crucible_sdk.observe("answer", type="agent") as span:
    crucible_sdk.set_output("Your refund is on its way.")
    trace_id = format(span.get_span_context().trace_id, "032x")

# Straight after the trace ends: its spans are sent first.
crucible_sdk.feedback("helpful", 1, trace_id=trace_id, comment="solved it", idempotency_key=f"thumbs-{trace_id}")
crucible_sdk.feedback("tone", "friendly", trace_id=trace_id, score_id="reviewer-2")

crucible_sdk.flush()
stats = crucible_sdk.export_stats()
print(stats.exported, stats.feedback_sent, stats.feedback_dropped, stats.feedback_failed)
crucible_sdk.shutdown()
```

Run against a Crucible server, it prints `1 2 0 0`, and the trace list shows both scores on the trace.

Each score goes to `POST <api_url>/api/v1/projects/<project_id>/feedback` as JSON with
`Authorization: Bearer <secret_key>`; the key needs the `write` scope.

- **Where it goes.** `api_url` defaults to `endpoint`. The API listener (port 8080 by default) serves both
  `/v1/traces` and the API, so pointing `endpoint` at it, as above, is enough. The OTLP listener (port 4318) serves
  only ingest and answers 404 for the feedback route: with `endpoint` on port 4318, set `api_url` to the API
  listener.
- **The project id** is the key's project: `crucible admin create-key` prints it as `project_id`, a keys file names
  it with each key, and the UI shows it as `project=` in the address bar. Without it, every score is dropped.
- **The score.** The target is `trace_id`, `trace_id` with `span_id`, or `session_id`. `name` (without `#`) and
  `value` (a number, a bool or a string) are required; `score_id`, `data_type` (`numeric`, `boolean`,
  `categorical` or `text`), `comment` (4 KiB), `metadata` (a JSON object, 16 KiB) and `retracted` are optional. The
  latest score of a name (and `score_id`) on a target wins; `retracted=True` removes it.
- **Never in the way.** `feedback` queues the score and returns at once; it never raises. One background thread posts
  the scores in call order, one at a time, each after the spans that ended before the call, so a score on a trace
  that just ended finds the trace.
- **Sent once.** Each score carries an `Idempotency-Key`: `idempotency_key` when you give one, else a random key. A
  failed connection, a timeout, 408, 429, a 5xx answer and a `conflict` 409 are retried with the same key and body,
  three attempts in all, after the answer's `Retry-After` (at most 10 s) or else after 1 s and 2 s, so Crucible
  writes the score once. Any other refusal is final.
- **Counted.** Up to 1,024 scores wait to be sent; beyond that a score is dropped. `export_stats()` counts
  `feedback_sent`, `feedback_dropped` (queue full, no project id, or `shutdown` out of time) and `feedback_failed`
  (refused, or failed on every attempt). The first drop and the first failure each log one warning on the
  `crucible_sdk` logger that names the URL, Crucible's answer and the settings to check.
- **Sessions.** Crucible stores a session about ten seconds after its spans and answers 404 for a session it has
  not stored, which counts as a failure: score a session once its spans are stored.

## Instrument an MCP server

```python
# server.py
import crucible_sdk
import crucible_sdk.mcp
from mcp.server.mcpserver import MCPServer
from mcp_types import CallToolResult, TextContent

crucible_sdk.init(service_name="orders-mcp")

server = MCPServer("orders-mcp", version="2.1.0")
ORDERS = {"A-1001": "shipped"}


@server.tool()
def lookup_order(order_id: str) -> CallToolResult:
    """Looks an order up by id."""
    crucible_sdk.mcp.identify(user_id="user-7", tenant="acme")
    status = ORDERS.get(order_id)
    if status is None:
        crucible_sdk.mcp.mark_not_found()
        return CallToolResult(content=[TextContent(type="text", text=f"no order {order_id}")], is_error=True)
    return CallToolResult(content=[TextContent(type="text", text=f"order {order_id}: {status}")])


@server.tool()
def refund(order_id: str, card: str) -> CallToolResult:
    """Refunds an order to a card."""
    crucible_sdk.mcp.annotate_error("upstream")
    text = f"the payment service refused {order_id}"
    return CallToolResult(content=[TextContent(type="text", text=text)], is_error=True)


crucible_sdk.mcp.instrument(
    server,
    capture=[crucible_sdk.mcp.Capture(tool="refund", max_bytes=4096)],
    handle_params={"lookup_order": "order_id", "refund": "order_id"},
)
server.run("stdio")
```

`instrument(server, ...)` works on `MCPServer` and on the low-level `Server`, over stdio and streamable HTTP
(`server.run("streamable-http")`), in both protocol eras: the session-based revisions up to 2025-11-25 and the
stateless 2026-07-28 revision. It adds one middleware directly after the `mcp` library's own OpenTelemetry
middleware, which opens a SERVER span for every message; the Crucible middleware adds Crucible's keys to that span
and never opens a second one, so every message, a refused one included, gets exactly one span. If the built-in
middleware was removed, `instrument` puts it back. It returns the server it was given.

| Option | Meaning |
|---|---|
| `capture` | Content capture rules, off when empty; see below |
| `handle_params` | Tool name to the argument whose string value identifies the thing a call works on (an order, a ticket). Sent as `crucible.mcp.handle.id`, scrubbed and cut at 256 bytes. Nothing is added to the tool's schema or arguments |
| `identify` | A function, sync or async, called with the request context of each message. It returns `crucible_sdk.mcp.Identity(user_id=, issuer=, subject=, tenant=, handle_id=)` or `None` |
| `identity` | `"none"` sends no identity at all: no user, tenant, handle, token claims, user agent or IP prefix |
| `transport` | `"stdio"`, `"streamable_http"` or `"sse"`. By default a message with an HTTP request is HTTP and anything else is stdio |
| `server_name`, `server_version` | Default to the server's own name and version |

### What the middleware sends

On every message it adds `crucible.mcp.capture_source = sdk`, the era, the transport, the server name and version,
and the client's self-reported name, version and a hash of its capabilities. Notifications are marked
`crucible.mcp.is_notification`. On HTTP it adds the user agent, the client's /24 (IPv4) or /48 (IPv6) network, the
request size from `Content-Length` and, in the session-based era, the session id as the first 16 bytes of its
SHA-256. A token that the hosting server verified contributes its client id, issuer and subject.

After the handler it adds the outcome: the annotations as `crucible.mcp.error.class` and `.error.subclass`,
`internal` for an exception nothing handled, the cancel source (`notification`, or `stream_close` when the stream
ended first), the result type, and the number of content blocks of each type. For a multi-round call (2026-07-28),
the request's and the result's `requestState` are sent only as keyed hashes (`crucible.mcp.mrtr.state_in` and
`.state_out`: HMAC-SHA256 with the secret key, first 16 bytes), so Crucible can chain the rounds without seeing the
state.

Never sent: `requestState` values, raw session ids, raw IP addresses, Authorization values, cookies and baggage.
Error messages and exception events of MCP spans are cut to 8,192 and 16,384 characters and scrubbed before export.

On main, Crucible stores these spans as spans: the trace view shows a `tools/call` span as a `tool` span and a
tool error with status `error`, captured arguments and results become the span's input and output, and the other
MCP keys stay among its attributes (the payload route's `attributes` field). Crucible also records each such span
as an MCP operation on the server side, which the SQL workspace's `crucible_sql.mcp_operations` view reads.

### Errors and identity in tool handlers

Inside a tool handler these calls refine the operation being handled; outside one they do nothing:

| Call | Effect |
|---|---|
| `annotate_error("auth" \| "rate_limit" \| "upstream" \| "business")` | Refines a tool error (`isError: true`) with `crucible.mcp.error.subclass`; another value raises `ValueError` |
| `mark_validation()` | The tool error is a validation failure |
| `mark_not_found()` | The tool error, or a -32602 JSON-RPC error, means "not found" |
| `identify(user_id=, issuer=, subject=, tenant=, handle_id=)` | Sets who made this call; it takes precedence over the `identify` hook |

Crucible hashes the subject with a per-project salt and keeps only the hash and the issuer, in the MCP operation's
`user_subject_hash` and `user_issuer`.

### Content capture and scrubbing

Arguments and results are captured only for tools that a `capture` rule matches. A rule is
`Capture(tool="*", args=True, result=True, sample_rate=1.0, max_bytes=16384)`; `tool` is a glob over tool names, and
the first rule whose glob matches applies. The sampling decision comes first, before anything is copied. The value
is encoded as JSON under the byte cap and scrubbed while it is encoded, so the work is bounded by the cap however
large the value is, and it is sent as `gen_ai.tool.call.arguments` or `gen_ai.tool.call.result`. A cut value is
marked `crucible.mcp.content.truncation = cap_exceeded`. Only a result's content is captured, never its envelope.

The rules are data: `crucible_sdk/scrub_rules.json` is a copy of the SDK conformance suite's shared rules. They
replace PEM blocks, JWTs, bearer tokens, Crucible secret keys, card numbers (Luhn checked), email addresses, IPv6
and IPv4 addresses and phone numbers, in that order, each match with a marker such as `[redacted:card]`. In
captured JSON, the value under an `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie` or `requestState`
key is replaced whole. The loader refuses any pattern outside the shared subset (no lookaround, backreferences,
anchors, class escapes or nested unbounded repetition). One shared rule, the email local part, is unbounded: in
Python's backtracking matcher it costs about 0.15 s on a 16 KiB run of letters and digits with no `@`, which the
capture cap bounds.

## Instrument an MCP client

```python
# client.py
import asyncio
import os
import sys

import crucible_sdk
import crucible_sdk.mcp
from mcp import Client
from mcp.client.stdio import StdioServerParameters

crucible_sdk.init(service_name="support-agent")

# The stdio transport starts the server with a minimal environment, so pass the Crucible settings on.
orders_server = StdioServerParameters(
    command=sys.executable,
    args=["server.py"],
    env={key: os.environ[key] for key in ("CRUCIBLE_ENDPOINT", "CRUCIBLE_SECRET_KEY") if key in os.environ},
)


async def main() -> None:
    async with crucible_sdk.mcp.instrument_client(Client(orders_server)) as client:
        with crucible_sdk.session("conv-42"), crucible_sdk.observe("handle refund request", type="agent"):
            order = await client.call_tool("lookup_order", {"order_id": "A-1001"})
            refund = await client.call_tool("refund", {"order_id": "A-1001", "card": "4111 1111 1111 1111"})
            print(order.content, refund.is_error)


asyncio.run(main())
```

`instrument_client(client)` wraps an `mcp.Client`; every attribute it does not define is the wrapped client's.
`call_tool` and `get_prompt` write the caller's W3C trace context (`traceparent`, `tracestate`) into
`params._meta`, never baggage. Over its stream transports the `mcp` client also opens a CLIENT span per request
and writes that span's context there itself; the wrapper adds Crucible's keys to that span instead of opening
another one. An instrumented server records its span as the CLIENT span's child, so the agent's trace holds the
server's handling of each call. `read_resource` is tagged but its `_meta` is left alone, because a request with
`_meta` skips the client's response cache. The `transport` option sets the label, which is otherwise
`streamable_http` for a URL, `stdio` for `StdioServerParameters` and `unknown` for anything else.

## The export queue

Export runs on one background thread. `on_end` only appends a span to a bounded queue; the thread sends batches of
up to 512 spans as OTLP/HTTP protobuf, as soon as a batch is full or every `flush_interval` seconds. When the queue
holds `queue_size` spans, or `queue_bytes` bytes of spans with the batch being exported, a span that ends is dropped
and counted, and the first drop logs a warning that says which settings to raise. A collector that is down, refuses connections or never answers can delay only that thread: ending
a span takes the same time either way, and `flush` and `shutdown` return when their timeout passes.

Within its 10-second timeout per export, the OpenTelemetry exporter the SDK uses retries a refused export, such as
a 503, and waits for the server's `Retry-After` when it fits in the time left. A batch that still fails counts as
failed and is not sent again.

`export_stats()` returns the counters of the current exporter, all zero before `init` and after `shutdown`:

| Counter | Meaning |
|---|---|
| `queued` | Spans waiting in the queue |
| `dropped` | Spans dropped because the queue was full |
| `queued_bytes` | Estimated bytes of the queued spans and of the batch being exported |
| `exported` | Spans the collector accepted |
| `failed` | Spans in export requests that failed |
| `feedback_sent` | Scores Crucible accepted |
| `feedback_dropped` | Scores never sent: the feedback queue was full, no project id is set, or `shutdown` ran out of time |
| `feedback_failed` | Scores Crucible refused, or that failed on every attempt |

## Configuration

| Variable | Used for |
|---|---|
| `CRUCIBLE_ENDPOINT` | `endpoint` when the option is not given |
| `CRUCIBLE_SECRET_KEY` | `secret_key` when the option is not given |
| `CRUCIBLE_PROJECT_ID` | `project_id` when the option is not given |
| `CRUCIBLE_API_URL` | `api_url` when the option is not given |

Every other setting is an option of `init` or `instrument`. The OpenTelemetry exporter the SDK builds is configured
in code, so `OTEL_EXPORTER_OTLP_*` endpoint settings do not redirect it. Each export and each feedback request
carries `User-Agent: crucible-sdk-python/<version>`.

## Limits

- Progress notifications are not observed, so `crucible.mcp.progress.count` is never sent (absent means
  unobserved).
- Response sizes and HTTP response statuses are not observed by a middleware and are not sent.
- Server deadlines are not observed, so `crucible.mcp.timed_out` is never sent.
- In the 2026-07-28 era a client learns the server's name from a response, so the first call's CLIENT span has no
  server name.
- On the client side, a tool error (`isError: true`) is not marked on the CLIENT span: the `mcp` library closes that
  span before the result reaches any public hook.
- The tool inventory (registered tools and their schemas) is not sent.

## Conformance

The SDK passes the shared [SDK conformance suite](../testing/sdk-conformance.md), agent and MCP cases, in both protocol
eras over stdio and streamable HTTP, with no skipped case. Its capability manifest,
`sdk/python/conformance/manifest.json`, declares that a raising tool becomes an `isError` result and that progress is
not observed. CI runs the suite (`.github/workflows/sdk-conformance.yml`) on every pull request that touches the SDKs or
the suite. To run it as CI does, on Linux with Rust and the SDK's own toolchain, from the repository root:

```sh
uv sync --locked --project sdk/python
cargo build -p crucible-fake-collector
target/debug/crucible-sdk-driver run --manifest sdk/python/conformance/manifest.json
```

The SDK's own tests (`.github/workflows/ci-sdk-python.yml`) run against a fake OTLP collector and talk to the
reference server over the `mcp` library's in-memory transports, over streamable HTTP on a loopback port and over
stdio, in both eras.
