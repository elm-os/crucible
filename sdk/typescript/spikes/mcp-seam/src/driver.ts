// Driver for the seam spike. Exercises the reference server over stdio, session-based Streamable
// HTTP (legacy era) and the stateless HTTP handler (modern and legacy eras), then checks the
// operations the instrumentation recorded. Exits non-zero when any check fails.

import { randomUUID } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import { NodeStreamableHTTPServerTransport } from "@modelcontextprotocol/node";
import {
  createMcpHandler,
  InMemoryTransport,
  PROTOCOL_VERSION_META_KEY,
  WebStandardStreamableHTTPServerTransport,
} from "@modelcontextprotocol/server";
import { host } from "./http-host.ts";
import {
  type Era,
  instrument,
  instrumentFactory,
  instrumentHandler,
  type Operation,
} from "./instrument.ts";
import { buildServer } from "./reference-server.ts";
import { sdkVersion } from "./sdk-version.ts";

const CLIENT_NAME = "crucible-spike-driver";
const MODERN_REVISION = "2026-07-28";

let failures = 0;
function check(scenario: string, name: string, pass: boolean, detail = ""): void {
  if (!pass) {
    failures += 1;
  }
  console.log(
    `${pass ? "PASS" : "FAIL"}  ${scenario}: ${name}${detail === "" ? "" : `  (${detail})`}`,
  );
}

function describe(operation: Operation): string {
  return [
    operation.source,
    operation.method,
    operation.target ?? "-",
    `id=${operation.jsonrpcId ?? "-"}`,
    operation.outcome,
    `code=${operation.errorCode ?? "-"}`,
    `sub=${operation.errorSubclass ?? "-"}`,
    operation.era,
    `v=${operation.protocolVersion ?? "-"}`,
    `client=${operation.clientName ?? "-"}`,
    `user=${operation.identity?.userId ?? "-"}`,
    `session=${operation.sessionId === undefined ? "-" : "yes"}`,
    `http=${operation.httpStatus ?? "-"}`,
    operation.transport,
    `${operation.durationMs.toFixed(2)}ms`,
  ].join(" ");
}

function newClient(mode: "legacy" | "auto"): Client {
  return new Client({ name: CLIENT_NAME, version: "0.0.0" }, { versionNegotiation: { mode } });
}

/** The fixed workload: one list, one call per tool, two overlapping slow calls, one unknown tool. */
async function exercise(client: Client): Promise<void> {
  await client.listTools();
  await client.callTool({ name: "echo", arguments: { text: "hello" } });
  await client.callTool({ name: "fail", arguments: {} });
  await client.callTool({ name: "throw", arguments: {} }).catch(() => undefined);
  await Promise.all([
    client.callTool({ name: "slow", arguments: { ms: 150, tag: "alpha" } }),
    client.callTool({ name: "slow", arguments: { ms: 60, tag: "beta" } }),
  ]);
  await client.callTool({ name: "missing", arguments: {} }).catch(() => undefined);
}

interface Expectation {
  era: Era;
  /** Whether client info is observable on tool calls for this path. */
  clientInfo: boolean;
  sessionId: boolean;
}

function verify(scenario: string, operations: Operation[], expected: Expectation): void {
  console.log(`\n== ${scenario}: ${operations.length} operations`);
  for (const operation of operations) {
    console.log(`   ${describe(operation)}`);
  }
  const call = (target: string) =>
    operations.filter((op) => op.method === "tools/call" && op.target === target);
  const one = (target: string) => {
    const found = call(target);
    check(scenario, `exactly one tools/call ${target}`, found.length === 1, `${found.length}`);
    return found[0];
  };

  const lists = operations.filter((op) => op.method === "tools/list");
  check(scenario, "tools/list recorded once, ok", lists.length === 1 && lists[0]?.outcome === "ok");

  const echo = one("echo");
  check(
    scenario,
    "echo is ok with a jsonrpc id",
    echo?.outcome === "ok" && echo.jsonrpcId !== undefined,
  );

  const fail = one("fail");
  check(scenario, "fail is tool_error", fail?.outcome === "tool_error");
  check(scenario, "annotateError reached the fail operation", fail?.errorSubclass === "business");

  const thrown = one("throw");
  check(scenario, "throw is tool_error", thrown?.outcome === "tool_error", `${thrown?.outcome}`);
  check(scenario, "throw carries no annotation from fail", thrown?.errorSubclass === undefined);

  const missing = one("missing");
  check(
    scenario,
    "unknown tool is protocol_error -32602",
    missing?.outcome === "protocol_error" && missing.errorCode === -32602,
    `${missing?.outcome} ${missing?.errorCode}`,
  );

  const slow = call("slow");
  const alpha = slow.find((op) => op.identity?.userId === "alpha");
  const beta = slow.find((op) => op.identity?.userId === "beta");
  check(
    scenario,
    "two slow calls, each tagged by its own handler",
    slow.length === 2 && !!alpha && !!beta,
  );
  check(
    scenario,
    "slow calls overlapped",
    !!alpha &&
      !!beta &&
      beta.startMs < alpha.startMs + alpha.durationMs &&
      alpha.startMs < beta.startMs + beta.durationMs,
  );
  check(
    scenario,
    "slow durations cover the handler wait",
    (alpha?.durationMs ?? 0) >= 140 &&
      (beta?.durationMs ?? 0) >= 50 &&
      (beta?.durationMs ?? 999) < 140,
    `${alpha?.durationMs.toFixed(0)} ${beta?.durationMs.toFixed(0)}`,
  );

  const calls = operations.filter((op) => op.method === "tools/call");
  check(
    scenario,
    `every operation is ${expected.era} era`,
    operations.every((op) => op.era === expected.era),
  );
  check(
    scenario,
    "every operation came from the transport tap",
    operations.every((op) => op.source === "connect"),
  );
  check(
    scenario,
    "tool calls carry a protocol version",
    calls.every((op) => op.protocolVersion !== undefined),
  );
  check(
    scenario,
    expected.clientInfo
      ? "tool calls carry client info"
      : "client info absent on tool calls (not on the wire)",
    calls.every((op) => (op.clientName === CLIENT_NAME) === expected.clientInfo),
  );
  check(
    scenario,
    expected.sessionId ? "tool calls carry a session id" : "no session id",
    calls.every((op) => (op.sessionId !== undefined) === expected.sessionId),
  );
  if (expected.era === "legacy") {
    const init = operations.filter((op) => op.method === "initialize");
    const notified = operations.filter((op) => op.method === "notifications/initialized");
    check(scenario, "initialize request recorded", init.length === 1 && init[0]?.outcome === "ok");
    check(
      scenario,
      "initialized notification recorded",
      notified.length === 1 && notified[0]?.kind === "notification",
    );
  } else {
    check(
      scenario,
      `modern operations claim ${MODERN_REVISION}`,
      operations.every((op) => op.protocolVersion === MODERN_REVISION),
    );
  }
}

async function stdioScenario(
  scenario: string,
  serverMode: "connect" | "serve",
  era: Era,
): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "crucible-spike-"));
  const opsFile = join(dir, "ops.jsonl");
  const client = newClient(era === "legacy" ? "legacy" : "auto");
  await client.connect(
    new StdioClientTransport({
      command: process.execPath,
      args: [fileURLToPath(new URL("./stdio-server.ts", import.meta.url)), serverMode, opsFile],
    }),
  );
  await exercise(client);
  await client.close();
  await sleep(200);
  const operations = readFileSync(opsFile, "utf8")
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line) as Operation);
  rmSync(dir, { recursive: true, force: true });
  verify(scenario, operations, { era, clientInfo: true, sessionId: false });
}

async function sessionHttpScenario(): Promise<void> {
  const operations: Operation[] = [];
  const sink = (operation: Operation) => void operations.push(operation);
  const sessions = new Map<string, WebStandardStreamableHTTPServerTransport>();
  const server = await host(async (request) => {
    const sessionId = request.headers.get("mcp-session-id");
    let transport = sessionId === null ? undefined : sessions.get(sessionId);
    if (transport === undefined) {
      if (sessionId !== null) {
        return new Response("unknown session", { status: 404 });
      }
      const created = new WebStandardStreamableHTTPServerTransport({
        sessionIdGenerator: () => randomUUID(),
        onsessioninitialized: (id) => void sessions.set(id, created),
      });
      await instrument(buildServer(), { sink }).connect(created);
      transport = created;
    }
    return transport.handleRequest(request);
  });
  const client = newClient("legacy");
  await client.connect(new StreamableHTTPClientTransport(new URL(server.url)));
  await exercise(client);
  await client.close();
  await server.close();
  check("http-session-legacy", "one session was created", sessions.size === 1, `${sessions.size}`);
  verify("http-session-legacy", operations, { era: "legacy", clientInfo: true, sessionId: true });
}

async function statelessHttpScenarios(): Promise<void> {
  const operations: Operation[] = [];
  const sink = (operation: Operation) => void operations.push(operation);
  const handler = instrumentHandler(createMcpHandler(instrumentFactory(buildServer, { sink })), {
    sink,
  });

  // Keep one real modern tools/call request as the template for the edge cases below.
  let template: { headers: Headers; body: string } | undefined;
  const server = await host(async (request) => {
    if (request.method === "POST" && template === undefined) {
      const body = await request.clone().text();
      if (body.includes('"tools/call"') && body.includes(PROTOCOL_VERSION_META_KEY)) {
        template = { headers: new Headers(request.headers), body };
      }
    }
    return handler.fetch(request);
  });

  const modern = newClient("auto");
  await modern.connect(new StreamableHTTPClientTransport(new URL(server.url)));
  check(
    "http-stateless-modern",
    `client negotiated ${MODERN_REVISION}`,
    modern.getNegotiatedProtocolVersion() === MODERN_REVISION,
    `${modern.getNegotiatedProtocolVersion()}`,
  );
  operations.length = 0; // drop the negotiation probe; the workload starts here
  await exercise(modern);
  await modern.close();
  await sleep(50);
  verify("http-stateless-modern", operations.splice(0), {
    era: "modern",
    clientInfo: true,
    sessionId: false,
  });

  const legacy = newClient("legacy");
  await legacy.connect(new StreamableHTTPClientTransport(new URL(server.url)));
  await exercise(legacy);
  await legacy.close();
  await sleep(50);
  // Each legacy request gets a fresh server instance, so client info from `initialize` is gone by
  // the time a tool call arrives.
  const legacyOperations = operations.splice(0);
  const legacyEdge = legacyOperations.filter((op) => op.source === "edge");
  verify(
    "http-stateless-legacy",
    legacyOperations.filter((op) => op.source === "connect"),
    { era: "legacy", clientInfo: false, sessionId: false },
  );
  // The legacy client also tries to open a GET stream, which the stateless handler refuses before
  // any server instance exists.
  for (const operation of legacyEdge) {
    console.log(`   ${describe(operation)}`);
  }
  check(
    "http-stateless-legacy",
    "the refused GET stream is seen by the fetch wrap only (http 405)",
    legacyEdge.length === 1 &&
      legacyEdge[0]?.method === "http.get" &&
      legacyEdge[0].httpStatus === 405,
    `${legacyEdge.length}`,
  );
  await server.close();

  // Exchanges the handler answers before any server instance is connected.
  const scenario = "http-modern-edge";
  if (template === undefined) {
    check(scenario, "captured a modern request template", false);
    return;
  }
  const base = template;
  const post = async (
    mutate: (
      body: { id: number; method: string; params: Record<string, unknown> },
      headers: Headers,
    ) => void,
  ): Promise<{ response: Response; recorded: Operation[] }> => {
    const body = JSON.parse(base.body) as {
      id: number;
      method: string;
      params: Record<string, unknown>;
    };
    const headers = new Headers(base.headers);
    headers.delete("content-length");
    mutate(body, headers);
    const response = await handler.fetch(
      new Request(server.url, { method: "POST", headers, body: JSON.stringify(body) }),
    );
    await sleep(20);
    return { response, recorded: operations.splice(0) };
  };
  const report = (name: string, response: Response, recorded: Operation[]) => {
    console.log(
      `\n== ${scenario}: ${name}: http ${response.status}, ${recorded.length} operations`,
    );
    for (const operation of recorded) {
      console.log(`   ${describe(operation)}`);
    }
  };

  const replay = await post((body) => {
    body.id = 9001;
  });
  report("replayed tools/call", replay.response, replay.recorded);
  check(
    scenario,
    "a served call is recorded once, by the transport tap only",
    replay.recorded.length === 1 && replay.recorded[0]?.source === "connect",
  );

  const badRevision = await post((body, headers) => {
    body.id = 9002;
    (body.params._meta as Record<string, unknown>)[PROTOCOL_VERSION_META_KEY] = "2099-01-01";
    headers.set("mcp-protocol-version", "2099-01-01");
  });
  report("unsupported revision", badRevision.response, badRevision.recorded);
  check(
    scenario,
    "unsupported revision: transport tap records nothing",
    badRevision.recorded.every((op) => op.source !== "connect"),
  );
  check(
    scenario,
    "unsupported revision: fetch wrap records one protocol_error with method, target, id and code",
    badRevision.recorded.length === 1 &&
      badRevision.recorded[0]?.source === "edge" &&
      badRevision.recorded[0].outcome === "protocol_error" &&
      badRevision.recorded[0].method === "tools/call" &&
      badRevision.recorded[0].target !== undefined &&
      badRevision.recorded[0].jsonrpcId === 9002 &&
      badRevision.recorded[0].errorCode !== undefined,
  );

  const badMedia = await post((body, headers) => {
    body.id = 9003;
    headers.set("content-type", "text/plain");
  });
  report("wrong content type", badMedia.response, badMedia.recorded);
  check(
    scenario,
    "wrong content type: only the fetch wrap records it (http 415)",
    badMedia.recorded.length === 1 &&
      badMedia.recorded[0]?.source === "edge" &&
      badMedia.recorded[0].httpStatus === 415 &&
      badMedia.recorded[0].outcome === "protocol_error",
  );

  // The same call against a handler that always answers with an event stream.
  const streaming = instrumentHandler(
    createMcpHandler(instrumentFactory(buildServer, { sink }), { responseMode: "sse" }),
    { sink },
  );
  const sseHeaders = new Headers(base.headers);
  sseHeaders.delete("content-length");
  const sseResponse = await streaming.fetch(
    new Request(server.url, { method: "POST", headers: sseHeaders, body: base.body }),
  );
  const sseText = await sseResponse.text();
  await sleep(20);
  const sseRecorded = operations.splice(0);
  report("tools/call with responseMode sse", sseResponse, sseRecorded);
  check(
    scenario,
    "event stream response: recorded once by the transport tap, ok",
    (sseResponse.headers.get("content-type") ?? "").includes("text/event-stream") &&
      sseText.includes("hello") &&
      sseRecorded.length === 1 &&
      sseRecorded[0]?.source === "connect" &&
      sseRecorded[0].outcome === "ok",
    `content-type ${sseResponse.headers.get("content-type")}`,
  );
  await streaming.close();

  const abort = new AbortController();
  const listenBody = JSON.parse(base.body) as { params: { _meta: Record<string, unknown> } };
  const listenHeaders = new Headers(base.headers);
  listenHeaders.delete("content-length");
  listenHeaders.set("mcp-method", "subscriptions/listen");
  listenHeaders.delete("mcp-name");
  const listenResponse = await handler.fetch(
    new Request(server.url, {
      method: "POST",
      headers: listenHeaders,
      signal: abort.signal,
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 9004,
        method: "subscriptions/listen",
        params: { _meta: listenBody.params._meta, notifications: { toolsListChanged: true } },
      }),
    }),
  );
  await sleep(20);
  const listenRecorded = operations.splice(0);
  report("subscriptions/listen", listenResponse, listenRecorded);
  check(
    scenario,
    "subscriptions/listen: only the fetch wrap records it",
    listenRecorded.length === 1 &&
      listenRecorded[0]?.source === "edge" &&
      listenRecorded[0].method === "subscriptions/listen",
    `content-type ${listenResponse.headers.get("content-type")}`,
  );
  abort.abort();
  await handler.close();
}

/**
 * The Node adapter's transport forwards its callbacks to an inner transport through accessors on
 * its class, so it exercises the other branch of the tap.
 */
async function nodeAdapterScenario(): Promise<void> {
  const scenario = "http-node-session-legacy";
  const operations: Operation[] = [];
  const sink = (operation: Operation) => void operations.push(operation);
  const sessions = new Map<string, NodeStreamableHTTPServerTransport>();
  const server = createServer(async (req, res) => {
    const header = req.headers["mcp-session-id"];
    const sessionId = typeof header === "string" ? header : undefined;
    let transport = sessionId === undefined ? undefined : sessions.get(sessionId);
    if (transport === undefined) {
      if (sessionId !== undefined) {
        res.writeHead(404).end();
        return;
      }
      const created = new NodeStreamableHTTPServerTransport({
        sessionIdGenerator: () => randomUUID(),
        onsessioninitialized: (id) => void sessions.set(id, created),
      });
      await instrument(buildServer(), { sink }).connect(created);
      transport = created;
    }
    await transport.handleRequest(req, res);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  const client = newClient("legacy");
  await client.connect(new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${port}/mcp`)));
  await exercise(client);
  await client.close();
  server.closeAllConnections();
  await new Promise<void>((resolve) => server.close(() => resolve()));
  check(scenario, "one session was created", sessions.size === 1, `${sessions.size}`);
  verify(scenario, operations, { era: "legacy", clientInfo: true, sessionId: true });
}

/** A request the client cancels gets no response from the SDK; it must still be recorded once. */
async function cancelScenario(): Promise<void> {
  const scenario = "in-memory-cancel";
  const operations: Operation[] = [];
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  await instrument(buildServer(), { sink: (operation) => void operations.push(operation) }).connect(
    serverSide,
  );
  const client = newClient("legacy");
  await client.connect(clientSide);
  const abort = new AbortController();
  const pending = client
    .callTool({ name: "slow", arguments: { ms: 400, tag: "cancelled" } }, { signal: abort.signal })
    .catch(() => undefined);
  await sleep(50);
  abort.abort();
  await pending;
  await sleep(500); // let the handler finish; a late response must not create a second record
  await serverSide.close();
  const slow = operations.filter((op) => op.target === "slow");
  console.log(`
== ${scenario}: ${operations.length} operations`);
  for (const operation of operations) {
    console.log(`   ${describe(operation)} type=${operation.errorType ?? "-"}`);
  }
  check(
    scenario,
    "the cancelled call is recorded once as protocol_error cancelled",
    slow.length === 1 && slow[0]?.outcome === "protocol_error" && slow[0].errorType === "cancelled",
    `${slow.length} ${slow[0]?.outcome} ${slow[0]?.errorType}`,
  );
  check(
    scenario,
    "the cancel notification is recorded",
    operations.some((op) => op.method === "notifications/cancelled" && op.kind === "notification"),
  );
}

/** A transport that closes with a request still in flight must not lose the operation. */
async function abandonScenario(): Promise<void> {
  const scenario = "in-memory-abandon";
  const operations: Operation[] = [];
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  await instrument(buildServer(), { sink: (operation) => void operations.push(operation) }).connect(
    serverSide,
  );
  const client = newClient("legacy");
  await client.connect(clientSide);
  const pending = client
    .callTool({ name: "slow", arguments: { ms: 5000, tag: "never" } })
    .catch(() => undefined);
  await sleep(50);
  await serverSide.close();
  await pending;
  const slow = operations.filter((op) => op.target === "slow");
  console.log(`
== ${scenario}: ${operations.length} operations`);
  for (const operation of operations) {
    console.log(`   ${describe(operation)} type=${operation.errorType ?? "-"}`);
  }
  check(
    scenario,
    "the unanswered call is recorded once as protocol_error connection_closed",
    slow.length === 1 &&
      slow[0]?.outcome === "protocol_error" &&
      slow[0].errorType === "connection_closed",
  );
}

console.log(`@modelcontextprotocol/server ${sdkVersion}, node ${process.version}`);
await stdioScenario("stdio-connect-legacy", "connect", "legacy");
await stdioScenario("stdio-serve-legacy", "serve", "legacy");
await stdioScenario("stdio-serve-modern", "serve", "modern");
await sessionHttpScenario();
await nodeAdapterScenario();
await statelessHttpScenarios();
await cancelScenario();
await abandonScenario();
console.log(
  `\n${failures === 0 ? "ALL CHECKS PASSED" : `${failures} CHECKS FAILED`} (sdk ${sdkVersion})`,
);
process.exit(failures === 0 ? 0 : 1);
