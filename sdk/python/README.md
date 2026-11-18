# Crucible SDK for Python

`crucible-sdk` sends OpenTelemetry traces from Python agents and MCP servers to Crucible. Import it as `crucible_sdk`.

- Agent helpers: spans for steps and model calls, with input, output, token usage and cost, and session, user, tags
  and metadata carried onto every span.
- MCP servers built on the official `mcp` package (2.3 or newer, below 3): one middleware that adds Crucible's keys
  to the span the `mcp` library already opens for every message. It never opens a second span.
- MCP clients: a wrapper that sends the caller's trace context in `params._meta`, so a trace shows the agent's call and
  the server's handling of it together.

> **Status: in development.** The package is not published yet. It will be published to PyPI as `crucible-sdk`.
> Until then, install it from this repository.

## Install

```sh
uv pip install "./sdk/python[mcp]"   # or: pip install "./sdk/python[mcp]"
```

The `mcp` extra adds `crucible_sdk.mcp`. Agent tracing alone needs only the core package. Python 3.10 to 3.14.

## Set up

```python
import crucible_sdk

crucible_sdk.init(
    endpoint="http://crucible.internal:4318",  # default: CRUCIBLE_ENDPOINT, then http://localhost:4318
    secret_key="csk_...",  # default: CRUCIBLE_SECRET_KEY; a project key with the ingest scope
    service_name="support-agent",
)
```

| Option | Default | Meaning |
|---|---|---|
| `endpoint` | `CRUCIBLE_ENDPOINT`, then `http://localhost:4318` | Crucible's OTLP HTTP listener; spans go to `<endpoint>/v1/traces` as protobuf |
| `secret_key` | `CRUCIBLE_SECRET_KEY` | Sent as `Authorization: Bearer <key>`. Without one, `init` logs how to set it |
| `service_name` | from the OpenTelemetry resource | Resource `service.name`; MCP spans fall back to it as the server name |
| `environment` | none | Resource `deployment.environment.name` |
| `release` | none | Resource `service.version` |
| `deploy_id` | none | Resource `crucible.deploy.id`; Crucible uses the server version when it is absent |
| `sample_rate` | `1.0` | Fraction of traces exported, decided by trace id so a trace is kept or dropped whole |
| `queue_size` | `2048` | Spans waiting for export. A full queue drops new spans and counts them; it never blocks |
| `flush_interval` | `1.0` | Seconds between exports of a partial batch (batches hold up to 512 spans) |

`crucible_sdk.flush(timeout=5.0)` waits until every span that ended before the call has been sent and returns `False`
when the timeout passes first. `crucible_sdk.shutdown(timeout=5.0)` sends what is queued and stops; it also runs at
interpreter exit. `crucible_sdk.export_stats()` returns the queue counters: `queued`, `dropped`, `exported` and
`failed`.

Export runs on one background thread. A collector that is down, refuses connections or never answers can delay only
that thread: ending a span takes the same time either way, and `flush` and `shutdown` return when their timeout
passes. A process forked after `init` starts its own export thread.

### With an existing OpenTelemetry setup

When the global tracer provider is an OpenTelemetry SDK `TracerProvider`, `init` adds one span processor to it and
leaves its other processors and exporters alone. Otherwise `init` creates a provider and sets it as the global one.
Call `init` after your own provider is configured. The `init` options (`service_name`, `environment`, `release`,
`deploy_id`) and a per-process `service.instance.id` are added to the resource of the spans sent to Crucible only.

## Trace an agent

```python
from decimal import Decimal

import crucible_sdk


@crucible_sdk.observe(type="agent")  # records the arguments as input and the return value as output
def answer(question: str) -> str:
    with crucible_sdk.generation("chat", model="model-large", provider="provider-x"):
        reply = call_model(question)
        crucible_sdk.set_usage(input=1200, output=300, cache_read=1000)
        crucible_sdk.set_cost(total=Decimal("0.0042"))
    return reply


with crucible_sdk.session("conversation-17"), crucible_sdk.user("user-42"), crucible_sdk.tags("beta"):
    answer("Where is my order?")
```

| Helper | What it sets |
|---|---|
| `observe(name, type=...)` | A span with `crucible.observation.type` (`span`, `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`, `evaluator`, `guardrail`); optional `input`, `metadata`, `tags` and `trace_name` (`crucible.trace.name`, for the root observation). A context manager that yields the OpenTelemetry span, or a decorator for sync and async functions (`@observe` alone uses the function name; `capture_input=False` and `capture_output=False` turn the recording off) |
| `generation(name, model=..., provider=...)` | `observe` with type `generation`, plus `gen_ai.request.model` and `gen_ai.provider.name` |
| `set_input(value)`, `set_output(value)` | `crucible.input`, `crucible.output` on the current span: a string as is, anything else as JSON |
| `set_messages(input=, output=)`, `set_finish_reasons(*reasons)` | `gen_ai.input.messages` and `gen_ai.output.messages` as JSON, and `gen_ai.response.finish_reasons`, for generations |
| `set_usage(input=, output=, cache_read=, cache_write=, reasoning=)` | `gen_ai.usage.*` token counts. Cache reads and writes count inside `input`, reasoning inside `output` |
| `set_cost(total=, input=, output=, ...)` | `crucible.cost.<key>` in USD. Pass a `Decimal` to send the amount exactly |
| `session(id)`, `user(id)`, `tags(*values)`, `metadata(**values)` | `crucible.session.id`, `crucible.user.id`, `crucible.tags` and `crucible.metadata.<key>` on every span started inside, including spans of other libraries. Context managers or decorators; nested `tags` and `metadata` add to the enclosing ones |

The setters act on the current span, so they also work inside spans that another instrumentation opened.

## Instrument an MCP server

```python
from mcp.server.mcpserver import MCPServer

import crucible_sdk
import crucible_sdk.mcp

crucible_sdk.init(service_name="orders-server")

server = MCPServer("orders-server", version="1.4.0")


@server.tool()
def lookup_order(order_id: str) -> str: ...


crucible_sdk.mcp.instrument(server, handle_params={"lookup_order": "order_id"})
```

`instrument` works on `MCPServer` and on the low-level `Server`, over stdio and streamable HTTP, in both protocol
eras: the session-based revisions up to 2025-11-25 and the stateless 2026-07-28 revision. It adds one middleware
directly after the `mcp` library's own OpenTelemetry middleware, so every message, including one a later middleware
refuses, gets exactly one span. If that built-in middleware was removed, `instrument` puts it back.

| Option | Meaning |
|---|---|
| `capture` | Content capture rules, off by default. `Capture(tool="search_*", args=True, result=True, sample_rate=1.0, max_bytes=16384)`; the first rule whose glob matches a tool applies |
| `handle_params` | Tool name to the argument whose string value identifies the thing a call works on (an order, a ticket). Sent as `crucible.mcp.handle.id`, scrubbed and cut at 256 bytes. Nothing is added to the tool's schema or arguments |
| `identify` | A function, sync or async, called with the request context of each message; it returns `crucible_sdk.mcp.Identity(user_id=, issuer=, subject=, tenant=, handle_id=)` or `None` |
| `identity` | `"none"` sends no identity at all: no user, tenant, handle, token claims, user agent or IP prefix |
| `transport` | `"stdio"`, `"streamable_http"` or `"sse"`. By default a message with an HTTP request is HTTP and anything else is stdio |
| `server_name`, `server_version` | Default to the server's own name and version |

Inside a tool handler:

| Call | Effect |
|---|---|
| `annotate_error("auth" \| "rate_limit" \| "upstream" \| "business")` | Refines a tool error (`isError: true`) with `crucible.mcp.error.subclass` |
| `mark_validation()` | A tool error is classified as a validation failure |
| `mark_not_found()` | A tool error, or a `-32602` JSON-RPC error, is classified as not found |
| `identify(user_id=, issuer=, subject=, tenant=, handle_id=)` | Sets who made this call; it takes precedence over the `identify` hook |

Outside a handler these calls do nothing.

### What the middleware sends

On every message it adds `crucible.mcp.capture_source` (`sdk`), the era, the transport, the server name and version,
and the client's self-reported name, version and a hash of its capabilities. Notifications are marked
`crucible.mcp.is_notification`. On HTTP it adds the user agent, the client's /24 (IPv4) or /48 (IPv6) network, the
request size from `Content-Length` and, in the session-based era, the session id as the first 16 bytes of its SHA-256.
A token the hosting server verified contributes its client id, issuer and subject.

After the handler it adds the outcome: annotations as `crucible.mcp.error.class` and `.error.subclass`, `internal` for
an exception nothing handled, the cancel source (`notification`, or `stream_close` when the stream ended first), the
result type, and the number of content blocks per type.

For a multi-round call (2026-07-28), the request's and the result's `requestState` are sent only as keyed hashes
(`crucible.mcp.mrtr.state_in` and `.state_out`: HMAC-SHA256 with the secret key, first 16 bytes), so Crucible can
chain the rounds without seeing the state.

Never sent: `requestState` values, raw session ids, raw IP addresses, Authorization values, cookies and baggage.

### Content capture and scrubbing

Arguments and results are captured only for tools a `capture` rule matches. The sampling decision comes first, before
anything is copied. The value is encoded as JSON under a byte cap (16 KiB per field by default) and scrubbed while it
is encoded, so the work is bounded by the cap however large the value is. A cut value is marked
`crucible.mcp.content.truncation = cap_exceeded`. Only a result's content is captured, never its envelope.

The rules are data: `crucible_sdk/scrub_rules.json` is a copy of the SDK conformance suite's shared rules, with
their examples. They replace PEM blocks, JWTs, bearer tokens, Crucible secret keys, card numbers (Luhn checked),
email addresses, IPv6 and IPv4 addresses and phone numbers, in that order. In captured JSON the value under an
`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie` or `requestState` key is replaced whole. The loader
refuses any pattern outside the shared subset (no lookaround, backreferences, anchors, class escapes or nested
unbounded repetition). One shared rule, the email local part, is unbounded: in Python's backtracking matcher it costs
about 0.15 s on a 16 KiB run of letters and digits with no `@`, which the capture cap bounds. Error messages and
exception events of MCP spans are cut (8 KiB and 16 KiB) and scrubbed with the same rules before export.

## Instrument an MCP client

```python
from mcp import Client

import crucible_sdk.mcp

async with crucible_sdk.mcp.instrument_client(Client("http://orders.internal/mcp")) as client:
    with crucible_sdk.observe("refund", type="agent"):
        result = await client.call_tool("lookup_order", {"order_id": "A-1001"})
```

`call_tool` and `get_prompt` write the caller's W3C trace context (`traceparent`, `tracestate`) into `params._meta`,
never baggage. Over its network transports the `mcp` client also opens a CLIENT span per request and writes that span's
context there itself, with the global OpenTelemetry propagator; the wrapper adds Crucible's keys to that span instead
of opening another one. `read_resource` is
tagged but its `_meta` is left alone, because a request with `_meta` skips the client's response cache. Every other
attribute is the wrapped client's.

## Limits

- Progress notifications are not observed, so `crucible.mcp.progress.count` is never sent (absent means unobserved).
- Response sizes and HTTP response statuses are not observed by a middleware and are not sent.
- Server deadlines are not observed, so `crucible.mcp.timed_out` is never sent.
- In the 2026-07-28 era a client learns the server's name from a response, so the first call's CLIENT span has no
  server name.
- On the client side, a tool error (`isError: true`) is not marked on the CLIENT span: the `mcp` library closes that
  span before the result reaches any public hook.
- The tool inventory (registered tools and their schemas) is not sent.

## Develop

```sh
cd sdk/python
uv sync --locked
uv run --locked ruff check . && uv run --locked ruff format --check .
uv run --locked mypy
uv run --locked pytest
```

The tests run a fake OTLP collector (`tests/collector.py`) and talk to the reference server
(`conformance/reference_server.py`) over the `mcp` library's in-memory transports, over streamable HTTP on a loopback
port and over stdio, in both eras. `crucible_sdk/mcp/_seam.py` holds every use of `mcp` internals and of its provisional
middleware interface.

### Conformance

`conformance/` is this SDK's runner for the shared SDK conformance suite (`docs/testing/sdk-conformance.md` at the
repository root). `runner.py` plays one run spec; `server.py` runs the reference server, instrumented with the run
spec's options, over stdio or streamable HTTP; `manifest.json` declares that a raising tool becomes an `isError`
result and that progress is not observed. From the repository root:

```sh
cargo build -p crucible-fake-collector
target/debug/crucible-sdk-driver run --manifest sdk/python/conformance/manifest.json
```
