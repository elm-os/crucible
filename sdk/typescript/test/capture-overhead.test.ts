// The capture-overhead scenario of the shared SDK suite (conformance/capture-overhead.json, contract 9.10): the same
// tool with capture off and on, in paired alternating rounds over fixed result shapes. The work hook bounds what the
// capture path serializes and scrubs per field; the added p99 is reported, and gated only on the laptop rig
// (CRUCIBLE_CAPTURE_GATE=1).

import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
import { Client } from "@modelcontextprotocol/client";
import { type CallToolResult, InMemoryTransport, McpServer } from "@modelcontextprotocol/server";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import * as z from "zod/v4";
import { init, shutdown } from "../src/index.ts";
import { setCaptureWorkHook } from "../src/mcp/capture.ts";
import { instrument } from "../src/mcp/index.ts";
import rules from "../src/scrub-rules.json" with { type: "json" };
import { setMatcherHook } from "../src/scrub.ts";
import { type Collector, startCollector } from "./support/collector.ts";

interface Block {
  type: "text" | "image";
  fill: string;
  length: number;
  mime_type?: string;
}

interface Shape {
  id: string;
  blocks: Block[];
  records?: number;
  /** The share of the uncaptured p99 the added p99 may reach, when that is more than the absolute gate. */
  gate_relative_p99?: number;
}

interface Run {
  warmup: number;
  rounds: number;
  calls: number;
}

interface Scenario {
  schema: string;
  tool: string;
  max_bytes: number;
  work_bound: number;
  matcher_steps_per_rule_char: number;
  ceiling_ms: number;
  gate_added_p99_ms: number;
  runs: { ci: Run; laptop: Run };
  fills: Record<string, string>;
  shapes: Shape[];
}

const scenario = JSON.parse(
  readFileSync(new URL("../../../conformance/capture-overhead.json", import.meta.url), "utf8"),
) as Scenario;
const gated = process.env.CRUCIBLE_CAPTURE_GATE === "1";
const run = gated ? scenario.runs.laptop : scenario.runs.ci;

function repeatTo(unit: string, length: number): string {
  return unit.repeat(Math.ceil(length / unit.length)).slice(0, length);
}

function base64(length: number): string {
  const bytes = new Uint8Array((length / 4) * 3);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = i % 256;
  }
  return Buffer.from(bytes).toString("base64");
}

function result(shape: Shape): CallToolResult {
  const content: CallToolResult["content"] = shape.blocks.map((block) =>
    block.type === "image"
      ? { type: "image", data: base64(block.length), mimeType: block.mime_type ?? "image/png" }
      : { type: "text", text: repeatTo(scenario.fills[block.fill] ?? "", block.length) },
  );
  if (shape.records === undefined) {
    return { content };
  }
  const records = Array.from({ length: shape.records }, (_, i) => ({
    id: i + 1,
    sku: `SKU-${String(i + 1).padStart(5, "0")}`,
    qty: (i + 1) % 7,
  }));
  return { content, structuredContent: { records } };
}

const results = new Map(scenario.shapes.map((shape) => [shape.id, result(shape)]));

function shapeServer(): McpServer {
  const server = new McpServer({ name: "capture-overhead", version: "1.0.0" });
  server.registerTool(
    scenario.tool,
    { description: "Returns a fixed result shape.", inputSchema: z.object({ shape: z.string() }) },
    async ({ shape }) => results.get(shape) ?? { content: [] },
  );
  return server;
}

async function connect(server: McpServer): Promise<Client> {
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  await server.connect(serverSide);
  const client = new Client({ name: "capture-overhead-client", version: "1.0.0" });
  await client.connect(clientSide);
  return client;
}

function quantile(samples: number[], q: number): number {
  const sorted = samples.toSorted((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))] ?? Number.NaN;
}

const p99 = (samples: number[]): number => quantile(samples, 0.99);

/** Long enough that a stalled export holds the queue still for the whole run; it also bounds `shutdown`. */
const EXPORT_TIMEOUT_MS = 60_000;

let collector: Collector;
let off: Client;
let on: Client;

beforeAll(async () => {
  // The collector never answers, so the export queue fills and drops instead of exporting during the rounds.
  collector = await startCollector();
  collector.mode = "stall";
  init({
    endpoint: collector.url,
    secretKey: collector.key,
    flushIntervalMs: 60_000,
    exportTimeoutMs: EXPORT_TIMEOUT_MS,
  });
  off = await connect(instrument(shapeServer(), { capture: [] }));
  on = await connect(
    instrument(shapeServer(), {
      capture: [{ tool: scenario.tool, maxBytes: scenario.max_bytes }],
    }),
  );
});

afterAll(async () => {
  setCaptureWorkHook(undefined);
  setMatcherHook(undefined);
  await off.close();
  await on.close();
  await collector.close();
  // The laptop rounds fill a batch, so an export to the stalled collector may still be in flight here.
  await shutdown();
}, EXPORT_TIMEOUT_MS + 10_000);

describe("capture overhead (conformance/capture-overhead.json)", () => {
  test("the scenario file is the version this test reads", () => {
    expect(scenario.schema).toBe("crucible.conformance.capture-overhead/v1");
  });

  for (const shape of scenario.shapes) {
    test(`${shape.id}: the capture path stays within the work bound`, async () => {
      const args = { name: scenario.tool, arguments: { shape: shape.id } };
      const call = async (client: Client): Promise<number> => {
        const started = performance.now();
        await client.callTool(args);
        return performance.now() - started;
      };
      for (let i = 0; i < run.warmup; i += 1) {
        await call(off);
        await call(on);
      }
      // The work of one captured call, with the hooks installed; the timed rounds run without them.
      let serialized = 0;
      let scrubbed = 0;
      let scrubbedInCall = 0;
      let steps = 0;
      setCaptureWorkHook((s, c) => {
        serialized = Math.max(serialized, s);
        scrubbed = Math.max(scrubbed, c);
        scrubbedInCall += c;
      });
      setMatcherHook((_, s) => {
        steps += s;
      });
      try {
        await call(on);
      } finally {
        setCaptureWorkHook(undefined);
        setMatcherHook(undefined);
      }
      const stepsPerChar = steps / Math.max(scrubbedInCall, 1);
      const without: number[] = [];
      const withCapture: number[] = [];
      for (let round = 0; round < run.rounds; round += 1) {
        for (const arm of round % 2 === 0 ? [off, on] : [on, off]) {
          for (let i = 0; i < run.calls; i += 1) {
            (arm === on ? withCapture : without).push(await call(arm));
          }
        }
      }
      const added = p99(withCapture) - p99(without);
      const gate = Math.max(
        scenario.gate_added_p99_ms,
        (shape.gate_relative_p99 ?? 0) * p99(without),
      );
      const excess = Math.max(...withCapture) - quantile(without, 0.5);
      process.stdout.write(
        `capture-overhead typescript ${shape.id}: added p99 ${added.toFixed(3)} ms (off ${p99(without).toFixed(3)}, on ${p99(withCapture).toFixed(3)}, gate ${gate.toFixed(3)}), slowest excess ${excess.toFixed(3)} ms, serialized ${serialized}, scrubbed ${scrubbed}, matcher steps ${steps} (${stepsPerChar.toFixed(1)} per character)\n`,
      );
      expect(serialized, "code units serialized for one field").toBeLessThanOrEqual(
        scenario.work_bound,
      );
      expect(scrubbed, "code units handed to the scrubber for one field").toBeLessThanOrEqual(
        scenario.work_bound,
      );
      expect(stepsPerChar, "matcher steps per character scrubbed").toBeLessThanOrEqual(
        scenario.matcher_steps_per_rule_char * rules.rules.length,
      );
      expect(
        excess,
        "slowest captured call over the median uncaptured one, ms",
      ).toBeLessThanOrEqual(scenario.ceiling_ms);
      if (gated) {
        expect(added, "added p99, ms").toBeLessThanOrEqual(gate);
      }
    });
  }
});
