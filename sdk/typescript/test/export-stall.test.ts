// The stalled-collector scenario of the shared SDK suite (conformance/export-stall.json, contract 9.10): with
// capture on and a collector that never answers, the export queue stays under its byte ceiling and drops.

import { readFileSync } from "node:fs";
import { Client } from "@modelcontextprotocol/client";
import { InMemoryTransport, McpServer } from "@modelcontextprotocol/server";
import { expect, test } from "vitest";
import * as z from "zod/v4";
import { init, shutdown, stats } from "../src/index.ts";
import { instrument } from "../src/mcp/index.ts";
import { startCollector } from "./support/collector.ts";

interface Scenario {
  schema: string;
  tool: string;
  max_bytes: number;
  result_bytes: number;
  calls: number;
  queue_spans: number;
  queue_bytes: number;
}

const read = (name: string): unknown =>
  JSON.parse(readFileSync(new URL(`../../../conformance/${name}`, import.meta.url), "utf8"));
const scenario = read("export-stall.json") as Scenario;
const prose = (read("capture-overhead.json") as { fills: { prose: string } }).fills.prose;

test("a collector that never answers: the queue stays under its byte ceiling and drops", async () => {
  expect(scenario.schema).toBe("crucible.conformance.export-stall/v1");
  expect(scenario.calls).toBeLessThan(scenario.queue_spans);
  const collector = await startCollector();
  collector.mode = "stall";
  init({ endpoint: collector.url, secretKey: collector.key, exportTimeoutMs: 2_000 });
  const text = prose
    .repeat(Math.ceil(scenario.result_bytes / prose.length))
    .slice(0, scenario.result_bytes);
  const server = new McpServer({ name: "export-stall", version: "1.0.0" });
  server.registerTool(
    scenario.tool,
    { description: "Returns text.", inputSchema: z.object({}) },
    async () => ({ content: [{ type: "text", text }] }),
  );
  instrument(server, { capture: [{ tool: scenario.tool, maxBytes: scenario.max_bytes }] });
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  await server.connect(serverSide);
  const client = new Client({ name: "export-stall-client", version: "1.0.0" });
  await client.connect(clientSide);
  let most = 0;
  for (let i = 0; i < scenario.calls; i += 1) {
    await client.callTool({ name: scenario.tool, arguments: {} });
    most = Math.max(most, stats().queuedBytes);
  }
  const { dropped } = stats();
  await client.close();
  await collector.close();
  await shutdown();
  process.stdout.write(
    `export-stall typescript: most queued ${most} bytes (ceiling ${scenario.queue_bytes}), dropped ${dropped}\n`,
  );
  expect(most, "bytes queued").toBeLessThanOrEqual(scenario.queue_bytes);
  expect(dropped, "spans dropped").toBeGreaterThan(0);
});
