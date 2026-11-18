# TypeScript SDK

`@elm-os/crucible` sends traces from Node.js agents and MCP servers to Crucible over OTLP/HTTP. It has three entry
points:

| Import | Contents |
|---|---|
| `@elm-os/crucible` | `init`, `flush`, `shutdown` and `stats`; the agent helpers `withContext` and `observe`; `feedback`, which is planned |
| `@elm-os/crucible/mcp` | `instrument` and `instrumentHandler` for MCP servers; `annotateError`, `markValidation`, `markNotFound` and `identify` for tool handlers |
| `@elm-os/crucible/mcp-client` | `instrumentClient` for MCP clients |

It needs Node.js 24 or newer. The MCP entry points work with the 2.x line of the official TypeScript MCP SDK
(`@modelcontextprotocol/server` and `@modelcontextprotocol/client`, from 2.0.0 up to but not including 3.0.0).
Servers on the 1.x line are designed to use the Crucible proxy, which is planned.

## Install

The package is not on npm yet; it will be published as `@elm-os/crucible`. Until then, build it from a checkout of
the repository and install the packed file. The build needs Node.js 24 and pnpm 11:

```sh
git clone https://github.com/elm-os/crucible.git
cd crucible
pnpm install --frozen-lockfile
pnpm --filter @elm-os/crucible build
pnpm --filter @elm-os/crucible pack --pack-destination ..
```

That writes `elm-os-crucible-0.0.0.tgz` next to the checkout. In your project:

```sh
npm install ../elm-os-crucible-0.0.0.tgz
npm install @modelcontextprotocol/server@^2 @modelcontextprotocol/client@^2 zod@^4   # for the MCP examples
```

The examples on this page are complete ES module files. Each was run with `node <file>.ts` in such a project, with
`"type": "module"` in its `package.json` and MCP SDK 2.4.0, and with only the endpoint changed to the SDK's test
collector; they also pass `tsc --noEmit` in strict mode.

## Set up

```ts
// hello.ts
import { flush, init, observe, shutdown, stats } from "@elm-os/crucible";

init({
  endpoint: "http://localhost:4318",
  secretKey: process.env.CRUCIBLE_SECRET_KEY,
  serviceName: "hello-agent",
});

const greeting = await observe("say hello", async () => "hello");
console.log(greeting);

await flush();
console.log(stats());
await shutdown();
```

It prints `hello` and then `{ queued: 0, dropped: 0, exported: 1, failed: 0 }`.

`init` starts the exporter. Spans go to `POST <endpoint>/v1/traces` as OTLP/HTTP protobuf with
`Authorization: Bearer <secretKey>`.

| Option | Default | Effect |
|---|---|---|
| `endpoint` | `CRUCIBLE_ENDPOINT`, else `http://localhost:4318` | Base URL of an OTLP listener; `/v1/traces` is added unless the URL ends with it |
| `secretKey` | `CRUCIBLE_SECRET_KEY` | Required. The project's secret key, sent as a Bearer token |
| `serviceName`, `serviceVersion` | | Resource `service.name` and `service.version` |
| `environment` | | Resource `deployment.environment.name` |
| `release` | | `crucible.release` on every span |
| `deployId` | | Resource `crucible.deploy.id`; without it, Crucible is designed to use the server version as the deploy id |
| `queueSize` | 2,048 | Spans the export queue holds, the batch being sent included |
| `flushIntervalMs` | 1,000 | Longest wait before a partial batch is sent |
| `exportTimeoutMs` | 10,000 | Bound on one export, and on `flush` and `shutdown` |

`init` throws, with a message that names the option or variable to set, when the key is missing, when the endpoint
is not an `http` or `https` URL, when a number option is not a positive integer, and when it was already called;
call `shutdown` before calling `init` again. Each `init` gives the process a new random `service.instance.id`.

The SDK uses its own tracer provider and never registers it globally, so an application's own OpenTelemetry setup
is left alone. When the application has a context manager registered, Crucible spans nest under the active span and
the application's spans nest under Crucible spans. `OTEL_*` variables cannot redirect the export, replace its key
or turn its sampling off: sampling is always on.

- `flush()` exports every queued span. It resolves within `exportTimeoutMs`, whether or not the collector answers,
  and never rejects.
- `shutdown()` flushes and then stops the exporter. Spans that end after it are dropped and counted. Call it before
  the process exits, for example on `SIGTERM`:

```ts
// shutdown.ts
import { init, shutdown } from "@elm-os/crucible";

init({ serviceName: "trip-agent" });

process.once("SIGTERM", async () => {
  await shutdown(); // sends what is queued, within exportTimeoutMs
  process.exit(0);
});
```

## Trace an agent

```ts
// agent.ts
import { init, observe, shutdown, withContext } from "@elm-os/crucible";

init({ serviceName: "trip-agent", serviceVersion: "1.4.0", environment: "staging" });

// Stands in for your model client.
async function callModel(question: string): Promise<{ text: string }> {
  return { text: `For "${question}": Lisbon` };
}

const answer = await withContext(
  { sessionId: "conv-42", userId: "user-7", tags: ["beta"], metadata: { plan: "pro" } },
  () =>
    observe("plan trip", { type: "agent", input: "Where should I go in May?" }, async (agent) => {
      agent.update({ traceName: "trip planner" });
      agent.span.setAttribute("crucible.trace.tags", ["beta"]);
      const reply = await observe("chat", { type: "generation" }, async (generation) => {
        const result = await callModel("Where should I go in May?");
        generation.update({
          model: "model-large",
          responseModel: "model-large-2026-05",
          provider: "acme",
          usage: { input: 1200, output: 300, cacheRead: 1000 },
          cost: { total: "0.0042" },
          prompt: { name: "trip-planner", version: 3 },
          output: result.text,
        });
        return result;
      });
      agent.update({ output: reply.text });
      return reply.text;
    }),
);
console.log(answer);

await shutdown();
```

This `init` reads the endpoint and the key from `CRUCIBLE_ENDPOINT` and `CRUCIBLE_SECRET_KEY`.

- `withContext(values, fn)` runs `fn` with values that every span started inside it carries, across `await`:
  `sessionId` as `crucible.session.id`, `userId` as `crucible.user.id`, `tags` as `crucible.tags` and each
  `metadata` entry as `crucible.metadata.<key>`. A nested `withContext` adds tags and overrides the other values.
- `observe(name, options?, fn)` runs `fn` in a new span and ends the span when `fn` returns or its promise
  settles. A throw or a rejection marks the span as an error, records the exception and is passed on unchanged.
  `type` is sent as `crucible.observation.type` and `input` as `crucible.input`. Without a type, Crucible infers
  one from the span's attributes, else `span`.
- `update(fields)` on the observation sets fields on its span:

| Field | Key |
|---|---|
| `traceName` | `crucible.trace.name` |
| `input`, `output` | `crucible.input`, `crucible.output`: a string as it is, any other value as JSON |
| `inputMessages`, `outputMessages` | `gen_ai.input.messages`, `gen_ai.output.messages`, as JSON |
| `finishReasons` | `gen_ai.response.finish_reasons` |
| `model`, `responseModel` | `gen_ai.request.model`, `gen_ai.response.model` |
| `provider` | `gen_ai.provider.name` |
| `usage` | `gen_ai.usage.input_tokens`, `.output_tokens`, `.cache_read.input_tokens`, `.cache_creation.input_tokens` and `.reasoning.output_tokens` from `input`, `output`, `cacheRead`, `cacheWrite` and `reasoning`. Cache and reasoning counts are parts of the input and output counts |
| `cost` | `crucible.cost.<key>` in USD for `total` or a usage key; a string keeps decimal precision. A provided cost replaces the one Crucible would calculate |
| `prompt` | `crucible.prompt.name` and `crucible.prompt.version` |

- The observation's `span` is the OpenTelemetry span, for keys the helpers do not set, such as
  `crucible.trace.tags` and `crucible.level`.

[Sending traces](sending-traces.md#attributes-the-trace-views-read) lists which of these keys the trace views show
on main. Two notes for agents: the views show the model from `responseModel`, while `model` alone prices a span but
is not shown; and the trace list shows the trace tags, `crucible.trace.tags`, not the span tags that `withContext`
sets.

`feedback()` is planned: it will post a score to the project's feedback route. The API has no feedback route yet,
so in this release the function rejects without sending anything.

## Instrument an MCP server

```ts
// server.ts
import { McpServer } from "@modelcontextprotocol/server";
import { StdioServerTransport } from "@modelcontextprotocol/server/stdio";
import { init, shutdown } from "@elm-os/crucible";
import { annotateError, identify, instrument, markNotFound } from "@elm-os/crucible/mcp";
import * as z from "zod/v4";

init({ serviceName: "orders-mcp", serviceVersion: "2.1.0" });
process.stdin.on("end", () => void shutdown()); // export what is queued when the client goes away

const orders = new Map([["A-1001", "shipped"]]);
const server = new McpServer({ name: "orders-mcp", version: "2.1.0" });

server.registerTool(
  "lookup_order",
  { description: "Looks an order up by id.", inputSchema: z.object({ order_id: z.string() }) },
  async ({ order_id }) => {
    identify({ userId: "user-7", tenant: "acme" });
    const status = orders.get(order_id);
    if (status === undefined) {
      markNotFound();
      return { content: [{ type: "text", text: `no order ${order_id}` }], isError: true };
    }
    return { content: [{ type: "text", text: `order ${order_id}: ${status}` }] };
  },
);

server.registerTool(
  "refund",
  {
    description: "Refunds an order to a card.",
    inputSchema: z.object({ order_id: z.string(), card: z.string() }),
  },
  async ({ order_id }) => {
    annotateError("upstream");
    return {
      content: [{ type: "text", text: `the payment service refused ${order_id}` }],
      isError: true,
    };
  },
);

await instrument(server, {
  capture: [{ tool: "refund", maxBytes: 4096 }],
  handleParams: { lookup_order: "order_id", refund: "order_id" },
}).connect(new StdioServerTransport());
```

`instrument(server, options?)` returns the server it was given. It replaces the public `connect` of the instance,
and of an `McpServer`'s inner server, and taps the transport it is connected to in place; it reads no private
member of the MCP SDK. It takes three forms:

- An instance with a transport you connect yourself: stdio, as above, or a session-based HTTP transport.
- A factory for `serveStdio`: `serveStdio(instrument(buildServer))`.
- A factory for `createMcpHandler`, which builds a server per HTTP request, together with `instrumentHandler`:

```ts
// http.ts
import { createMcpHandler, McpServer } from "@modelcontextprotocol/server";
import { init } from "@elm-os/crucible";
import { instrument, instrumentHandler } from "@elm-os/crucible/mcp";
import * as z from "zod/v4";

init({ serviceName: "orders-mcp", serviceVersion: "2.1.0" });

function buildServer(): McpServer {
  const server = new McpServer({ name: "orders-mcp", version: "2.1.0" });
  server.registerTool(
    "lookup_order",
    { description: "Looks an order up by id.", inputSchema: z.object({ order_id: z.string() }) },
    async ({ order_id }) => ({ content: [{ type: "text", text: `order ${order_id}: shipped` }] }),
  );
  return server;
}

// Serve `handler.fetch` from any host of fetch-style handlers.
export const handler = instrumentHandler(createMcpHandler(instrument(buildServer)));
```

`instrumentHandler` records the HTTP exchanges that `createMcpHandler` answers before any server instance exists,
such as an unsupported protocol revision, a wrong content type or `subscriptions/listen`. If a transport cannot be
tapped, the server runs uninstrumented.

Each request and notification the server receives becomes one SERVER span named after its method and target, such
as `tools/call lookup_order`, with `crucible.mcp.capture_source = sdk` and the keys of
[contract 9.1](../architecture/contracts.md#91-wire-attribute-contract): method and target, JSON-RPC id, protocol
era and version, transport, client and server name and version, progress count, the error evidence that the
classifier of [contract 9.2](../architecture/contracts.md#92-error-taxonomy) reads, the legacy session id hashed,
the multi-round state hashes, the request size and user agent on HTTP, and the number of content blocks of each
type. A `traceparent` in `params._meta` makes the span a child of the caller's span; baggage is never read.

On main, Crucible stores these spans as spans: the trace view shows a `tools/call` span as a `tool` span and a
tool error with status `error`, and the MCP keys, with any captured content, stay among the span's attributes
(the payload route's `attributes` field). Recording them as MCP operations, and the MCP analytics routes and
pages that read them, are planned.

| Option of `instrument` | Effect |
|---|---|
| `capture` | Content capture rules by tool glob; see below |
| `handleParams` | Per tool, the argument whose string value is the operation's handle id, for example `{ lookup_order: "order_id" }`. It is scrubbed and cut at 256 bytes, and it groups the operations that work on one thing |
| `identify` | Called once per inbound message with the transport's `authInfo` and HTTP request; returns an identity |
| `identity: "none"` | Sends no identity keys at all, the OAuth client id included |
| `transport` | Overrides the transport label: `stdio`, `streamable_http`, `sse` or `unknown` |

### Errors and identity in tool handlers

Inside a tool handler these calls refine the operation being handled; outside one they do nothing:

| Call | Effect |
|---|---|
| `annotateError(subclass)` | Refines a tool error (`isError: true`) with `auth`, `rate_limit`, `upstream` or `business` (`crucible.mcp.error.subclass`) |
| `markValidation()` | The tool error is a validation failure |
| `markNotFound()` | The tool error, or an invalid-params error (-32602), means "not found" |
| `identify({ userId, tenant, issuer, subject, handleId })` | Merges into the operation's identity (`user.id`, `crucible.mcp.tenant`, `crucible.mcp.user.issuer`, `crucible.mcp.user.subject`, `crucible.mcp.handle.id`) |

An identity from the `identify` option or call is sent with `crucible.mcp.identity.source = identify_hook`. Without
one, a request whose transport carries `authInfo`, from a token the hosting server verified, is sent with
`token_verified` and the token's client id. Crucible is designed to hash the subject with a per-project salt and
keep only the hash and the issuer; the MCP views that would show identities are planned.

A tool handler that throws and one that returns `isError: true` look the same on the wire: both are tool errors
unless the handler calls `annotateError` or a mark.

### Content capture and scrubbing

MCP metadata is always sent; arguments and results are not, unless a `capture` rule matches the tool. A rule is
`{ tool, args, result, sampleRate, maxBytes }`: `tool` is a glob where `*` matches any run of characters and `?`
one character (default `*`), `args` and `result` default to true, `sampleRate` (0 to 1, default 1) is decided
before anything is copied, and `maxBytes` defaults to 16,384. The first matching rule applies.

Arguments go out as `gen_ai.tool.call.arguments` and results as `gen_ai.tool.call.result`, as JSON, scrubbed and cut
at `maxBytes` on a character boundary, with `crucible.mcp.content.captured`, `.content.sample_rate` and
`.content.truncation` (`cap_exceeded` when a field was cut). Scrubbing sees 1 KiB past the cap, so a secret that
straddles the cap is still matched, and a cut never keeps the prefix of a token it splits.

The scrub rules are the shared list of `conformance/scrub-rules.json`, packaged as `src/scrub-rules.json`, which a test
keeps equal: PEM blocks, JWTs, Bearer tokens, Crucible secret keys, card numbers that pass the Luhn check, email
addresses, IP addresses and phone numbers. Each match is replaced with a marker such as `[redacted:card]`. Error
messages and handle ids are scrubbed with the same rules. The rules match values only, so a secret under a key such as
`password` is sent unless its value matches a rule. A `requestState` is never sent in any form other than its keyed
hash.

## Instrument an MCP client

```ts
// client.ts
import { Client } from "@modelcontextprotocol/client";
import { getDefaultEnvironment, StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import { init, observe, shutdown, withContext } from "@elm-os/crucible";
import { instrumentClient } from "@elm-os/crucible/mcp-client";

init({ serviceName: "support-agent" });

const client = instrumentClient(new Client({ name: "support-agent", version: "1.0.0" }), {
  transport: "stdio",
});
// The stdio transport starts the server with a minimal environment, so pass the Crucible settings on.
await client.connect(
  new StdioClientTransport({
    command: "node",
    args: ["server.ts"],
    env: {
      ...getDefaultEnvironment(),
      CRUCIBLE_ENDPOINT: process.env.CRUCIBLE_ENDPOINT ?? "http://localhost:4318",
      CRUCIBLE_SECRET_KEY: process.env.CRUCIBLE_SECRET_KEY ?? "",
    },
  }),
);

await withContext({ sessionId: "conv-42" }, () =>
  observe("handle refund request", { type: "agent" }, async () => {
    const order = await client.callTool({
      name: "lookup_order",
      arguments: { order_id: "A-1001" },
    });
    const refund = await client.callTool({
      name: "refund",
      arguments: { order_id: "A-1001", card: "4111 1111 1111 1111" },
    });
    console.log(order.content, refund.isError);
  }),
);

await client.close();
await shutdown();
```

`instrumentClient(client, options?)` returns the client it was given. Each request the client sends becomes a
CLIENT span, a child of the active Crucible span, and carries that span's `traceparent` and `tracestate` in
`params._meta`. An instrumented server records its span as the client span's child, so the agent's trace holds the
server's handling of each call. In this example the trace has the agent span, a CLIENT span per call and, under
each, the server's SERVER span. The `transport` option sets the label, which is otherwise `streamable_http` for a
per-request HTTP transport and `unknown` for anything else.

## The export queue

Export runs off the request path. A span that ends goes on a bounded queue; a background loop sends it in batches
of up to 512 spans, as soon as a batch is full or `flushIntervalMs` after the first span of a partial batch, with
one export in flight at a time. When the queue holds `queueSize` spans, the batch in flight included, a span that
ends is dropped and counted. Ending a span never blocks, whether the collector is up, refusing connections or not
answering.

Within `exportTimeoutMs` the OpenTelemetry exporter the SDK uses retries a refused export, such as a 503 with
`Retry-After`, when the server's delay fits in the time left. A batch that still fails, or that does not finish in
time, counts as failed and is not sent again.

`stats()` returns the counters of the exporter that `init` started. They are all zero before `init` and again after
`shutdown`, so read them before you shut down:

| Counter | Meaning |
|---|---|
| `queued` | Spans waiting for export or being exported |
| `dropped` | Spans dropped because the queue was full, or because they ended after `shutdown` |
| `exported` | Spans the collector accepted |
| `failed` | Spans in batches whose export failed or timed out |

## Configuration

| Variable | Used for |
|---|---|
| `CRUCIBLE_ENDPOINT` | `endpoint` when the option is not given |
| `CRUCIBLE_SECRET_KEY` | `secretKey` when the option is not given |

Every other setting is an option of `init` or `instrument`. Each export carries
`User-Agent: crucible-sdk-typescript/<version>`.

## Known limits

- Stateless legacy HTTP builds a fresh server per request, so tool calls there carry no client info. Server info is
  remembered per `instrument` call, so it reaches every operation after the first result.
- Exchanges that `instrumentHandler` records run no handler, so `annotateError` and `identify` cannot reach them,
  and they carry no server name; Crucible is designed to use the resource `service.name` instead.
- Headers named in `OTEL_EXPORTER_OTLP_HEADERS`, other than `Authorization`, are still sent with each export.
- `serveStdio` answers its `server/discover` probe and a few errors before any server instance exists; those are
  not recorded.
- Not observed: the client IP prefix, response sizes, the client capabilities hash, requests from the server to the
  client, and task results. Resource reads send the requested URI, not the registered template.

## Conformance

The SDK passes the shared [SDK conformance suite](../testing/sdk-conformance.md), agent and MCP cases, in both
protocol eras over stdio and streamable HTTP, with no skipped case. Its capability manifest,
`sdk/typescript/conformance/manifest.json`, declares that a tool handler's exception becomes a tool error and that
progress notifications are counted. CI runs the suite (`.github/workflows/sdk-conformance.yml`) on every pull
request that touches the SDKs or the suite. To run it locally from the repository root, with the dev container's
toolchain for the driver:

```sh
cargo build -p crucible-fake-collector
target/debug/crucible-sdk-driver run --manifest sdk/typescript/conformance/manifest.json
```

CI runs the SDK's own tests (`.github/workflows/ci-sdk-ts.yml`) on Node.js 24 with MCP SDK 2.4.0; other versions
and runtimes are not tested in CI.
