// Setup and the export path: OTLP/HTTP protobuf with Bearer auth, the bounded queue, and the collector states.

import { createServer } from "node:net";
import type { ReadableSpan, SpanExporter, SpanProcessor } from "@opentelemetry/sdk-trace-base";
import { BasicTracerProvider } from "@opentelemetry/sdk-trace-base";
import { afterEach, describe, expect, test } from "vitest";
import pkg from "../package.json" with { type: "json" };
import { flush, init, observe, shutdown, stats } from "../src/index.ts";
import { QueueSpanProcessor, spanBytes } from "../src/queue.ts";
import { startCollector } from "./support/collector.ts";

afterEach(async () => {
  await shutdown();
});

async function closedPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  const port = typeof address === "object" && address !== null ? address.port : 0;
  await new Promise<void>((resolve) => server.close(() => resolve()));
  return port;
}

describe("init", () => {
  test("exports protobuf to /v1/traces with the secret key as a Bearer token", async () => {
    const collector = await startCollector();
    init({
      endpoint: collector.url,
      secretKey: collector.key,
      serviceName: "orders",
      serviceVersion: "4.5.6",
      environment: "staging",
      release: "r-42",
      deployId: "deploy-7",
    });
    observe("work", () => 1);
    await flush();
    const [request] = collector.requests;
    expect(request?.method).toBe("POST");
    expect(request?.path).toBe("/v1/traces");
    expect(request?.headers["content-type"]).toBe("application/x-protobuf");
    expect(request?.headers.authorization).toBe(`Bearer ${collector.key}`);
    expect(request?.headers["user-agent"]?.split(" ")[0]).toBe(
      `crucible-sdk-typescript/${pkg.version}`,
    );
    const [span] = collector.spans;
    expect(span?.name).toBe("work");
    expect(span?.attributes["crucible.release"]).toBe("r-42");
    expect(span?.scope).toEqual({ name: "@elm-os/crucible", version: pkg.version });
    const { "service.instance.id": instance, ...resource } = span?.resource ?? {};
    expect(instance).toMatch(/^[0-9a-f-]{36}$/);
    expect(resource).toEqual({
      "service.name": "orders",
      "service.version": "4.5.6",
      "deployment.environment.name": "staging",
      "crucible.deploy.id": "deploy-7",
      "telemetry.sdk.language": "nodejs",
      "telemetry.sdk.name": "opentelemetry",
      "telemetry.sdk.version": "2.12.1",
    });
    expect(stats()).toEqual({
      queued: 0,
      queuedBytes: 0,
      dropped: 0,
      exported: 1,
      failed: 0,
      feedbackSent: 0,
      feedbackDropped: 0,
      feedbackFailed: 0,
    });
    await collector.close();
  });

  test("OTEL_* variables cannot redirect the export, replace the key or turn sampling off", async () => {
    const collector = await startCollector();
    const elsewhere = await closedPort();
    const saved = { ...process.env };
    process.env.OTEL_EXPORTER_OTLP_ENDPOINT = `http://127.0.0.1:${elsewhere}`;
    process.env.OTEL_EXPORTER_OTLP_TRACES_ENDPOINT = `http://127.0.0.1:${elsewhere}/v1/traces`;
    process.env.OTEL_EXPORTER_OTLP_HEADERS =
      "authorization=Basic%20other,Authorization=Basic%20other";
    process.env.OTEL_TRACES_SAMPLER = "always_off";
    try {
      init({ endpoint: collector.url, secretKey: collector.key });
      observe("work", () => 1);
      await flush();
    } finally {
      process.env = saved;
    }
    expect(collector.spans.map((span) => span.name)).toEqual(["work"]);
    expect(collector.requests[0]?.headers.authorization).toBe(`Bearer ${collector.key}`);
    await collector.close();
  });

  test("the endpoint may be given with or without the /v1/traces path", async () => {
    const collector = await startCollector();
    for (const endpoint of [`${collector.url}/`, `${collector.url}/v1/traces`]) {
      init({ endpoint, secretKey: collector.key });
      observe("work", () => 1);
      await shutdown();
    }
    expect(collector.requests.map((request) => request.path)).toEqual(["/v1/traces", "/v1/traces"]);
    expect(collector.spans).toHaveLength(2);
    await collector.close();
  });

  test("reads CRUCIBLE_ENDPOINT and CRUCIBLE_SECRET_KEY", async () => {
    const collector = await startCollector();
    const saved = { ...process.env };
    process.env.CRUCIBLE_ENDPOINT = collector.url;
    process.env.CRUCIBLE_SECRET_KEY = collector.key;
    try {
      init();
    } finally {
      process.env = saved;
    }
    observe("work", () => 1);
    await flush();
    expect(collector.spans).toHaveLength(1);
    await collector.close();
  });

  test("every refusal names the setting that fixes it", () => {
    const saved = { ...process.env };
    delete process.env.CRUCIBLE_SECRET_KEY;
    try {
      expect(() => init()).toThrow(/secretKey option or set CRUCIBLE_SECRET_KEY/);
    } finally {
      process.env = saved;
    }
    expect(() => init({ secretKey: "k", endpoint: "not a url" })).toThrow(
      /endpoint option or set CRUCIBLE_ENDPOINT/,
    );
    expect(() => init({ secretKey: "k", endpoint: "ftp://host" })).toThrow(/http or https/);
    expect(() => init({ secretKey: "k", queueSize: 0 })).toThrow(/queueSize/);
    init({ secretKey: "k" });
    expect(() => init({ secretKey: "k" })).toThrow(/call shutdown\(\) before/);
  });

  test("before init, helpers run and nothing is exported", async () => {
    expect(observe("work", () => 7)).toBe(7);
    expect(await observe("work", async () => 8)).toBe(8);
    expect(stats()).toEqual({
      queued: 0,
      queuedBytes: 0,
      dropped: 0,
      exported: 0,
      failed: 0,
      feedbackSent: 0,
      feedbackDropped: 0,
      feedbackFailed: 0,
    });
    await flush();
    await shutdown();
  });
});

describe("collector states", () => {
  test("a collector that never answers does not slow spans down, and shutdown stays bounded", async () => {
    const collector = await startCollector();
    collector.mode = "stall";
    init({
      endpoint: collector.url,
      secretKey: collector.key,
      exportTimeoutMs: 300,
      flushIntervalMs: 10,
    });
    const started = performance.now();
    for (let i = 0; i < 5_000; i += 1) {
      observe("work", () => i);
    }
    const spanMs = performance.now() - started;
    expect(spanMs).toBeLessThan(2_000);
    const closing = performance.now();
    await shutdown();
    expect(performance.now() - closing).toBeLessThan(2_000);
    await collector.close();
  });

  test("a refused connection counts the batch as failed", async () => {
    init({
      endpoint: `http://127.0.0.1:${await closedPort()}`,
      secretKey: "k",
      exportTimeoutMs: 500,
    });
    observe("work", () => 1);
    await flush();
    expect(stats()).toMatchObject({ exported: 0, failed: 1, dropped: 0 });
  });

  test("a collector answering 503 counts the batch as failed", async () => {
    const collector = await startCollector();
    collector.mode = "fail";
    init({ endpoint: collector.url, secretKey: collector.key, exportTimeoutMs: 300 });
    observe("work", () => 1);
    await flush();
    expect(stats()).toMatchObject({ exported: 0, failed: 1 });
    await collector.close();
  });
});

/** An exporter whose exports never complete. */
function stalledExporter(): SpanExporter & { batches: ReadableSpan[][] } {
  const batches: ReadableSpan[][] = [];
  return {
    batches,
    export: (spans) => void batches.push(spans),
    shutdown: async () => {},
  };
}

describe("queue", () => {
  test("2,048 spans fit, including the batch in flight; the 2,049th is dropped and counted", () => {
    const exporter = stalledExporter();
    const queue = new QueueSpanProcessor(exporter, {
      maxQueueSize: 2_048,
      maxQueueBytes: 1 << 30,
      maxBatchSize: 512,
      flushIntervalMs: 60_000,
      timeoutMs: 60_000,
    });
    const tracer = new BasicTracerProvider({ spanProcessors: [queue] }).getTracer("test");
    for (let i = 0; i < 2_049; i += 1) {
      tracer.startSpan("s").end();
    }
    expect(exporter.batches.map((batch) => batch.length)).toEqual([512]);
    expect(queue.stats()).toMatchObject({ queued: 2_048, dropped: 1, exported: 0, failed: 0 });
  });

  test("the byte bound counts the batch in flight and drops beyond it", () => {
    const exporter = stalledExporter();
    const ended: ReadableSpan[] = [];
    const recorder: SpanProcessor = {
      onStart: () => {},
      onEnd: (span) => void ended.push(span),
      forceFlush: async () => {},
      shutdown: async () => {},
    };
    new BasicTracerProvider({ spanProcessors: [recorder] })
      .getTracer("size")
      .startSpan("s", { attributes: { note: "x".repeat(1_000) } })
      .end();
    const size = spanBytes(ended[0] as ReadableSpan);
    const queue = new QueueSpanProcessor(exporter, {
      maxQueueSize: 2_048,
      maxQueueBytes: 10 * size,
      maxBatchSize: 4,
      flushIntervalMs: 60_000,
      timeoutMs: 60_000,
    });
    const bounded = new BasicTracerProvider({ spanProcessors: [queue] }).getTracer("test");
    for (let i = 0; i < 12; i += 1) {
      bounded.startSpan("s", { attributes: { note: "x".repeat(1_000) } }).end();
    }
    expect(exporter.batches.map((batch) => batch.length)).toEqual([4]);
    expect(queue.stats()).toMatchObject({ queued: 10, queuedBytes: 10 * size, dropped: 2 });
  });

  test("a full queue drops in constant time and never blocks", () => {
    const queue = new QueueSpanProcessor(stalledExporter(), {
      maxQueueSize: 16,
      maxQueueBytes: 1 << 30,
      maxBatchSize: 8,
      flushIntervalMs: 60_000,
      timeoutMs: 60_000,
    });
    const tracer = new BasicTracerProvider({ spanProcessors: [queue] }).getTracer("test");
    const started = performance.now();
    for (let i = 0; i < 100_000; i += 1) {
      tracer.startSpan("s").end();
    }
    expect(performance.now() - started).toBeLessThan(5_000);
    expect(queue.stats()).toMatchObject({ queued: 16, dropped: 100_000 - 16 });
  });

  test("an export that never calls back is given up after its timeout, and the queue moves on", async () => {
    const exporter = stalledExporter();
    const queue = new QueueSpanProcessor(exporter, {
      maxQueueSize: 64,
      maxQueueBytes: 1 << 30,
      maxBatchSize: 2,
      flushIntervalMs: 5,
      timeoutMs: 20,
    });
    const tracer = new BasicTracerProvider({ spanProcessors: [queue] }).getTracer("test");
    for (let i = 0; i < 4; i += 1) {
      tracer.startSpan("s").end();
    }
    await queue.forceFlush();
    await new Promise((resolve) => setTimeout(resolve, 2_500));
    expect(exporter.batches.length).toBeGreaterThanOrEqual(2);
    expect(queue.stats().failed).toBeGreaterThanOrEqual(2);
  });
});
