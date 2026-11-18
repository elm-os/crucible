// The reference MCP server: fixed tools whose outcomes the tests assert exactly.

import { setTimeout as sleep } from "node:timers/promises";
import { inputRequired, McpServer } from "@modelcontextprotocol/server";
import * as z from "zod/v4";
import { annotateError, identify, markNotFound, markValidation } from "../../src/mcp/index.ts";

export const SERVER_NAME = "crucible-reference";
export const SERVER_VERSION = "1.2.3";
/** The state `needs_input` returns on its first round. */
export const REQUEST_STATE = "crucible-conformance-request-state-1";

export function buildServer(): McpServer {
  const server = new McpServer(
    { name: SERVER_NAME, version: SERVER_VERSION },
    { capabilities: { tools: {} } },
  );

  server.registerTool(
    "echo",
    { description: "Returns its input.", inputSchema: z.object({ text: z.string() }) },
    async ({ text }) => ({ content: [{ type: "text", text }] }),
  );

  server.registerTool(
    "fail",
    { description: "Returns a tool error.", inputSchema: z.object({}) },
    async () => {
      annotateError("business");
      return { content: [{ type: "text", text: "order not found" }], isError: true };
    },
  );

  server.registerTool(
    "throw",
    { description: "Throws from the handler.", inputSchema: z.object({}) },
    async () => {
      throw new Error("handler exploded");
    },
  );

  server.registerTool(
    "slow",
    {
      description: "Waits, then tags the active operation with the caller's tag.",
      inputSchema: z.object({ ms: z.number(), tag: z.string() }),
    },
    async ({ ms, tag }) => {
      await sleep(ms);
      // Runs after other calls have started, so a process-wide "current operation" would be wrong.
      identify({ userId: tag });
      annotateError("upstream");
      return { content: [{ type: "text", text: tag }] };
    },
  );

  server.registerTool(
    "secret",
    {
      description: "Echoes a password and a note; the note carries personal data.",
      inputSchema: z.object({ password: z.string(), note: z.string() }),
    },
    async ({ note }) => ({ content: [{ type: "text", text: `stored: ${note}` }] }),
  );

  server.registerTool(
    "needs_input",
    { description: "Asks for another round before it completes.", inputSchema: z.object({}) },
    async (_args, ctx) => {
      if (ctx.mcpReq.requestState() === undefined) {
        return inputRequired({ requestState: REQUEST_STATE });
      }
      return { content: [{ type: "text", text: "confirmed" }] };
    },
  );

  server.registerTool(
    "lookup_order",
    {
      description: "Looks an order up by id.",
      inputSchema: z.object({ order_id: z.string() }),
    },
    async ({ order_id: orderId }) => {
      if (orderId === "missing") {
        markNotFound();
        return { content: [{ type: "text", text: "no such order" }], isError: true };
      }
      if (!/^[a-z0-9-]+$/.test(orderId)) {
        markValidation();
        return { content: [{ type: "text", text: "malformed order id" }], isError: true };
      }
      return {
        content: [
          { type: "text", text: `order ${orderId}` },
          { type: "text", text: "shipped" },
        ],
      };
    },
  );

  return server;
}
