// An application that runs its own OpenTelemetry setup: Crucible leaves it alone, nests with its spans, and the
// MCP handler context still reaches the right operation.

import { context, trace } from "@opentelemetry/api";
import { AsyncLocalStorageContextManager } from "@opentelemetry/context-async-hooks";
import {
  BasicTracerProvider,
  InMemorySpanExporter,
  SimpleSpanProcessor,
} from "@opentelemetry/sdk-trace-base";
import { afterAll, beforeAll, expect, test } from "vitest";
import { flush, init, observe, shutdown } from "../src/index.ts";
import { instrument } from "../src/mcp/index.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import { buildServer } from "./support/reference-server.ts";
import { inMemoryPair, newClient } from "./support/scenarios.ts";

const hostExporter = new InMemorySpanExporter();
const hostProvider = new BasicTracerProvider({
  spanProcessors: [new SimpleSpanProcessor(hostExporter)],
});
let collector: Collector;

beforeAll(async () => {
  context.setGlobalContextManager(new AsyncLocalStorageContextManager().enable());
  trace.setGlobalTracerProvider(hostProvider);
  collector = await startCollector();
  init({ endpoint: collector.url, secretKey: collector.key, flushIntervalMs: 10 });
});

afterAll(async () => {
  await shutdown();
  await collector.close();
  trace.disable();
  context.disable();
});

test("Crucible spans nest under and over the host's spans, and each side exports only its own", async () => {
  const host = trace.getTracer("host");
  await host.startActiveSpan("host request", async (request) => {
    await observe("crucible step", async () => {
      host.startSpan("host query").end();
    });
    request.end();
  });
  await flush();
  const hostSpans = hostExporter.getFinishedSpans();
  const hostRequest = hostSpans.find((span) => span.name === "host request");
  const hostQuery = hostSpans.find((span) => span.name === "host query");
  const step = collector.spans.find((span) => span.name === "crucible step");
  expect(hostSpans.map((span) => span.name).toSorted()).toEqual(["host query", "host request"]);
  expect(collector.spans.map((span) => span.name)).toEqual(["crucible step"]);
  expect(step?.traceId).toBe(hostRequest?.spanContext().traceId);
  expect(step?.parentSpanId).toBe(hostRequest?.spanContext().spanId);
  expect(hostQuery?.parentSpanContext?.spanId).toBe(step?.spanId);
});

test("with a host context manager registered, overlapping calls still tag their own operations", async () => {
  const [clientSide, serverSide] = inMemoryPair();
  await instrument(buildServer()).connect(serverSide);
  const client = newClient("legacy");
  await client.connect(clientSide);
  await Promise.all([
    client.callTool({ name: "slow", arguments: { ms: 120, tag: "alpha" } }),
    client.callTool({ name: "slow", arguments: { ms: 30, tag: "beta" } }),
  ]);
  await client.close();
  await flush();
  const slow = collector.spans.filter((span) => span.attributes["gen_ai.tool.name"] === "slow");
  expect(slow.map((span) => span.attributes["user.id"]).toSorted()).toEqual(["alpha", "beta"]);
  expect(hostExporter.getFinishedSpans().some((span) => span.name.startsWith("tools/"))).toBe(
    false,
  );
});
