# MCP TypeScript seam: decision record

Status: decided by a spike on 2026-11-17. The spike lives in `sdk/typescript/spikes/mcp-seam/` and is not part of
any published package. The SDK's `@elm-os/crucible/mcp` entry point implements the decision; the API sketch below is
the design it started from, and the [TypeScript SDK guide](../guides/sdk-typescript.md#instrument-an-mcp-server)
describes the API as it is.

## Question

The Crucible TypeScript SDK instruments MCP servers built on the official TypeScript MCP SDK
(`@modelcontextprotocol/server`, 2.x line). That SDK has no public request middleware. This record decides where
Crucible attaches, so that every request and notification becomes one operation with method, target, JSON-RPC id,
timing, outcome class, era and client info, without reading private members of the SDK.

## Decision

Use both candidates, each for the part it can see:

1. **Connect decoration (primary).** `instrument(server)` replaces the public `connect` method on the server
   instance. When the SDK, or user code, connects a transport, Crucible taps three public members of the
   `Transport` interface on that same transport object: `onmessage`, `send` and `onclose`. The factory form is
   `ctx => instrument(await factory(ctx), { era: ctx.era })`.
2. **Fetch wrap (companion).** `instrumentHandler(handler)` wraps the `fetch` face of the object returned by
   `createMcpHandler`. It records an operation only when the connect decoration saw nothing for that HTTP exchange.

The fallback from the design (transport decoration only, plus the Crucible proxy for modern HTTP) is not needed.

## Evidence from the SDK source

Read at tag `v2.4.0` (`packages/server/src/server/`):

- **All three serving entries call the public `connect` on the instance the factory returned.** The modern HTTP path
  does it in `invoke.ts` line 66, the stateless legacy HTTP path in `createMcpHandler.ts` line 352, and `serveStdio`
  in `serveStdio.ts` line 531. The design had this as an inference. It is now confirmed by source and by the driver.
- **The transport object must stay the same object.** `Server.connect` checks the class of the transport
  (`server.ts` line 370, `perRequestTransport.ts` line 454) to bind the modern revision, and the entries keep their
  own reference to the transport they built and call `handleMessage` or `handleRequest` on it. A separate wrapper
  object would lose both. The spike therefore taps the given transport in place.
- **The tap cannot be raced.** `Protocol.connect` (`core-internal/src/shared/protocol.ts` lines 794 to 829) assigns
  `onclose`, `onerror` and `onmessage`, then calls `start()`. The spike installs accessors for `onmessage` and
  `onclose` before it calls the original `connect`, so the SDK's own assignment goes through the tap and no inbound
  message can be dispatched around it.
- **Not every transport keeps its callbacks in plain fields.** The Node adapter's Streamable HTTP transport
  (`packages/middleware/node/src/streamableHttp.ts` lines 114 to 142) forwards `onclose`, `onerror` and
  `onmessage` to an inner transport through accessors on its class. An accessor placed on the instance would hide
  that forwarding and the server would stop answering. The tap therefore looks for an existing accessor on the
  class chain and keeps it in the path: it hands the wrapped callback to the original setter and reads through the
  original getter. The driver runs this transport as its own scenario.
- **Some modern HTTP exchanges never reach `connect`.** In `createMcpHandler.ts` these return first: wrong content
  type (line 957), body too large (966), classification rejection (982), unsupported revision (735), standard header
  rejection (756), missing client capability (778), `subscriptions/listen` (814), parameter header rejection (835)
  and scope challenge (856). Only a wrap of the fetch face can see them.

## What the spike built

| File | Purpose |
|---|---|
| `src/instrument.ts` | `instrument`, `instrumentFactory`, `instrumentHandler`, `annotateError`, `identify` |
| `src/reference-server.ts` | Reference server with tools `echo`, `fail`, `throw`, `slow` |
| `src/stdio-server.ts` | Child process for the stdio scenarios (`connect` or `serveStdio`) |
| `src/driver.ts` | Runs every scenario with the official client and checks the recorded operations |
| `src/bench.ts` | Paired latency measurement |
| `scripts/check-private.mjs` | Scan that bans private member access and type escapes, with a self-test |
| `scripts/matrix.mjs` | Runs the typecheck and the driver against several 2.x releases |

Inside the tap, the SDK's dispatch function runs inside `AsyncLocalStorage.run(operation, ...)`. Responses are
matched to requests by JSON-RPC id through a pending map per connection, because session transports multiplex.
Era comes from the per-message classification, else from the presence of the protocol version key in
`params._meta`, else from the factory context. Client info comes from `params._meta` in the modern era and from the
observed `initialize` request in the legacy era. Only exported constants and types of the SDK are imported.

## Result per exit criterion

| # | Criterion | Result | Evidence |
|---|---|---|---|
| 1 | All transports and both eras observed | Pass | Driver: 158 checks pass on 2.4.0 across ten scenarios (below) |
| 2 | No private member access, enforced | Pass | `tsc --noEmit` in strict mode is clean; `check-private.mjs` reports 0 findings in 7 files and its self-test catches 8 planted violations |
| 3 | `annotateError` and `identify` reach the active operation from a tool handler | Pass | In every scenario two overlapping `slow` calls each tag their own operation after their wait; `fail` carries its annotation and `throw` carries none |
| 4 | Added p99 at most 1 ms | Pass | Largest added p99 on a gated path: 0.0376 ms (below) |
| 5 | Works across 2.x versions | Pass | 2.0.0, 2.1.0, 2.2.0, 2.3.1 and 2.4.0: typecheck clean and 158 of 158 driver checks each, with the client at the same version and the Node adapter at 2.0.0, 2.1.0, 2.1.0, 2.1.1 and 2.1.2 |

Scenarios the driver runs, each with one `tools/list`, one call per tool, two overlapping `slow` calls and one call
to an unknown tool:

| Scenario | Entry | Era | Seen by |
|---|---|---|---|
| `stdio-connect-legacy` | `StdioServerTransport` plus `connect` | legacy | connect decoration |
| `stdio-serve-legacy` | `serveStdio(factory)` | legacy | connect decoration |
| `stdio-serve-modern` | `serveStdio(factory)` | modern | connect decoration |
| `http-session-legacy` | Session-based Streamable HTTP transport plus `connect` | legacy | connect decoration |
| `http-node-session-legacy` | Node adapter transport (`@modelcontextprotocol/node`) plus `connect` | legacy | connect decoration |
| `http-stateless-modern` | `createMcpHandler(factory)` | modern | connect decoration |
| `http-stateless-legacy` | `createMcpHandler(factory)`, stateless legacy leg | legacy | connect decoration; the refused GET stream (405) only by the fetch wrap |
| `http-modern-edge` | Raw requests to the handler | modern | see next table |

What each candidate records for exchanges at the HTTP edge:

| Exchange | HTTP | Connect decoration | Fetch wrap |
|---|---|---|---|
| Served `tools/call` | 200 | 1 operation | 0 (no double count) |
| Served `tools/call` with `responseMode: "sse"` | 200, event stream | 1 operation | 0 |
| Unsupported revision | 400 | 0 | 1, `protocol_error`, code -32022, with method, target and id |
| Wrong content type | 415 | 0 | 1, `protocol_error`, code -32000 |
| `subscriptions/listen` | 200, event stream | 0 | 1, `ok` at stream open |

Two more scenarios cover requests that never get a response. In `in-memory-cancel` the client cancels a call. The
SDK sends no response for a cancelled request, so the tap finishes the operation when the
`notifications/cancelled` message arrives: `protocol_error` with error type `cancelled`, recorded once even though
the handler runs on. In `in-memory-abandon` the transport closes with a call in flight, and the operation is
recorded once as `protocol_error` with error type `connection_closed`.

The criterion in the design reads "all conformance transcripts pass". The shared conformance suite does not exist
yet, so the driver's fixed workload stands in for it. Re-run this criterion against the transcripts when they land.

## Latency

Measured on Node 24.19.0, Windows x64, SDK 2.4.0. The tool returns a constant. Each variant ran 100,000 calls after
a 2,000-call warm-up, in 40 rounds that alternate between the variants. The sink is a bounded in-memory queue that
drops when full, which stands in for the export queue.

| Path | Variant | p50 ms | p99 ms | Added p50 ms | Added p99 ms |
|---|---|---|---|---|---|
| In-memory transport, long-lived connection | baseline | 0.0106 | 0.0215 | | |
| | connect decoration | 0.0110 | 0.0217 | 0.0004 | 0.0002 |
| Stateless handler in process, modern era | baseline | 0.1084 | 0.3610 | | |
| | connect decoration | 0.1174 | 0.3839 | 0.0090 | 0.0229 |
| | connect decoration plus fetch wrap | 0.1261 | 0.3986 | 0.0177 | 0.0376 |
| Stateless handler over loopback HTTP (2,000 calls, informational) | baseline | 15.5445 | 17.2991 | | |
| | connect decoration plus fetch wrap | 15.5524 | 17.2192 | 0.0079 | -0.0799 |

An earlier full run of the same benchmark, before the accessor and cancel handling were added, gave added p99
values of 0.0088, 0.0375 and 0.0290 ms on the three gated rows. Absolute times moved between the runs with machine
load; the added latency stayed two orders of magnitude under the gate.

The loopback run is informational: each call takes about 15 ms on this host, so socket timing hides an effect of
this size. Not covered by the spike, and still required by the design gate: the three collector states (healthy,
refused, never answering) and content capture at the default cap. Both belong to the exporter, which the spike does
not include.

## Gaps and risks found

- **A thrown handler and a returned tool error look the same.** The SDK turns a thrown error into a result with
  `isError: true` (`mcp.ts` line 320), so both `fail` and `throw` are `tool_error` on the wire. Telling them apart
  needs a handler wrap, which has no public hook, or an `annotateError` call by the author. The capability manifest
  for TypeScript should list this.
- **No client info on stateless legacy tool calls.** Each legacy request gets a fresh server, so the `initialize`
  that carried the client info is a different exchange. The driver asserts the field is absent there.
- **Edge operations have no handler context.** `annotateError` and `identify` cannot reach them, because no handler
  runs.
- **`serveStdio` answers some errors itself.** It writes a few error responses straight to the wire before any
  instance exists (for example an unsupported revision, `serveStdio.ts` lines 400 and 459). The connect decoration
  does not see those. `serveStdio` accepts a `transport` option, so the production SDK can tap that wire transport
  with the same code. The spike did not build or test this.
- **Instrumenting an `McpServer` does not cover its inner `server`.** If user code calls `mcp.server.connect(...)`
  directly, the decoration on the outer object is bypassed. The production SDK should decorate both; the tap already
  refuses to tap a transport twice.
- **The seam depends on two behaviours that are public but not promised.** The entries call `connect` through the
  instance, and the SDK assigns `onmessage` as a plain property. Both hold from 2.0.0 to 2.4.0. A weekly run of
  `scripts/matrix.mjs` against the newest 2.x will catch a change. The production code must catch a failed tap and
  leave the server uninstrumented instead of failing `connect`.
- **Modern HTTP cancellation is a closed stream, not a notification.** The tap then reports the operation through
  the close path as `connection_closed`. The driver covers the close path only on the in-memory transport.
- **Progress counting is implemented but not exercised** by the driver. Request and response sizes are not measured.
- **Only Node 24 was tested.** The module imports `node:async_hooks`. Other runtimes are untested.

## Fallback

If a later 2.x release stops calling `connect` through the instance, or makes the transport members
non-configurable, the fallback is the one in the design: keep the transport tap for stdio and session HTTP, where
the user passes the transport to `connect` and Crucible can offer an explicit `instrumentTransport(transport)`, and
document the Crucible proxy for modern HTTP. The fetch wrap can also be widened to parse every JSON and event
stream response, at the cost of losing handler context for `annotateError` and `identify`.

## API sketch for the production SDK

The design for the `./mcp` subpath of `@elm-os/crucible`, with `@modelcontextprotocol/server` as an optional peer.
The peer range can be `>=2.0.0 <3` on this evidence; the design asked for `>=2.4`.

```ts
import { annotateError, identify, instrument } from "@elm-os/crucible/mcp";

// One entry point, three overloads.
instrument(server, options?);   // McpServer or Server: decorates connect on it and on its inner server; returns it
instrument(factory, options?);  // McpServerFactory: returns a factory for createMcpHandler and serveStdio
instrument(handler, options?);  // result of createMcpHandler: returns a copy whose fetch adds the edge operations

// Typical stateless HTTP server
const handler = instrument(createMcpHandler(instrument(buildServer)));

// Typical stdio server
await instrument(buildServer()).connect(new StdioServerTransport());

// Inside a tool handler
annotateError("upstream");            // also markValidation(), markNotFound()
identify({ userId, tenant });         // merges into the active operation
```

Options will follow the common surface in the design: `capture` rules, `identify(extra)` called once per inbound
message with the transport's `authInfo` and HTTP request, `transport` label override, and `measureSizes`.

Mapping from the spike's operation to the wire contract:

| Spike field | Span key |
|---|---|
| `method`, `target` | `mcp.method.name`; `gen_ai.tool.name`, `gen_ai.prompt.name` or the resource key |
| `jsonrpcId` | `jsonrpc.request.id` |
| `era`, `protocolVersion` | `crucible.mcp.era`, `mcp.protocol.version` |
| `outcome`, `errorCode`, `errorType`, `errorSubclass` | `crucible.mcp.error.class`, `rpc.response.status_code`, `error.type`, `crucible.mcp.error.subclass` |
| `clientName`, `clientVersion` | `crucible.mcp.client.name`, `crucible.mcp.client.version` |
| `sessionId` | `mcp.session.id` (legacy only) |
| `transport` | `crucible.mcp.transport` |
| `httpStatus` | `http.response.status_code` |
| `progressCount` | `crucible.mcp.progress.count` |
| `identity` | `user.id`, `crucible.mcp.tenant`, `crucible.mcp.user.issuer`, `crucible.mcp.user.subject`, `crucible.mcp.handle.id` |
| `source` | No key in the contract yet. One is needed if edge operations are to be told apart |

In the production SDK the stored value will be the active span, set through the OpenTelemetry context, which
itself rides on `AsyncLocalStorage` in Node. The spike shows that this context survives from the transport callback
to the tool handler on every path.

The lint rule for criterion 2 in the production package will be Oxlint `no-underscore-dangle` (allowing only the
wire field `_meta`) and `typescript/no-explicit-any` scoped to `./mcp`, plus the scan from the spike in CI.

## How to reproduce

```sh
cd sdk/typescript/spikes/mcp-seam
pnpm install --ignore-workspace
pnpm typecheck
node scripts/check-private.mjs selftest && node scripts/check-private.mjs
node src/driver.ts
node src/bench.ts
node scripts/matrix.mjs
```
