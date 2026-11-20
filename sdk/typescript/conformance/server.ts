// The reference MCP server of the SDK conformance suite (docs/testing/sdk-conformance.md), built on the official
// TypeScript MCP server and instrumented with the run spec's `sdk` options. Run as
// `node server.ts --stdio` with the run spec in CRUCIBLE_CONFORMANCE_SPEC, it serves both eras over stdio and
// flushes when its stdin closes; `startHttp` serves it in process over streamable HTTP.

import { randomUUID } from "node:crypto";
import { setTimeout as sleep } from "node:timers/promises";
import {
  acceptedContent,
  createMcpHandler,
  inputRequired,
  McpServer,
  WebStandardStreamableHTTPServerTransport,
} from "@modelcontextprotocol/server";
import { serveStdio } from "@modelcontextprotocol/server/stdio";
import * as z from "zod/v4";
import { init, shutdown } from "../src/index.ts";
import {
  annotateError,
  type CaptureRule,
  type InstrumentOptions,
  instrument,
  instrumentHandler,
} from "../src/mcp/index.ts";
import { host } from "../test/support/http-host.ts";

/** The parts of the run spec (`crucible.conformance.run/v1`) the SDK is configured from. */
export interface RunSpec {
  schema: string;
  case: string;
  kind: "mcp" | "agent";
  era?: "legacy" | "modern";
  transport?: "stdio" | "streamable_http";
  otlp: { endpoint: string; traces_url: string };
  checkpoint_url: string;
  api_url: string;
  project_id: string;
  keys: { public_key: string; secret_key: string };
  sdk: {
    service_name: string;
    flush_interval_ms: number;
    environment?: string;
    release?: string;
    deploy_id?: string;
    capture?: {
      tool: string;
      args?: boolean;
      result?: boolean;
      sample_rate?: number;
      max_bytes?: number;
    }[];
    handle_params?: Record<string, string>;
  };
  server?: { name: string; version: string };
  steps: Record<string, unknown>[];
}

const REQUEST_STATE = "crucible-conformance-request-state-1";

/** Starts the SDK as the run spec configures it. */
export function initFromSpec(spec: RunSpec): void {
  init({
    endpoint: spec.otlp.endpoint,
    apiUrl: spec.api_url,
    projectId: spec.project_id,
    secretKey: spec.keys.secret_key,
    serviceName: spec.sdk.service_name,
    flushIntervalMs: spec.sdk.flush_interval_ms,
    ...(spec.server !== undefined && { serviceVersion: spec.server.version }),
    ...(spec.sdk.environment !== undefined && { environment: spec.sdk.environment }),
    ...(spec.sdk.release !== undefined && { release: spec.sdk.release }),
    ...(spec.sdk.deploy_id !== undefined && { deployId: spec.sdk.deploy_id }),
  });
}

function instrumentOptions(spec: RunSpec): InstrumentOptions {
  const capture = spec.sdk.capture?.map(
    (rule): CaptureRule => ({
      tool: rule.tool,
      ...(rule.args !== undefined && { args: rule.args }),
      ...(rule.result !== undefined && { result: rule.result }),
      ...(rule.sample_rate !== undefined && { sampleRate: rule.sample_rate }),
      ...(rule.max_bytes !== undefined && { maxBytes: rule.max_bytes }),
    }),
  );
  return {
    ...(capture !== undefined && { capture }),
    ...(spec.sdk.handle_params !== undefined && { handleParams: spec.sdk.handle_params }),
  };
}

function buildServer(spec: RunSpec): McpServer {
  const identity = spec.server ?? { name: "crucible-conformance-server", version: "0.0.0" };
  const server = new McpServer(identity, { capabilities: { tools: {} } });

  server.registerTool(
    "echo",
    { description: "Returns its text.", inputSchema: z.object({ text: z.string() }) },
    async ({ text }) => ({ content: [{ type: "text", text }] }),
  );

  server.registerTool(
    "fail",
    { description: "Returns a tool error.", inputSchema: z.object({ reason: z.string() }) },
    async ({ reason }) => {
      annotateError("upstream");
      return { content: [{ type: "text", text: reason }], isError: true };
    },
  );

  server.registerTool(
    "throw",
    { description: "Throws.", inputSchema: z.object({ message: z.string() }) },
    async ({ message }) => {
      throw new Error(message);
    },
  );

  server.registerTool(
    "slow",
    {
      description: "Waits, sending progress when asked.",
      inputSchema: z.object({ ms: z.number(), steps: z.number() }),
    },
    async ({ ms, steps }, ctx) => {
      const progressToken = ctx.mcpReq._meta?.progressToken;
      const slices = progressToken !== undefined && steps > 0 ? steps : 1;
      for (let progress = 1; progress <= slices; progress += 1) {
        try {
          await sleep(ms / slices, undefined, { signal: ctx.mcpReq.signal });
        } catch {
          break; // cancelled
        }
        if (progressToken !== undefined && steps > 0) {
          await ctx.mcpReq.notify({
            method: "notifications/progress",
            params: { progressToken, progress, total: steps },
          });
        }
      }
      return { content: [{ type: "text", text: "done" }] };
    },
  );

  server.registerTool(
    "secret",
    { description: "Returns its note unchanged.", inputSchema: z.object({ note: z.string() }) },
    async ({ note }) => ({ content: [{ type: "text", text: note }] }),
  );

  const confirmation = z.object({ confirmed: z.boolean() });
  server.registerTool(
    "needs_input",
    { description: "Asks for a confirmation first.", inputSchema: z.object({ item: z.string() }) },
    async ({ item }, ctx) => {
      const answer = acceptedContent(ctx.mcpReq.inputResponses, "confirm", confirmation);
      if (ctx.mcpReq.requestState() !== undefined && answer?.confirmed === true) {
        return { content: [{ type: "text", text: `confirmed ${item}` }] };
      }
      return inputRequired({
        requestState: REQUEST_STATE,
        inputRequests: {
          confirm: inputRequired.elicit({
            message: `Confirm ${item}?`,
            requestedSchema: confirmation,
          }),
        },
      });
    },
  );

  server.registerTool(
    "lookup_order",
    { description: "Looks an order up.", inputSchema: z.object({ order_id: z.string() }) },
    async ({ order_id: orderId }) => ({
      content: [{ type: "text", text: `order ${orderId}: shipped` }],
    }),
  );

  return server;
}

/**
 * Serves the reference server over streamable HTTP in this process. An `initialize` request opens a legacy session;
 * a request without a session id is served statelessly in the modern era.
 */
export async function startHttp(
  spec: RunSpec,
): Promise<{ url: string; close: () => Promise<void> }> {
  const options = instrumentOptions(spec);
  const sessions = new Map<string, WebStandardStreamableHTTPServerTransport>();
  const modern = instrumentHandler(createMcpHandler(instrument(() => buildServer(spec), options)));
  const server = await host(async (request) => {
    const sessionId = request.headers.get("mcp-session-id");
    if (sessionId !== null) {
      const transport = sessions.get(sessionId);
      return transport === undefined
        ? new Response("unknown session", { status: 404 })
        : transport.handleRequest(request);
    }
    const body = (await request
      .clone()
      .json()
      .catch(() => undefined)) as { method?: unknown } | undefined;
    if (body?.method === "initialize") {
      const transport: WebStandardStreamableHTTPServerTransport =
        new WebStandardStreamableHTTPServerTransport({
          sessionIdGenerator: () => randomUUID(),
          onsessioninitialized: (id) => void sessions.set(id, transport),
        });
      await instrument(buildServer(spec), options).connect(transport);
      return transport.handleRequest(request);
    }
    return modern.fetch(request);
  });
  return {
    url: server.url,
    close: async () => {
      await server.close();
      await Promise.all([...sessions.values()].map((transport) => transport.close()));
      await modern.close();
    },
  };
}

if (process.argv.includes("--stdio")) {
  const spec = JSON.parse(process.env.CRUCIBLE_CONFORMANCE_SPEC ?? "") as RunSpec;
  initFromSpec(spec);
  process.stdin.on("end", () => {
    void shutdown().then(() => process.exit(0));
  });
  serveStdio(instrument(() => buildServer(spec), instrumentOptions(spec)));
}
