// Content capture and scrubbing, handle ids, the error marks, multi-round state hashes and identity sources.

import { StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import {
  createMcpHandler,
  ProtocolError,
  ProtocolErrorCode,
  Server,
} from "@modelcontextprotocol/server";
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "vitest";
import { flush, init, shutdown } from "../src/index.ts";
import {
  type InstrumentOptions,
  instrument,
  instrumentHandler,
  markNotFound,
} from "../src/mcp/index.ts";
import { stateHash } from "../src/mcp/wire.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import { ops } from "./support/expect.ts";
import type { DecodedSpan } from "./support/otlp.ts";
import { buildServer, REQUEST_STATE } from "./support/reference-server.ts";
import {
  fromTemplate,
  inMemoryPair,
  modernTemplate,
  newClient,
  statelessHost,
  type Template,
} from "./support/scenarios.ts";

let collector: Collector;
let template: Template;

beforeAll(async () => {
  collector = await startCollector();
  init({ endpoint: collector.url, secretKey: collector.key, flushIntervalMs: 10 });
  template = await modernTemplate();
});

afterAll(async () => {
  await shutdown();
  await collector.close();
});

beforeEach(async () => {
  await flush();
  collector.spans.length = 0;
});

/** Runs `calls` against an in-memory server instrumented with `options`; returns the tool call spans. */
async function inMemory(
  options: InstrumentOptions,
  calls: { name: string; arguments: Record<string, unknown> }[],
): Promise<DecodedSpan[]> {
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(buildServer(), options).connect(serverSide);
  const client = newClient("legacy");
  await client.connect(clientSide);
  for (const call of calls) {
    await client.callTool(call);
  }
  await client.close();
  await flush();
  return collector.spans.filter((span) => span.attributes["mcp.method.name"] === "tools/call");
}

/** The attributes that differ between tool calls of one in-memory session. */
function callAttributes(span: DecodedSpan | undefined): Record<string, unknown> {
  const {
    "mcp.method.name": _method,
    "crucible.mcp.capture_source": _source,
    "crucible.mcp.era": _era,
    "crucible.mcp.transport": _transport,
    "mcp.protocol.version": _version,
    "crucible.mcp.client.name": _clientName,
    "crucible.mcp.client.version": _clientVersion,
    "crucible.mcp.server.name": _serverName,
    "crucible.mcp.server.version": _serverVersion,
    "crucible.mcp.progress.count": _progress,
    "jsonrpc.request.id": _id,
    ...rest
  } = span?.attributes ?? {};
  return rest;
}

describe("content capture", () => {
  test("off by default: no tool gets arguments or results captured", async () => {
    const [secret] = await inMemory({}, [
      {
        name: "secret",
        arguments: {
          password: "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
          note: "mail alice@example.com",
        },
      },
    ]);
    expect(callAttributes(secret)).toEqual({
      "gen_ai.tool.name": "secret",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
    });
  });

  test("opted in by tool glob, scrubbed, capped, and marked", async () => {
    const [secret, order, long, echo] = await inMemory(
      {
        capture: [{ tool: "secret" }, { tool: "lookup_*", result: false, maxBytes: 64 }],
        handleParams: { lookup_order: "order_id" },
      },
      [
        {
          name: "secret",
          arguments: {
            password: "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
            note: "mail alice@example.com",
          },
        },
        { name: "lookup_order", arguments: { order_id: "ord-1" } },
        { name: "lookup_order", arguments: { order_id: "x".repeat(300) } },
        { name: "echo", arguments: { text: "not captured" } },
      ],
    );
    expect(callAttributes(secret)).toEqual({
      "gen_ai.tool.name": "secret",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
      "gen_ai.tool.call.arguments":
        '{"password":"[redacted:secret_key]","note":"mail [redacted:email]"}',
      "gen_ai.tool.call.result":
        '{"content":[{"type":"text","text":"stored: mail [redacted:email]"}]}',
      "crucible.mcp.content.captured": true,
      "crucible.mcp.content.sample_rate": 1n,
      "crucible.mcp.content.truncation": "none",
    });
    expect(callAttributes(order)).toEqual({
      "gen_ai.tool.name": "lookup_order",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 2n,
      "gen_ai.tool.call.arguments": '{"order_id":"ord-1"}',
      "crucible.mcp.handle.id": "ord-1",
      "crucible.mcp.content.captured": true,
      "crucible.mcp.content.sample_rate": 1n,
      "crucible.mcp.content.truncation": "none",
    });
    expect(callAttributes(long)).toEqual({
      "gen_ai.tool.name": "lookup_order",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 2n,
      // The cut drops the partial token at the end, so no prefix of a long credential survives.
      "gen_ai.tool.call.arguments": '{"order_id":"',
      "crucible.mcp.handle.id": "x".repeat(256),
      "crucible.mcp.content.captured": true,
      "crucible.mcp.content.sample_rate": 1n,
      "crucible.mcp.content.truncation": "cap_exceeded",
    });
    expect(callAttributes(echo)).toEqual({
      "gen_ai.tool.name": "echo",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
    });
  });
});

describe("error marks", () => {
  test("markNotFound and markValidation refine a tool error; annotateError adds a subclass", async () => {
    const [missing, malformed, fail] = await inMemory({}, [
      { name: "lookup_order", arguments: { order_id: "missing" } },
      { name: "lookup_order", arguments: { order_id: "BAD!" } },
      { name: "fail", arguments: {} },
    ]);
    expect(callAttributes(missing)).toEqual({
      "gen_ai.tool.name": "lookup_order",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
      "error.type": "tool_error",
      "crucible.mcp.error.class": "not_found",
    });
    expect(callAttributes(malformed)).toEqual({
      "gen_ai.tool.name": "lookup_order",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
      "error.type": "tool_error",
      "crucible.mcp.error.class": "validation",
    });
    expect(callAttributes(fail)).toEqual({
      "gen_ai.tool.name": "fail",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
      "error.type": "tool_error",
      "crucible.mcp.error.subclass": "business",
    });
    expect([missing, malformed, fail].map((span) => span?.status.code)).toEqual([2, 2, 2]);
  });
});

describe("identity", () => {
  test("the identify hook runs once per message and identify() adds to it", async () => {
    const seen: string[] = [];
    const [slow] = await inMemory(
      {
        identify: (extra) => {
          seen.push(extra === undefined ? "none" : "extra");
          return { tenant: "acme", issuer: "https://issuer.example", subject: "sub-1" };
        },
      },
      [{ name: "slow", arguments: { ms: 1, tag: "alpha" } }],
    );
    expect(callAttributes(slow)).toEqual({
      "gen_ai.tool.name": "slow",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
      "user.id": "alpha",
      "crucible.mcp.tenant": "acme",
      "crucible.mcp.user.issuer": "https://issuer.example",
      "crucible.mcp.user.subject": "sub-1",
      "crucible.mcp.identity.source": "identify_hook",
    });
    // initialize, notifications/initialized and the call.
    expect(seen).toHaveLength(3);
  });

  test('identity "none" suppresses every identity key', async () => {
    const [slow, order] = await inMemory(
      {
        identity: "none",
        identify: () => ({ tenant: "acme" }),
        handleParams: { lookup_order: "order_id" },
      },
      [
        { name: "slow", arguments: { ms: 1, tag: "alpha" } },
        { name: "lookup_order", arguments: { order_id: "ord-1" } },
      ],
    );
    expect(callAttributes(slow)).toEqual({
      "gen_ai.tool.name": "slow",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 1n,
    });
    expect(callAttributes(order)).toEqual({
      "gen_ai.tool.name": "lookup_order",
      "crucible.mcp.result_type": "complete",
      "crucible.mcp.content.blocks.text": 2n,
    });
  });

  test("a token the hosting server verified gives the OAuth client id, never the token", async () => {
    const handler = instrumentHandler(createMcpHandler(instrument(buildServer)));
    const response = await handler.fetch(
      fromTemplate(template, (body) => {
        body.id = 77;
      }),
      { authInfo: { token: "tok-never-recorded", clientId: "client-7", scopes: ["read"] } },
    );
    expect(response.status).toBe(200);
    await handler.close();
    await flush();
    const [call] = ops(collector.spans);
    expect(call?.attributes["crucible.mcp.oauth.client_id"]).toBe("client-7");
    expect(call?.attributes["crucible.mcp.identity.source"]).toBe("token_verified");
    expect(Object.values(call?.attributes ?? {})).not.toContain("tok-never-recorded");
  });
});

test("multi-round requests carry keyed hashes of the state, never the state", async () => {
  const stateless = await statelessHost();
  const client = newClient("auto");
  await client.connect(new StreamableHTTPClientTransport(new URL(stateless.url)));
  await flush();
  collector.spans.length = 0;
  const result = await client.callTool({ name: "needs_input", arguments: {} });
  expect(result.content).toEqual([{ type: "text", text: "confirmed" }]);
  await client.close();
  await stateless.close();
  await flush();
  const rounds = ops(collector.spans).filter(
    (op) => op.attributes["gen_ai.tool.name"] === "needs_input",
  );
  const hash = stateHash(collector.key, REQUEST_STATE);
  expect(rounds.map((op) => op.attributes["crucible.mcp.result_type"]).toSorted()).toEqual([
    "complete",
    "input_required",
  ]);
  const first = rounds.find((op) => op.attributes["crucible.mcp.result_type"] === "input_required");
  const second = rounds.find((op) => op.attributes["crucible.mcp.result_type"] === "complete");
  expect(first?.attributes["crucible.mcp.mrtr.state_out"]).toBe(hash);
  expect(first?.attributes["crucible.mcp.mrtr.state_in"]).toBeUndefined();
  expect(second?.attributes["crucible.mcp.mrtr.state_in"]).toBe(hash);
  expect(second?.attributes["crucible.mcp.mrtr.state_out"]).toBeUndefined();
  for (const span of collector.spans) {
    expect(Object.values(span.attributes)).not.toContain(REQUEST_STATE);
  }
});

test("a low-level server: progress counts, prompt and resource targets, and not-found on invalid params", async () => {
  const server = new Server(
    { name: "low-level", version: "1.0.0" },
    { capabilities: { tools: {}, prompts: {}, resources: {} } },
  );
  server.setRequestHandler("tools/call", async (request, ctx) => {
    if (request.params.name === "progress") {
      const progressToken = request.params._meta?.progressToken;
      if (progressToken !== undefined) {
        for (const progress of [1, 2]) {
          await ctx.mcpReq.notify({
            method: "notifications/progress",
            params: { progressToken, progress, total: 2 },
          });
        }
      }
      return { content: [{ type: "text", text: "done" }] };
    }
    markNotFound();
    throw new ProtocolError(ProtocolErrorCode.InvalidParams, "no such order");
  });
  server.setRequestHandler("prompts/get", async () => ({
    messages: [{ role: "user", content: { type: "text", text: "hi" } }],
  }));
  server.setRequestHandler("resources/read", async (request) => ({
    contents: [{ uri: request.params.uri, text: "data" }],
  }));
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(server).connect(serverSide);
  const client = newClient("legacy");
  await client.connect(clientSide);
  let updates = 0;
  await client.callTool(
    { name: "progress", arguments: {} },
    { onprogress: () => void (updates += 1) },
  );
  await client.callTool({ name: "lookup", arguments: {} }).catch(() => undefined);
  await client.getPrompt({ name: "greet" });
  await client.readResource({ uri: "orders://42" });
  await client.close();
  await flush();
  expect(updates).toBe(2);
  const byName = new Map(ops(collector.spans).map((op) => [op.name, op]));
  expect(byName.get("tools/call progress")?.attributes["crucible.mcp.progress.count"]).toBe(2n);
  const lookup = byName.get("tools/call lookup");
  expect(lookup?.attributes["rpc.response.status_code"]).toBe("-32602");
  expect(lookup?.attributes["crucible.mcp.error.class"]).toBe("not_found");
  expect(lookup?.status.code).toBe(2);
  expect(lookup?.status.message).toContain("no such order");
  expect(byName.get("prompts/get greet")?.attributes["gen_ai.prompt.name"]).toBe("greet");
  expect(byName.get("resources/read")?.attributes["crucible.mcp.resource.template"]).toBe(
    "orders://42",
  );
  expect(byName.get("tools/call progress")?.attributes["crucible.mcp.server.name"]).toBe(
    "low-level",
  );
});
