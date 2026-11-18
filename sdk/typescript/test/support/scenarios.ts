// The spike's scenarios: one MCP client workload over every transport and both eras, against the instrumented
// reference server. Each runner returns what the test needs to state the exact expected operations.

import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { fileURLToPath } from "node:url";
import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import { NodeStreamableHTTPServerTransport } from "@modelcontextprotocol/node";
import {
  createMcpHandler,
  InMemoryTransport,
  WebStandardStreamableHTTPServerTransport,
} from "@modelcontextprotocol/server";
import { instrument, instrumentHandler } from "../../src/mcp/index.ts";
import type { Collector } from "./collector.ts";
import { host } from "./http-host.ts";
import { buildServer } from "./reference-server.ts";

export const CLIENT_NAME = "crucible-test-client";
export const CLIENT_VERSION = "0.0.0";

export function newClient(mode: "legacy" | "auto"): Client {
  return new Client(
    { name: CLIENT_NAME, version: CLIENT_VERSION },
    { versionNegotiation: { mode } },
  );
}

/** One list, one call per tool, two overlapping slow calls and one unknown tool. */
export async function exercise(client: Client): Promise<void> {
  await client.listTools();
  await client.callTool({ name: "echo", arguments: { text: "hello" } });
  await client.callTool({ name: "fail", arguments: {} });
  await client.callTool({ name: "throw", arguments: {} });
  await Promise.all([
    client.callTool({ name: "slow", arguments: { ms: 150, tag: "alpha" } }),
    client.callTool({ name: "slow", arguments: { ms: 60, tag: "beta" } }),
  ]);
  await client.callTool({ name: "missing", arguments: {} }).catch(() => undefined);
}

/** What the HTTP host saw per JSON-RPC id (per method for a notification): body length and user agent. */
export type Seen = Map<string, { bytes: number; userAgent: string | null }>;

async function remember(request: Request, seen: Seen): Promise<void> {
  if (request.method !== "POST") {
    return;
  }
  const body = await request.clone().text();
  try {
    const message = JSON.parse(body) as { id?: unknown; method?: unknown };
    const key =
      typeof message.id === "string" || typeof message.id === "number"
        ? String(message.id)
        : message.method;
    if (typeof key === "string") {
      seen.set(key, {
        bytes: Buffer.byteLength(body),
        userAgent: request.headers.get("user-agent"),
      });
    }
  } catch {
    // Not JSON: nothing to remember.
  }
}

export async function stdioScenario(
  collector: Collector,
  serverMode: "connect" | "serve",
  era: "legacy" | "modern",
): Promise<{ closeMs: number }> {
  const client = newClient(era === "legacy" ? "legacy" : "auto");
  await client.connect(
    new StdioClientTransport({
      command: process.execPath,
      args: [
        fileURLToPath(new URL("./stdio-server.ts", import.meta.url)),
        serverMode,
        collector.url,
        collector.key,
      ],
    }),
  );
  await exercise(client);
  const started = performance.now();
  await client.close();
  return { closeMs: performance.now() - started };
}

export async function sessionHttpScenario(): Promise<{ sessionId: string; seen: Seen }> {
  const seen: Seen = new Map();
  const sessions = new Map<string, WebStandardStreamableHTTPServerTransport>();
  const server = await host(async (request) => {
    await remember(request, seen);
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
      await instrument(buildServer()).connect(created);
      transport = created;
    }
    return transport.handleRequest(request);
  });
  const client = newClient("legacy");
  const transport = new StreamableHTTPClientTransport(new URL(server.url));
  await client.connect(transport);
  const sessionId = transport.sessionId ?? "";
  await exercise(client);
  await client.close();
  await server.close();
  return { sessionId, seen };
}

/** The Node adapter's transport forwards its callbacks through accessors, the other branch of the tap. */
export async function nodeSessionScenario(): Promise<{ sessionId: string; seen: Seen }> {
  const seen: Seen = new Map();
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
      await instrument(buildServer()).connect(created);
      transport = created;
    }
    await transport.handleRequest(req, res);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  const client = newClient("legacy");
  const transport = new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${port}/mcp`));
  await client.connect(transport);
  const sessionId = transport.sessionId ?? "";
  await exercise(client);
  await client.close();
  server.closeAllConnections();
  await new Promise<void>((resolve) => server.close(() => resolve()));
  return { sessionId, seen };
}

/** A stateless handler serving both eras, with the edge companion. */
export async function statelessHost(): Promise<{
  url: string;
  seen: Seen;
  fetch: (request: Request) => Promise<Response>;
  close: () => Promise<void>;
}> {
  const seen: Seen = new Map();
  const handler = instrumentHandler(createMcpHandler(instrument(buildServer)));
  const server = await host(async (request) => {
    await remember(request, seen);
    return handler.fetch(request);
  });
  return {
    url: server.url,
    seen,
    fetch: (request) => handler.fetch(request),
    close: async () => {
      await server.close();
      await handler.close();
    },
  };
}

export async function statelessScenario(
  url: string,
  mode: "legacy" | "auto",
): Promise<{ negotiated: string | undefined }> {
  const client = newClient(mode);
  await client.connect(new StreamableHTTPClientTransport(new URL(url)));
  const negotiated = client.getNegotiatedProtocolVersion();
  await exercise(client);
  await client.close();
  return { negotiated };
}

export function inMemoryPair(): [InMemoryTransport, InMemoryTransport] {
  return InMemoryTransport.createLinkedPair();
}

/** A real modern `tools/call echo` request, as the official client sends it, for hand-built exchanges. */
export interface Template {
  url: string;
  headers: Headers;
  body: string;
}

export async function modernTemplate(): Promise<Template> {
  let template: Template | undefined;
  const handler = createMcpHandler(buildServer);
  const server = await host(async (request) => {
    if (request.method === "POST" && template === undefined) {
      const body = await request.clone().text();
      if (body.includes('"tools/call"')) {
        template = { url: request.url, headers: new Headers(request.headers), body };
      }
    }
    return handler.fetch(request);
  });
  const client = newClient("auto");
  await client.connect(new StreamableHTTPClientTransport(new URL(server.url)));
  await client.callTool({ name: "echo", arguments: { text: "hello" } });
  await client.close();
  await server.close();
  await handler.close();
  if (template === undefined) {
    throw new Error("no modern tools/call request was captured");
  }
  return template;
}

/** A request built from `template` after `mutate` edits its body and headers. */
export function fromTemplate(
  template: Template,
  mutate: (body: Record<string, unknown>, headers: Headers) => void,
): Request {
  const body = JSON.parse(template.body) as Record<string, unknown>;
  const headers = new Headers(template.headers);
  headers.delete("content-length");
  mutate(body, headers);
  return new Request(template.url, { method: "POST", headers, body: JSON.stringify(body) });
}
