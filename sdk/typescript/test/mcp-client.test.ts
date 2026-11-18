// The MCP client wrapper: CLIENT spans, and trace context injected into params._meta so that the server's span
// is the client span's child.

import { setTimeout as sleep } from "node:timers/promises";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import { McpServer } from "@modelcontextprotocol/server";
import { afterAll, beforeAll, beforeEach, expect, test } from "vitest";
import * as z from "zod/v4";
import { flush, init, observe, shutdown, withContext } from "../src/index.ts";
import { instrument } from "../src/mcp/index.ts";
import { instrumentClient } from "../src/mcp-client/index.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import { CLIENT, SERVER } from "./support/expect.ts";
import type { DecodedSpan } from "./support/otlp.ts";
import { buildServer, SERVER_NAME, SERVER_VERSION } from "./support/reference-server.ts";
import {
  CLIENT_NAME,
  CLIENT_VERSION,
  inMemoryPair,
  newClient,
  statelessHost,
} from "./support/scenarios.ts";

let collector: Collector;

beforeAll(async () => {
  collector = await startCollector();
  init({ endpoint: collector.url, secretKey: collector.key, flushIntervalMs: 10 });
});

afterAll(async () => {
  await shutdown();
  await collector.close();
});

beforeEach(async () => {
  await flush();
  collector.spans.length = 0;
});

function find(kind: number, tool: string): DecodedSpan {
  const span = collector.spans.find(
    (candidate) => candidate.kind === kind && candidate.attributes["gen_ai.tool.name"] === tool,
  );
  if (span === undefined) {
    throw new Error(`no span of kind ${kind} for ${tool}`);
  }
  return span;
}

test("a client call is a CLIENT span, and the server span is its child", async () => {
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(buildServer()).connect(serverSide);
  const client = instrumentClient(newClient("legacy"));
  await client.connect(clientSide);
  await withContext({ sessionId: "conversation-1" }, () =>
    observe("agent turn", { type: "agent" }, async () => {
      await client.callTool({ name: "echo", arguments: { text: "hi" } });
      await client.callTool({ name: "fail", arguments: {} });
      await client.callTool({ name: "missing", arguments: {} }).catch(() => undefined);
    }),
  );
  await client.close();
  await flush();
  const turn = collector.spans.find((span) => span.name === "agent turn");
  const call = find(CLIENT, "echo");
  const served = find(SERVER, "echo");
  expect(call.name).toBe("tools/call echo");
  expect(call.attributes).toEqual({
    "crucible.session.id": "conversation-1",
    "mcp.method.name": "tools/call",
    "crucible.mcp.capture_source": "sdk",
    "crucible.mcp.era": "legacy",
    "crucible.mcp.transport": "unknown",
    "jsonrpc.request.id": "1",
    "gen_ai.tool.name": "echo",
    "mcp.protocol.version": "2025-11-25",
    "crucible.mcp.client.name": CLIENT_NAME,
    "crucible.mcp.client.version": CLIENT_VERSION,
    "crucible.mcp.server.name": SERVER_NAME,
    "crucible.mcp.server.version": SERVER_VERSION,
    "crucible.mcp.result_type": "complete",
  });
  expect(call.traceId).toBe(turn?.traceId);
  expect(call.parentSpanId).toBe(turn?.spanId);
  expect(served.traceId).toBe(call.traceId);
  expect(served.parentSpanId).toBe(call.spanId);
  // The server span does not take the agent's context values; only the trace context crossed.
  expect(served.attributes["crucible.session.id"]).toBeUndefined();

  const failed = find(CLIENT, "fail");
  expect(failed.attributes["error.type"]).toBe("tool_error");
  expect(failed.status.code).toBe(2);
  const missing = find(CLIENT, "missing");
  expect(missing.attributes["rpc.response.status_code"]).toBe("-32602");
  expect(missing.attributes["error.type"]).toBe("-32602");
  expect(missing.status).toEqual({ code: 2, message: "Tool missing not found" });
  // initialize is a client operation too.
  expect(
    collector.spans.some(
      (span) => span.kind === CLIENT && span.attributes["mcp.method.name"] === "initialize",
    ),
  ).toBe(true);
});

test("tracestate travels on: a tool that calls another MCP server links all three spans", async () => {
  const [downstreamClient, downstreamServer] = inMemoryPair();
  await instrument(buildServer()).connect(downstreamServer);
  const relayClient = instrumentClient(newClient("legacy"));
  await relayClient.connect(downstreamClient);

  const relay = new McpServer({ name: "relay", version: "1.0.0" }, { capabilities: { tools: {} } });
  relay.registerTool(
    "relay",
    { description: "Calls echo downstream.", inputSchema: z.object({}) },
    async () => {
      await relayClient.callTool({ name: "echo", arguments: { text: "relayed" } });
      return { content: [{ type: "text", text: "done" }] };
    },
  );
  const [upstreamClient, upstreamServer] = inMemoryPair();
  await instrument(relay).connect(upstreamServer);
  await upstreamClient.start();
  const traceId = "4bf92f3577b34da6a3ce929d0e0e4736";
  await upstreamClient.send({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: {
      name: "relay",
      arguments: {},
      _meta: { traceparent: `00-${traceId}-00f067aa0ba902b7-01`, tracestate: "vendor=value" },
    },
  });
  await flush();
  await collector.waitFor((spans) =>
    spans.some((span) => span.kind === SERVER && span.attributes["gen_ai.tool.name"] === "relay"),
  );
  const relayed = find(SERVER, "relay");
  const call = find(CLIENT, "echo");
  const served = find(SERVER, "echo");
  expect([relayed.traceId, call.traceId, served.traceId]).toEqual([traceId, traceId, traceId]);
  expect(call.parentSpanId).toBe(relayed.spanId);
  expect(served.parentSpanId).toBe(call.spanId);
  expect([relayed.traceState, call.traceState, served.traceState]).toEqual([
    "vendor=value",
    "vendor=value",
    "vendor=value",
  ]);
  await relayClient.close();
  await upstreamServer.close();
});

test("over streamable HTTP the parent crosses the wire, and an aborted call is a stream close", async () => {
  const stateless = await statelessHost();
  const client = instrumentClient(newClient("auto"));
  await client.connect(new StreamableHTTPClientTransport(new URL(stateless.url)));
  await client.callTool({ name: "echo", arguments: { text: "hi" } });
  const abort = new AbortController();
  const pending = client
    .callTool({ name: "slow", arguments: { ms: 300, tag: "gone" } }, { signal: abort.signal })
    .catch(() => undefined);
  await sleep(50);
  abort.abort();
  await pending;
  await client.close();
  await stateless.close();
  await flush();
  const call = find(CLIENT, "echo");
  const served = find(SERVER, "echo");
  expect(call.attributes["crucible.mcp.transport"]).toBe("streamable_http");
  expect(call.attributes["crucible.mcp.era"]).toBe("modern");
  expect(call.attributes["mcp.protocol.version"]).toBe("2026-07-28");
  expect(served.parentSpanId).toBe(call.spanId);
  expect(served.traceId).toBe(call.traceId);
  const aborted = find(CLIENT, "slow");
  expect(aborted.attributes["crucible.mcp.cancel.source"]).toBe("stream_close");
});

test("a legacy cancel notification ends the client span as cancelled", async () => {
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(buildServer()).connect(serverSide);
  const client = instrumentClient(newClient("legacy"));
  await client.connect(clientSide);
  const abort = new AbortController();
  const pending = client
    .callTool({ name: "slow", arguments: { ms: 300, tag: "gone" } }, { signal: abort.signal })
    .catch(() => undefined);
  await sleep(50);
  abort.abort();
  await pending;
  await client.close();
  await flush();
  expect(find(CLIENT, "slow").attributes["crucible.mcp.cancel.source"]).toBe("notification");
  expect(find(SERVER, "slow").attributes["crucible.mcp.cancel.source"]).toBe("notification");
});
