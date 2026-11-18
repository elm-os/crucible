// The seam never breaks the server: a transport that cannot be tapped stays uninstrumented, instrumenting twice
// records once, and the inner server of an McpServer is covered too.

import type { Transport } from "@modelcontextprotocol/server";
import { afterAll, beforeAll, beforeEach, expect, test } from "vitest";
import { flush, init, shutdown } from "../src/index.ts";
import { instrument } from "../src/mcp/index.ts";
import { instrumentClient } from "../src/mcp-client/index.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import { buildServer } from "./support/reference-server.ts";
import { inMemoryPair, newClient } from "./support/scenarios.ts";

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

/** Makes the transport's callback members non-configurable, so no tap can be installed on them. */
function sealCallbacks(transport: Transport): void {
  let onmessage = transport.onmessage;
  let onclose = transport.onclose;
  Object.defineProperty(transport, "onmessage", {
    configurable: false,
    get: () => onmessage,
    set: (next) => {
      onmessage = next;
    },
  });
  Object.defineProperty(transport, "onclose", {
    configurable: false,
    get: () => onclose,
    set: (next) => {
      onclose = next;
    },
  });
}

async function echo(): Promise<unknown> {
  const [clientSide, serverSide] = inMemoryPair();
  sealCallbacks(serverSide);
  sealCallbacks(clientSide);
  await instrument(buildServer()).connect(serverSide);
  const client = instrumentClient(newClient("legacy"));
  await client.connect(clientSide);
  const result = await client.callTool({ name: "echo", arguments: { text: "still works" } });
  await client.close();
  return result.content;
}

test("a transport that cannot be tapped is left uninstrumented and keeps working", async () => {
  expect(await echo()).toEqual([{ type: "text", text: "still works" }]);
  await flush();
  expect(collector.spans).toEqual([]);
});

test("instrumenting twice, or the McpServer and its inner server, records each operation once", async () => {
  const server = buildServer();
  instrument(server);
  instrument(server);
  instrument(server.server);
  const [clientSide, serverSide] = inMemoryPair();
  // Connecting the inner server directly bypasses McpServer.connect; it is decorated too.
  await server.server.connect(serverSide);
  const client = newClient("legacy");
  await client.connect(clientSide);
  await client.listTools();
  await client.close();
  await flush();
  expect(collector.spans.map((span) => span.name)).toEqual([
    "initialize",
    "notifications/initialized",
    "tools/list",
  ]);
});

test("an identify hook that throws does not affect the call", async () => {
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(buildServer(), {
    identify: () => {
      throw new Error("hook failed");
    },
  }).connect(serverSide);
  const client = newClient("legacy");
  await client.connect(clientSide);
  const result = await client.callTool({ name: "echo", arguments: { text: "fine" } });
  await client.close();
  expect(result.content).toEqual([{ type: "text", text: "fine" }]);
  await flush();
  const call = collector.spans.find((span) => span.attributes["gen_ai.tool.name"] === "echo");
  expect(call?.attributes["crucible.mcp.identity.source"]).toBeUndefined();
});

test("without init, instrumentation records nothing and the server works", async () => {
  await shutdown();
  try {
    const [clientSide, serverSide] = inMemoryPair();
    await instrument(buildServer()).connect(serverSide);
    const client = instrumentClient(newClient("legacy"));
    await client.connect(clientSide);
    const result = await client.callTool({ name: "echo", arguments: { text: "no init" } });
    await client.close();
    expect(result.content).toEqual([{ type: "text", text: "no init" }]);
  } finally {
    init({ endpoint: collector.url, secretKey: collector.key, flushIntervalMs: 10 });
  }
  await flush();
  expect(collector.spans).toEqual([]);
});
