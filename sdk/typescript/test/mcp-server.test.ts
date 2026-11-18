// Every scenario of the seam spike, checked against the exact attributes of each operation it records.

import { StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import { createMcpHandler, PROTOCOL_VERSION_META_KEY } from "@modelcontextprotocol/server";
import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "vitest";
import { flush, init, shutdown } from "../src/index.ts";
import { instrument, instrumentHandler } from "../src/mcp/index.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import {
  expectSlowOverlap,
  OK,
  type Op,
  ops,
  SERVER,
  sortOps,
  workload,
} from "./support/expect.ts";
import { host } from "./support/http-host.ts";
import { buildServer, SERVER_NAME, SERVER_VERSION } from "./support/reference-server.ts";
import {
  CLIENT_NAME,
  CLIENT_VERSION,
  inMemoryPair,
  newClient,
  nodeSessionScenario,
  sessionHttpScenario,
  statelessHost,
  statelessScenario,
  stdioScenario,
} from "./support/scenarios.ts";

const MODERN = "2026-07-28";
const LEGACY = "2025-11-25";
const LEGACY_IDS = ["0", "1", "2", "3", "4", "5", "6", "7"];
const MODERN_IDS = ["0", "1", "2", "3", "4", "5", "6"];

let collector: Collector;

beforeAll(async () => {
  collector = await startCollector();
  init({
    endpoint: collector.url,
    secretKey: collector.key,
    serviceName: SERVER_NAME,
    flushIntervalMs: 10,
  });
});

afterAll(async () => {
  await shutdown();
  await collector.close();
});

beforeEach(async () => {
  await reset();
});

/** Exports everything queued so far, then forgets it, so a test sees only its own operations. */
async function reset(): Promise<void> {
  await flush();
  collector.spans.length = 0;
}

/** Waits until the collector holds `count` operations. */
async function recorded(count: number): Promise<void> {
  await flush();
  await collector.waitFor(
    (spans) => spans.filter((span) => "mcp.method.name" in span.attributes).length >= count,
  );
}

/** An operation recorded by the fetch wrap for the template's `tools/call echo`. */
function edge(attributes: Record<string, unknown>, status: Op["status"]): Op {
  return {
    name: "tools/call echo",
    kind: SERVER,
    status,
    attributes: {
      "mcp.method.name": "tools/call",
      "crucible.mcp.capture_source": "sdk",
      "crucible.mcp.era": "modern",
      "crucible.mcp.transport": "streamable_http",
      "gen_ai.tool.name": "echo",
      "crucible.mcp.client.name": CLIENT_NAME,
      "crucible.mcp.client.version": CLIENT_VERSION,
      "user_agent.original": "node",
      ...attributes,
    } as Op["attributes"],
  };
}

/** A `tools/call slow` over the in-memory transport, legacy era. */
function inMemoryBase(id: string, extra: Record<string, unknown>): Op["attributes"] {
  return {
    "mcp.method.name": "tools/call",
    "crucible.mcp.capture_source": "sdk",
    "crucible.mcp.era": "legacy",
    "crucible.mcp.transport": "unknown",
    "mcp.protocol.version": LEGACY,
    "jsonrpc.request.id": id,
    "crucible.mcp.progress.count": 0n,
    "crucible.mcp.client.name": CLIENT_NAME,
    "crucible.mcp.client.version": CLIENT_VERSION,
    "crucible.mcp.server.name": SERVER_NAME,
    "crucible.mcp.server.version": SERVER_VERSION,
    "gen_ai.tool.name": "slow",
    ...extra,
  } as Op["attributes"];
}

describe("stdio", () => {
  test("stdio-connect-legacy: StdioServerTransport plus connect", async () => {
    const { closeMs } = await stdioScenario(collector, "connect", "legacy");
    await recorded(9);
    expect(ops(collector.spans)).toEqual(
      workload({
        era: "legacy",
        transport: "stdio",
        protocolVersion: LEGACY,
        ids: LEGACY_IDS,
        client: "all",
      }),
    );
    expectSlowOverlap(collector.spans);
    // The child flushed and exited when its stdin closed; the client did not have to kill it after 2 s.
    expect(closeMs).toBeLessThan(1_900);
  });

  test("stdio-serve-legacy: serveStdio(instrument(factory))", async () => {
    await stdioScenario(collector, "serve", "legacy");
    await recorded(9);
    expect(ops(collector.spans)).toEqual(
      workload({
        era: "legacy",
        transport: "stdio",
        protocolVersion: LEGACY,
        ids: LEGACY_IDS,
        client: "all",
      }),
    );
    expectSlowOverlap(collector.spans);
  });

  test("stdio-serve-modern: serveStdio(instrument(factory))", async () => {
    await stdioScenario(collector, "serve", "modern");
    await recorded(7);
    expect(ops(collector.spans)).toEqual(
      workload({
        era: "modern",
        transport: "stdio",
        protocolVersion: MODERN,
        ids: MODERN_IDS,
        client: "all",
      }),
    );
    expectSlowOverlap(collector.spans);
  });
});

describe("streamable HTTP", () => {
  test("http-session-legacy: session transport plus connect", async () => {
    const { sessionId, seen } = await sessionHttpScenario();
    await recorded(9);
    expect(ops(collector.spans)).toEqual(
      workload({
        era: "legacy",
        transport: "streamable_http",
        protocolVersion: LEGACY,
        ids: LEGACY_IDS,
        client: "all",
        sessionId,
        seen,
      }),
    );
    expectSlowOverlap(collector.spans);
  });

  test("http-node-session-legacy: the Node adapter's accessor-forwarding transport", async () => {
    const { sessionId, seen } = await nodeSessionScenario();
    await recorded(9);
    // This host hands the Node request to the adapter unread, so sizes and user agent are checked by type.
    const expected = workload({
      era: "legacy",
      transport: "streamable_http",
      protocolVersion: LEGACY,
      ids: LEGACY_IDS,
      client: "all",
      sessionId,
      seen,
    });
    const actual = ops(collector.spans);
    for (const op of actual) {
      expect(typeof op.attributes["crucible.mcp.request.bytes"]).toBe("bigint");
      expect(op.attributes["user_agent.original"]).toBe("node");
      delete op.attributes["crucible.mcp.request.bytes"];
      delete op.attributes["user_agent.original"];
    }
    expect(actual).toEqual(expected);
    expectSlowOverlap(collector.spans);
  });

  test("http-stateless-modern and http-stateless-legacy: createMcpHandler(instrument(factory))", async () => {
    const stateless = await statelessHost();
    const { negotiated } = await statelessScenario(stateless.url, "auto");
    expect(negotiated).toBe(MODERN);
    await recorded(8);
    expect(ops(collector.spans)).toEqual(
      workload({
        era: "modern",
        transport: "streamable_http",
        protocolVersion: MODERN,
        ids: MODERN_IDS,
        discover: "server-discover-probe-1",
        client: "all",
        seen: stateless.seen,
      }),
    );
    expectSlowOverlap(collector.spans);

    await reset();
    stateless.seen.clear();
    await statelessScenario(stateless.url, "legacy");
    await recorded(9);
    // Each legacy request gets a fresh server instance, so the client info of `initialize` is gone by the time a
    // tool call arrives. The refused GET stream (405) has no JSON-RPC method and is not recorded.
    expect(ops(collector.spans)).toEqual(
      workload({
        era: "legacy",
        transport: "streamable_http",
        protocolVersion: LEGACY,
        ids: LEGACY_IDS,
        client: "initialize",
        seen: stateless.seen,
      }),
    );
    expectSlowOverlap(collector.spans);
    await stateless.close();
  });

  test("http-modern-edge: exchanges that end before connect are recorded by the fetch wrap only", async () => {
    // Keep one real modern tools/call request as the template for the edge cases below.
    let template: { headers: Headers; body: string } | undefined;
    const handler = instrumentHandler(createMcpHandler(instrument(buildServer)));
    const server = await host(async (request) => {
      if (request.method === "POST" && template === undefined) {
        const body = await request.clone().text();
        if (body.includes('"tools/call"') && body.includes(PROTOCOL_VERSION_META_KEY)) {
          template = { headers: new Headers(request.headers), body };
        }
      }
      return handler.fetch(request);
    });
    const client = newClient("auto");
    await client.connect(new StreamableHTTPClientTransport(new URL(server.url)));
    await client.callTool({ name: "echo", arguments: { text: "hello" } });
    await client.close();
    await server.close();
    if (template === undefined) {
      throw new Error("no modern request template was captured");
    }
    const base = template;
    const post = async (
      mutate: (body: Record<string, unknown>, headers: Headers) => void,
      fetcher = handler,
    ): Promise<{ response: Response; recorded: Op[] }> => {
      const body = JSON.parse(base.body) as Record<string, unknown>;
      const headers = new Headers(base.headers);
      headers.delete("content-length");
      mutate(body, headers);
      await reset();
      const response = await fetcher.fetch(
        new Request(server.url, { method: "POST", headers, body: JSON.stringify(body) }),
      );
      await response.clone().text();
      await recorded(1);
      // A second record would mean the exchange was counted twice; give one the chance to arrive.
      await sleep(50);
      await flush();
      return { response, recorded: ops(collector.spans) };
    };

    const replay = await post((body) => {
      body.id = 9001;
    });
    expect(replay.response.status).toBe(200);
    expect(replay.recorded).toHaveLength(1);
    expect(replay.recorded[0]?.attributes["jsonrpc.request.id"]).toBe("9001");
    expect(replay.recorded[0]?.attributes["http.response.status_code"]).toBeUndefined();

    const revision = await post((body, headers) => {
      body.id = 9002;
      const params = body.params as { _meta: Record<string, unknown> };
      params._meta[PROTOCOL_VERSION_META_KEY] = "2099-01-01";
      headers.set("mcp-protocol-version", "2099-01-01");
    });
    expect(revision.response.status).toBe(400);
    expect(revision.recorded).toEqual([
      edge(
        {
          "jsonrpc.request.id": "9002",
          "mcp.protocol.version": "2099-01-01",
          "http.response.status_code": 400n,
          "rpc.response.status_code": "-32022",
          "error.type": "-32022",
        },
        { code: 2, message: revision.recorded[0]?.status.message ?? "" },
      ),
    ]);
    expect(revision.recorded[0]?.status.message).toMatch(/2099-01-01|revision|version/i);

    const media = await post((body, headers) => {
      body.id = 9003;
      headers.set("content-type", "text/plain");
    });
    expect(media.response.status).toBe(415);
    expect(media.recorded).toEqual([
      edge(
        {
          "jsonrpc.request.id": "9003",
          "mcp.protocol.version": MODERN,
          "http.response.status_code": 415n,
          "rpc.response.status_code": "-32000",
          "error.type": "-32000",
        },
        { code: 2, message: media.recorded[0]?.status.message ?? "" },
      ),
    ]);

    // The same call against a handler that always answers with an event stream.
    const streaming = instrumentHandler(
      createMcpHandler(instrument(buildServer), { responseMode: "sse" }),
    );
    const sse = await post((body) => {
      body.id = 9004;
    }, streaming);
    expect(sse.response.headers.get("content-type")).toContain("text/event-stream");
    expect(sse.recorded).toHaveLength(1);
    expect(sse.recorded[0]?.attributes["crucible.mcp.result_type"]).toBe("complete");
    expect(sse.recorded[0]?.status).toEqual(OK);
    await streaming.close();

    const abort = new AbortController();
    const listenHeaders = new Headers(base.headers);
    listenHeaders.delete("content-length");
    listenHeaders.set("mcp-method", "subscriptions/listen");
    listenHeaders.delete("mcp-name");
    const meta = (JSON.parse(base.body) as { params: { _meta: Record<string, unknown> } }).params
      ._meta;
    await reset();
    const listen = await handler.fetch(
      new Request(server.url, {
        method: "POST",
        headers: listenHeaders,
        signal: abort.signal,
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 9005,
          method: "subscriptions/listen",
          params: { _meta: meta, notifications: { toolsListChanged: true } },
        }),
      }),
    );
    await recorded(1);
    expect(ops(collector.spans)).toEqual([
      {
        name: "subscriptions/listen",
        kind: SERVER,
        status: OK,
        attributes: {
          "mcp.method.name": "subscriptions/listen",
          "crucible.mcp.capture_source": "sdk",
          "crucible.mcp.era": "modern",
          "crucible.mcp.transport": "streamable_http",
          "jsonrpc.request.id": "9005",
          "crucible.mcp.subscription.id": "9005",
          "mcp.protocol.version": MODERN,
          "http.response.status_code": BigInt(listen.status),
          "crucible.mcp.client.name": CLIENT_NAME,
          "crucible.mcp.client.version": CLIENT_VERSION,
          "user_agent.original": "node",
        },
      },
    ]);
    abort.abort();
    await handler.close();
  });

  test("modern HTTP cancellation: closing a request's stream is recorded as stream_close", async () => {
    const stateless = await statelessHost();
    const client = newClient("auto");
    await client.connect(new StreamableHTTPClientTransport(new URL(stateless.url)));
    await reset();
    const abort = new AbortController();
    const pending = client
      .callTool({ name: "slow", arguments: { ms: 400, tag: "gone" } }, { signal: abort.signal })
      .catch(() => undefined);
    await sleep(60);
    abort.abort();
    await pending;
    await sleep(500); // the handler finishes; its late answer must not add a second record
    await recorded(1);
    const slow = ops(collector.spans).filter((op) => op.attributes["gen_ai.tool.name"] === "slow");
    expect(slow).toHaveLength(1);
    expect(slow[0]?.attributes["crucible.mcp.cancel.source"]).toBe("stream_close");
    expect(slow[0]?.attributes["user.id"]).toBeUndefined();
    expect(slow[0]?.status).toEqual(OK);
    await client.close();
    await stateless.close();
  });
});

describe("in-memory", () => {
  test("in-memory-cancel: a cancelled call is recorded once, with cancel source notification", async () => {
    const [clientSide, serverSide] = inMemoryPair();
    await instrument(buildServer()).connect(serverSide);
    const client = newClient("legacy");
    await client.connect(clientSide);
    await reset();
    const abort = new AbortController();
    const pending = client
      .callTool(
        { name: "slow", arguments: { ms: 400, tag: "cancelled" } },
        { signal: abort.signal },
      )
      .catch(() => undefined);
    await sleep(50);
    abort.abort();
    await pending;
    await sleep(500); // let the handler finish; a late response must not create a second record
    await serverSide.close();
    await recorded(2);
    expect(ops(collector.spans)).toEqual(
      sortOps([
        {
          name: "tools/call slow",
          kind: SERVER,
          status: OK,
          attributes: inMemoryBase("1", { "crucible.mcp.cancel.source": "notification" }),
        },
        {
          name: "notifications/cancelled",
          kind: SERVER,
          status: OK,
          attributes: {
            "mcp.method.name": "notifications/cancelled",
            "crucible.mcp.capture_source": "sdk",
            "crucible.mcp.era": "legacy",
            "crucible.mcp.transport": "unknown",
            "mcp.protocol.version": LEGACY,
            "crucible.mcp.is_notification": true,
            "crucible.mcp.client.name": CLIENT_NAME,
            "crucible.mcp.client.version": CLIENT_VERSION,
            "crucible.mcp.server.name": SERVER_NAME,
            "crucible.mcp.server.version": SERVER_VERSION,
          },
        },
      ]),
    );
  });

  test("in-memory-abandon: a call pending when the transport closes is a transport error", async () => {
    const [clientSide, serverSide] = inMemoryPair();
    await instrument(buildServer()).connect(serverSide);
    const client = newClient("legacy");
    await client.connect(clientSide);
    await reset();
    const pending = client
      .callTool({ name: "slow", arguments: { ms: 5_000, tag: "never" } })
      .catch(() => undefined);
    await sleep(50);
    await serverSide.close();
    await pending;
    await recorded(1);
    expect(ops(collector.spans)).toEqual([
      {
        name: "tools/call slow",
        kind: SERVER,
        status: { code: 2, message: "The connection closed before the request was answered." },
        attributes: inMemoryBase("1", {
          "error.type": "transport",
          "crucible.mcp.error.class": "transport",
        }),
      },
    ]);
  });
});

test("the server span is a child of the trace context in params._meta", async () => {
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(buildServer()).connect(serverSide);
  await clientSide.start();
  await reset();
  const traceId = "4bf92f3577b34da6a3ce929d0e0e4736";
  const parentId = "00f067aa0ba902b7";
  await clientSide.send({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: {
      name: "echo",
      arguments: { text: "hi" },
      _meta: {
        traceparent: `00-${traceId}-${parentId}-01`,
        tracestate: "vendor=value",
        baggage: "user=never-recorded",
      },
    },
  });
  await recorded(1);
  const [span] = collector.spans;
  expect(span?.traceId).toBe(traceId);
  expect(span?.parentSpanId).toBe(parentId);
  expect(span?.traceState).toBe("vendor=value");
  expect(Object.values(span?.attributes ?? {})).not.toContain("user=never-recorded");
  expect(Object.keys(span?.attributes ?? {}).some((key) => key.includes("baggage"))).toBe(false);
  await serverSide.close();
});
