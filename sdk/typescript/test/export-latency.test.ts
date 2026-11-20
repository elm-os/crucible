// A slow or down collector adds no latency to the caller (conformance/export-latency.json, contract 9.10): the same
// calls run against a collector that answers, one that never answers and a port where nothing listens. A call that
// waited for an export would take the export timeout; every call stays far below it, and flush and shutdown keep to
// the export timeout. The added p99 over the healthy run is printed, and gated only on the laptop rig
// (CRUCIBLE_CAPTURE_GATE=1).

import { readFileSync } from "node:fs";
import { createServer } from "node:net";
import { Client } from "@modelcontextprotocol/client";
import { InMemoryTransport, McpServer } from "@modelcontextprotocol/server";
import { expect, test } from "vitest";
import * as z from "zod/v4";
import { flush, init, shutdown } from "../src/index.ts";
import { instrument } from "../src/mcp/index.ts";
import { type Collector, startCollector, TEST_KEY } from "./support/collector.ts";

interface Scenario {
  schema: string;
  tool: string;
  max_bytes: number;
  result_bytes: number;
  collectors: string[];
  warmup: number;
  calls: number;
  export_timeout_ms: number;
  call_ceiling_ms: number;
  slack_ms: number;
  gate_added_p99_ms: number;
}

const read = (name: string): unknown =>
  JSON.parse(readFileSync(new URL(`../../../conformance/${name}`, import.meta.url), "utf8"));
const scenario = read("export-latency.json") as Scenario;
const prose = (read("capture-overhead.json") as { fills: { prose: string } }).fills.prose;
const gated = process.env.CRUCIBLE_CAPTURE_GATE === "1";

/** A port that was free a moment ago: nothing listens on it. */
async function closedPort(): Promise<string> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  await new Promise<void>((resolve) => server.close(() => resolve()));
  if (address === null || typeof address === "string") {
    throw new Error("no port");
  }
  return `http://127.0.0.1:${address.port}`;
}

function quantile(samples: number[], q: number): number {
  const sorted = samples.toSorted((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))] ?? Number.NaN;
}

test("a stalled or refusing collector adds no latency to calls", { timeout: 120_000 }, async () => {
  expect(scenario.schema).toBe("crucible.conformance.export-latency/v1");
  expect(scenario.calls).toBeGreaterThan(512);
  const text = prose
    .repeat(Math.ceil(scenario.result_bytes / prose.length))
    .slice(0, scenario.result_bytes);
  const p99: Record<string, number> = {};
  for (const state of scenario.collectors) {
    let collector: Collector | undefined;
    let endpoint: string;
    if (state === "refused") {
      endpoint = await closedPort();
    } else {
      collector = await startCollector();
      collector.mode = state === "stalled" ? "stall" : "accept";
      endpoint = collector.url;
    }
    init({ endpoint, secretKey: TEST_KEY, exportTimeoutMs: scenario.export_timeout_ms });
    const server = new McpServer({ name: "export-latency", version: "1.0.0" });
    server.registerTool(
      scenario.tool,
      { description: "Returns text.", inputSchema: z.object({}) },
      async () => ({ content: [{ type: "text", text }] }),
    );
    instrument(server, { capture: [{ tool: scenario.tool, maxBytes: scenario.max_bytes }] });
    const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
    await server.connect(serverSide);
    const client = new Client({ name: "export-latency-client", version: "1.0.0" });
    await client.connect(clientSide);
    for (let i = 0; i < scenario.warmup; i += 1) {
      await client.callTool({ name: scenario.tool, arguments: {} });
    }
    const samples: number[] = [];
    for (let i = 0; i < scenario.calls; i += 1) {
      const start = performance.now();
      await client.callTool({ name: scenario.tool, arguments: {} });
      samples.push(performance.now() - start);
    }
    await client.close();
    let start = performance.now();
    await flush();
    const flushed = performance.now() - start;
    start = performance.now();
    await shutdown();
    const stopped = performance.now() - start;
    await collector?.close();
    p99[state] = quantile(samples, 0.99);
    const slowest = Math.max(...samples);
    process.stdout.write(
      `export-latency typescript ${state}: p50 ${quantile(samples, 0.5).toFixed(3)} ms, p99 ${p99[state]?.toFixed(3)} ms, slowest ${slowest.toFixed(3)} ms, flush ${flushed.toFixed(0)} ms, shutdown ${stopped.toFixed(0)} ms\n`,
    );
    if (state !== "healthy") {
      expect(slowest, `${state}: slowest call`).toBeLessThanOrEqual(scenario.call_ceiling_ms);
    }
    const deadline = scenario.export_timeout_ms + scenario.slack_ms;
    expect(flushed, `${state}: flush`).toBeLessThanOrEqual(deadline);
    expect(stopped, `${state}: shutdown`).toBeLessThanOrEqual(deadline);
  }
  for (const state of ["stalled", "refused"]) {
    const added = (p99[state] ?? Number.NaN) - (p99.healthy ?? Number.NaN);
    process.stdout.write(
      `export-latency typescript ${state}: added p99 ${added.toFixed(3)} ms over the healthy collector\n`,
    );
    if (gated) {
      expect(added, `${state}: added p99`).toBeLessThanOrEqual(scenario.gate_added_p99_ms);
    }
  }
});
