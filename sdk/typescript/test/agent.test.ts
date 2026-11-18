// Agent helpers: context values on every span, typed observations, generation fields, and the feedback stub.

import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "vitest";
import { feedback, flush, init, observe, shutdown, withContext } from "../src/index.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import type { DecodedSpan } from "./support/otlp.ts";

let collector: Collector;

beforeAll(async () => {
  collector = await startCollector();
  init({ endpoint: collector.url, secretKey: collector.key, flushIntervalMs: 10 });
});

afterAll(async () => {
  await shutdown();
  await collector.close();
});

beforeEach(async () => {
  await flush();
  collector.spans.length = 0;
});

async function spans(): Promise<Map<string, DecodedSpan>> {
  await flush();
  return new Map(collector.spans.map((span) => [span.name, span]));
}

describe("observe", () => {
  test("types, nesting and return values", async () => {
    const value = await observe(
      "agent run",
      { type: "agent", input: { task: "plan" } },
      async () => {
        const inner = observe("lookup", { type: "retriever" }, () => 41);
        await sleep(5);
        return observe("answer", () => inner + 1);
      },
    );
    expect(value).toBe(42);
    const recorded = await spans();
    const run = recorded.get("agent run");
    const lookup = recorded.get("lookup");
    const answer = recorded.get("answer");
    expect(run?.attributes).toEqual({
      "crucible.observation.type": "agent",
      "crucible.input": '{"task":"plan"}',
    });
    expect(lookup?.attributes).toEqual({ "crucible.observation.type": "retriever" });
    expect(answer?.attributes).toEqual({});
    expect(run?.parentSpanId).toBe("");
    expect(lookup?.parentSpanId).toBe(run?.spanId);
    expect(answer?.parentSpanId).toBe(run?.spanId);
    expect(new Set([run?.traceId, lookup?.traceId, answer?.traceId]).size).toBe(1);
    expect(run?.kind).toBe(1);
  });

  test("a generation carries model, usage, provided cost and a prompt reference", async () => {
    observe("chat", { type: "generation" }, (generation) => {
      generation.update({
        traceName: "support-chat",
        input: [{ role: "user", content: "hi" }],
        output: "hello",
        inputMessages: [{ role: "user", parts: [{ type: "text", content: "hi" }] }],
        outputMessages: [{ role: "assistant", parts: [{ type: "text", content: "hello" }] }],
        finishReasons: ["stop"],
        model: "model-large",
        responseModel: "model-large-2026-11-01",
        provider: "acme",
        usage: { input: 120, output: 30, cacheRead: 100, cacheWrite: 10, reasoning: 5 },
        cost: { total: "0.0042", input: 0.003 },
        prompt: { name: "greeting", version: 3 },
      });
    });
    expect((await spans()).get("chat")?.attributes).toEqual({
      "crucible.observation.type": "generation",
      "crucible.trace.name": "support-chat",
      "crucible.input": '[{"role":"user","content":"hi"}]',
      "crucible.output": "hello",
      "gen_ai.input.messages": '[{"role":"user","parts":[{"type":"text","content":"hi"}]}]',
      "gen_ai.output.messages":
        '[{"role":"assistant","parts":[{"type":"text","content":"hello"}]}]',
      "gen_ai.response.finish_reasons": ["stop"],
      "gen_ai.request.model": "model-large",
      "gen_ai.response.model": "model-large-2026-11-01",
      "gen_ai.provider.name": "acme",
      "gen_ai.usage.input_tokens": 120n,
      "gen_ai.usage.output_tokens": 30n,
      "gen_ai.usage.cache_read.input_tokens": 100n,
      "gen_ai.usage.cache_creation.input_tokens": 10n,
      "gen_ai.usage.reasoning.output_tokens": 5n,
      "crucible.cost.total": "0.0042",
      "crucible.cost.input": 0.003,
      "crucible.prompt.name": "greeting",
      "crucible.prompt.version": "3",
    });
  });

  test("a throw or a rejection marks the span as an error and is passed on", async () => {
    expect(() =>
      observe("sync failure", () => {
        throw new Error("boom");
      }),
    ).toThrow("boom");
    await expect(
      observe("async failure", async () => {
        await sleep(1);
        throw new Error("later");
      }),
    ).rejects.toThrow("later");
    const recorded = await spans();
    for (const [name, message] of [
      ["sync failure", "boom"],
      ["async failure", "later"],
    ] as const) {
      const span = recorded.get(name);
      expect(span?.status).toEqual({ code: 2, message });
      expect(span?.events.map((event) => event.name)).toEqual(["exception"]);
      expect(span?.events[0]?.attributes["exception.message"]).toBe(message);
    }
  });
});

describe("withContext", () => {
  test("session, user, tags and metadata reach every span inside, across async boundaries", async () => {
    await withContext(
      { sessionId: "s-1", userId: "u-1", tags: ["beta"], metadata: { plan: "pro", seats: 3 } },
      async () => {
        observe("outer", () => undefined);
        await sleep(5);
        await withContext(
          { userId: "u-2", tags: ["eu", "beta"], metadata: { plan: "team" } },
          async () => {
            await new Promise((resolve) => setTimeout(resolve, 5));
            observe("inner", () => undefined);
          },
        );
      },
    );
    observe("outside", () => undefined);
    const recorded = await spans();
    expect(recorded.get("outer")?.attributes).toEqual({
      "crucible.session.id": "s-1",
      "crucible.user.id": "u-1",
      "crucible.tags": ["beta"],
      "crucible.metadata.plan": "pro",
      "crucible.metadata.seats": 3n,
    });
    expect(recorded.get("inner")?.attributes).toEqual({
      "crucible.session.id": "s-1",
      "crucible.user.id": "u-2",
      "crucible.tags": ["beta", "eu"],
      "crucible.metadata.plan": "team",
      "crucible.metadata.seats": 3n,
    });
    expect(recorded.get("outside")?.attributes).toEqual({});
  });

  test("concurrent contexts do not leak into each other", async () => {
    await Promise.all(
      ["a", "b", "c"].map((id) =>
        withContext({ sessionId: id }, async () => {
          await sleep(id === "a" ? 20 : 5);
          observe(`span ${id}`, () => undefined);
        }),
      ),
    );
    const recorded = await spans();
    for (const id of ["a", "b", "c"]) {
      expect(recorded.get(`span ${id}`)?.attributes["crucible.session.id"]).toBe(id);
    }
  });
});

test("feedback is a stub that sends nothing and says why", async () => {
  const before = collector.requests.length;
  await expect(feedback({ name: "helpful", value: 1, traceId: "0".repeat(32) })).rejects.toThrow(
    /planned and not available in this release.*nothing was sent/,
  );
  expect(collector.requests.length).toBe(before);
});
