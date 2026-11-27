# Crucible SDK for TypeScript

`@elm-os/crucible` sends traces from Node.js agents and MCP servers to a self-hosted Crucible. It has three entry
points:

| Import                        | Contents                                                                                         |
| ----------------------------- | ------------------------------------------------------------------------------------------------ |
| `@elm-os/crucible`            | `init`, `flush`, `shutdown`, `stats`; agent helpers `withContext` and `observe`; `feedback`      |
| `@elm-os/crucible/mcp`        | `instrument`, `instrumentHandler`, `annotateError`, `markValidation`, `markNotFound`, `identify` |
| `@elm-os/crucible/mcp-client` | `instrumentClient`                                                                               |

Install it from the GitHub release of Crucible 0.1.3; the SDK has the server's version, and
[the guide](../../docs/guides/sdk-typescript.md#install) has the details:

```sh
npm install https://github.com/elm-os/crucible/releases/download/v0.1.3/elm-os-crucible-0.1.3.tgz
```

It requires Node.js 24. The MCP entry points work with the 2.x line of the official TypeScript MCP SDK
(`@modelcontextprotocol/server` and `@modelcontextprotocol/client` `>=2.0.0 <3`, optional peers). Servers on the 1.x
line can use `crucible proxy` instead ([MCP proxy](../../docs/guides/mcp-proxy.md)).

## Setup

```ts
import { init, shutdown } from "@elm-os/crucible";

init({
  endpoint: "http://crucible.internal:4318", // or CRUCIBLE_ENDPOINT; default http://localhost:4318
  secretKey: process.env.CRUCIBLE_SECRET_KEY, // a project secret key with the ingest scope
  serviceName: "orders-mcp",
});

process.on("SIGTERM", async () => {
  await shutdown(); // exports what is queued, within exportTimeoutMs
  process.exit(0);
});
```

Spans go to `POST {endpoint}/v1/traces` as OTLP/HTTP protobuf with `Authorization: Bearer <secretKey>`.

| Option                          | Default                                           | Effect                                                                   |
| ------------------------------- | ------------------------------------------------- | ------------------------------------------------------------------------ |
| `endpoint`                      | `CRUCIBLE_ENDPOINT`, else `http://localhost:4318` | Base URL of the OTLP listener; `/v1/traces` is added unless present      |
| `secretKey`                     | `CRUCIBLE_SECRET_KEY`                             | Required; `init` throws without it and says which setting to use         |
| `projectId`                     | `CRUCIBLE_PROJECT_ID`                             | The key's project id (a UUID), which `feedback` needs                    |
| `apiUrl`                        | `CRUCIBLE_API_URL`, else `endpoint`               | Base URL of the API listener, where `feedback` posts                     |
| `serviceName`, `serviceVersion` |                                                   | Resource `service.name`, `service.version`                               |
| `environment`                   |                                                   | Resource `deployment.environment.name`                                   |
| `release`                       |                                                   | `crucible.release` on every span                                         |
| `deployId`                      |                                                   | Resource `crucible.deploy.id`; Crucible falls back to the server version |
| `queueSize`                     | 2,048                                             | Spans held for export, including the batch being sent                    |
| `queueBytes`                    | 8 MiB                                             | Bytes of spans held for export, estimated from their strings             |
| `flushIntervalMs`               | 1,000                                             | Longest wait before a partial batch is sent                              |
| `exportTimeoutMs`               | 10,000                                            | Bound on one export, and on `flush` and `shutdown`                       |

The SDK uses its own tracer provider and never registers it globally, so an application's own OpenTelemetry setup
is left alone. When the application has a context manager registered, Crucible spans nest under the active span
and the application's spans nest under Crucible spans. `OTEL_*` variables cannot redirect the export, replace its
Bearer key or turn its sampling off.

Export runs off the request path. When the queue is full, a span is dropped and counted; ending a span never
blocks, whether the collector is up, refusing connections or not answering. `stats()` returns the counters
`{ queued, queuedBytes, dropped, exported, failed, feedbackSent, feedbackDropped, feedbackFailed }`. The first
failed export after `init` emits one Node warning, code `CRUCIBLE_EXPORT`, on stderr, with the traces URL, the
status and Crucible's reason; later failures only count in `failed`.

## Agent helpers

```ts
import { observe, withContext } from "@elm-os/crucible";

await withContext(
  { sessionId: "conv-42", userId: "u-7", tags: ["beta"], metadata: { plan: "pro" } },
  () =>
    observe("plan trip", { type: "agent" }, async () => {
      await observe("chat", { type: "generation" }, async (generation) => {
        const reply = await callModel();
        generation.update({
          model: "model-large",
          provider: "acme",
          usage: { input: 1200, output: 300, cacheRead: 1000 },
          cost: { total: "0.0042" },
          prompt: { name: "trip-planner", version: 3 },
          output: reply.text,
        });
      });
    }),
);
```

- `withContext` puts `crucible.session.id`, `crucible.user.id`, `crucible.tags` and `crucible.metadata.<key>` on
  every span started inside it, across `await`. Nested scopes add tags and override the other values.
- `observe(name, options?, fn)` runs `fn` in a new span and ends it when `fn` returns or its promise settles. A
  throw or rejection marks the span as an error and is passed on. `type` is sent as `crucible.observation.type`.
- `update` sets `crucible.trace.name`, `crucible.input` and `crucible.output` (strings as they are, other
  values as JSON), `gen_ai.input.messages` and `gen_ai.output.messages` (as JSON), `gen_ai.response.finish_reasons`,
  `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.provider.name`, the `gen_ai.usage.*` token counts,
  provided cost as `crucible.cost.<usage key>` in USD, and the prompt reference `crucible.prompt.name` and
  `crucible.prompt.version`.

## Feedback

```ts
import { feedback, init, observe } from "@elm-os/crucible";

init({
  endpoint: "http://crucible.internal:8080", // the API listener, which also takes spans
  projectId: process.env.CRUCIBLE_PROJECT_ID, // the key's project
  serviceName: "support-agent",
});

const traceId = await observe("answer", { type: "agent" }, async (agent) => {
  // the agent's work
  return agent.span.spanContext().traceId;
});

feedback({ traceId, name: "helpful", value: 1, comment: "solved it" });
feedback({ traceId, name: "tone", value: "friendly", idempotencyKey: `tone-${traceId}` });
```

`feedback(score)` records a score with source `feedback` on a trace (`traceId`), a span (`traceId` and `spanId`) or a
session (`sessionId`), and returns at once; it never throws. It posts to
`{apiUrl}/api/v1/projects/{projectId}/feedback` with `Authorization: Bearer <secretKey>`; the key needs the `write`
scope.

- The other fields are the route's: `scoreId`, `dataType` (`numeric`, `boolean`, `categorical` or `text`), `comment`
  (4 KiB), `metadata` (a JSON object, 16 KiB) and `retracted`. The latest score of a name (and `scoreId`) on a
  target wins.
- One background sender posts the scores in call order, each after the spans that ended before the call, so a score
  on a trace that just ended finds the trace.
- Each score carries an `Idempotency-Key`, `idempotencyKey` or else a random one. A failed connection, a timeout,
  408, 429, a 5xx answer and a `conflict` 409 are retried with the same key and body, three attempts in all, after
  `Retry-After` (at most 10 s) or else 1 s and 2 s, so Crucible writes the score once.
- `apiUrl` defaults to `endpoint`. The API listener (port 8080) serves both `/v1/traces` and the API; the OTLP
  listener (port 4318) does not serve the API, so with `endpoint` on port 4318, set `apiUrl`.
- The project id is the key's project: `crucible admin create-key` prints it as `project_id`, and the UI shows it as
  `project=` in the address bar. Without it, scores are dropped.
- Up to 1,024 scores wait to be sent; beyond that a score is dropped. `stats()` counts sent, dropped and failed
  scores, and the first drop and the first failure each raise one process warning (`CRUCIBLE_FEEDBACK`) that says
  what to change. `flush` and `shutdown` send the queued scores too.
- Crucible answers 404 for a target it does not know yet, which counts as failed: a trace none of whose spans has
  ended and been sent (a score sent from inside the trace's only open span, for example), and a session, which
  Crucible stores about ten seconds after its spans. Score a trace once one of its spans has ended, and a session
  once its spans are stored.

## MCP servers

```ts
import { createMcpHandler } from "@modelcontextprotocol/server";
import { StdioServerTransport, serveStdio } from "@modelcontextprotocol/server/stdio";
import { instrument, instrumentHandler } from "@elm-os/crucible/mcp";

// A server instance with a transport you connect yourself (stdio, or a session HTTP transport).
await instrument(buildServer()).connect(new StdioServerTransport());

// A factory for serveStdio.
serveStdio(instrument(buildServer));

// A stateless HTTP handler: the factory form plus the edge companion.
const handler = instrumentHandler(createMcpHandler(instrument(buildServer)));
```

`instrument(server)` replaces the public `connect` of the instance and of an `McpServer`'s inner server, and taps
the given transport in place; it reads no private member of the MCP SDK. `instrumentHandler` records the HTTP
exchanges that `createMcpHandler` answers before any server instance exists, such as an unsupported protocol
revision, a wrong content type or `subscriptions/listen`. If a transport cannot be tapped, the server runs
uninstrumented.

Each request and notification becomes one SERVER span with Crucible's MCP keys and
`crucible.mcp.capture_source = sdk`: method and target, JSON-RPC id, era, protocol version, transport, client and
server name and version, progress count, the error evidence, the hashed legacy session id, the
multi-round state hashes, the request size and user agent on HTTP, and content block counts. A server span is the
child of the trace context in `params._meta`; baggage is never read. Crucible records each such span as an MCP
operation, which takes the MCP keys: they show on the MCP **Operation** page and in the SQL workspace's
`crucible_sql.mcp_operations`, not among the span's attributes.

Inside a tool handler:

```ts
import { annotateError, identify, markNotFound, markValidation } from "@elm-os/crucible/mcp";

annotateError("upstream"); // auth, rate_limit, upstream or business; refines a tool error
markValidation(); // the tool error was a validation failure
markNotFound(); // the tool error, or an invalid-params error, means "not found"
identify({ userId, tenant, handleId }); // merges into the operation's identity
```

| Option of `instrument` | Effect                                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------------------------------- |
| `capture`              | Content capture rules by tool glob, for example `[{ tool: "lookup_*", result: false, maxBytes: 4096 }]` |
| `handleParams`         | Per tool, the argument whose value is the session handle, for example `{ lookup_order: "order_id" }`    |
| `identify`             | Called once per inbound message with the transport's `authInfo` and HTTP request; returns an identity   |
| `identity: "none"`     | Sends no identity keys at all                                                                           |
| `transport`            | Overrides the transport label (`stdio`, `streamable_http`, `sse`, `unknown`)                            |

The MCP **Sessions** page groups operations by the session id of a session-based HTTP connection, else by session
handle, else by the trace of the calling client span, else by server process. So on stdio each value of a
`handleParams` argument is a session of its own, and the operations without a session handle form one **Process**
session, or follow the client's trace when an instrumented client made them.

Content capture is off by default. A tool call matching a `capture` rule is sampled (`sampleRate`, default 1)
before its arguments are read; arguments and results are sent as `gen_ai.tool.call.arguments` and
`gen_ai.tool.call.result`, scrubbed, and cut at `maxBytes` (default 16 KiB) on a character boundary; a cut never
keeps the prefix of a token it splits. Scrubbing applies the shared rules of `conformance/scrub-rules.json`
(packaged as `src/scrub-rules.json`, which a test keeps equal): PEM blocks, JWTs, Bearer tokens, Crucible secret
keys, card numbers (Luhn-checked), emails, IP addresses and phone numbers. Error messages and session handles are
scrubbed too. A `requestState` is never sent, in any form other than its keyed hash.

## MCP clients

```ts
import { instrumentClient } from "@elm-os/crucible/mcp-client";

const client = instrumentClient(new Client({ name: "trip-agent", version: "1.0.0" }));
await client.connect(transport);
```

Each request the client sends becomes a CLIENT span, a child of the active Crucible span, and carries that span's
`traceparent` and `tracestate` in `params._meta`. An instrumented server records its span as the client span's
child, so the agent's trace and the server's operation link up.

## Known limits

- A tool handler that throws and one that returns `isError: true` look the same on the wire; both are tool errors
  unless the handler calls `annotateError` or a mark. The error's message, the result's first text block, is sent
  scrubbed and cut at 2,048 bytes. Calls the MCP SDK refuses before a handler runs (bad arguments, an unknown tool)
  are caller faults with the class `validation` or `not_found`.
- Stateless legacy HTTP builds a fresh server per request, so tool calls there carry no client info. Server info
  is remembered per `instrument` call, so it reaches every operation after the first result.
- Exchanges recorded by `instrumentHandler` run no handler, so `annotateError` and `identify` cannot reach them,
  and they carry no server name; Crucible then uses the resource `service.name`.
- Headers named in `OTEL_EXPORTER_OTLP_HEADERS` other than `Authorization` are still sent with the export.
- `serveStdio` answers its `server/discover` probe and a few errors before any server instance exists; those are
  not recorded.
- Not observed: the client IP prefix, response sizes, the client capabilities hash, server-to-client requests and
  task results. Resource reads send the requested URI, not the registered template.
- The shared scrub rules match value patterns only; a secret under a key such as `password` is sent unless its
  value matches a rule.
- The test suite passes on Node.js 24 with MCP SDK 2.0.0 and 2.4.0; the seam spike also covered 2.1.0 to 2.3.1.
  Other runtimes are untested.

## Conformance

`conformance/runner.ts` is this SDK's runner for the shared SDK conformance suite
(`docs/testing/sdk-conformance.md`), and `conformance/manifest.json` its capability manifest: a thrown handler
error is a tool error, and progress is observed. `conformance/server.ts` is the suite's reference server. From the
repository root, after `cargo build -p crucible-fake-collector`:

```sh
target/debug/crucible-sdk-driver run --manifest sdk/typescript/conformance/manifest.json
```

## Development

```sh
pnpm install                                    # at the repository root
pnpm --filter @elm-os/crucible typecheck
pnpm --filter @elm-os/crucible test             # vitest against an in-process OTLP collector
pnpm --filter @elm-os/crucible run check:private
pnpm --filter @elm-os/crucible build
pnpm exec oxlint --deny-warnings sdk/typescript && pnpm exec biome format sdk/typescript
```

To use a build from a checkout in another project, pack it after the build, from the repository root, and install
the packed file there:

```sh
pnpm --filter @elm-os/crucible pack --pack-destination ..   # writes elm-os-crucible-<version>.tgz next to the checkout
npm install /path/to/elm-os-crucible-<version>.tgz         # in the other project
```
