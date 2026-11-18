// Reference MCP server for the seam spike: four tools with fixed behaviour.

import { setTimeout as sleep } from "node:timers/promises";
import { McpServer } from "@modelcontextprotocol/server";
import * as z from "zod/v4";
import { annotateError, identify } from "./instrument.ts";

export const SERVER_NAME = "crucible-spike-reference";

export function buildServer(): McpServer {
  const server = new McpServer(
    { name: SERVER_NAME, version: "0.0.0" },
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

  return server;
}
