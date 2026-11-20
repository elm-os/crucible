// Feedback: each call posts one score to the project's feedback route with the secret key, in call order and after
// the spans that ended before it. The call never blocks or throws; a transient failure is retried with the same
// Idempotency-Key and body, and every score that is not sent is counted.

import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, beforeEach, expect, test, vi } from "vitest";
import { feedback, flush, init, observe, shutdown, stats } from "../src/index.ts";
import { type CollectedRequest, type Collector, startCollector } from "./support/collector.ts";

const PROJECT = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";
const ROUTE = `/api/v1/projects/${PROJECT}/feedback`;

let collector: Collector;

beforeAll(async () => {
  collector = await startCollector();
  // A long flush interval: spans leave only through flush, shutdown or the feedback sender.
  init({
    endpoint: collector.url,
    secretKey: collector.key,
    projectId: PROJECT,
    flushIntervalMs: 60_000,
    exportTimeoutMs: 5_000,
  });
});

afterAll(async () => {
  await shutdown();
  await collector.close();
});

beforeEach(async () => {
  await flush();
  collector.requests.length = 0;
  collector.feedbackAnswers.length = 0;
});

function posted(): CollectedRequest[] {
  return collector.requests.filter((request) => request.path === ROUTE);
}

function body(request: CollectedRequest | undefined): unknown {
  return JSON.parse(request?.body.toString() ?? "null");
}

function problem(code: string): string {
  return JSON.stringify({ code, status: 409 });
}

async function settled(): Promise<void> {
  await vi.waitFor(() => expect(posted().length).toBeGreaterThan(0), { timeout: 10_000 });
  await flush();
}

test("feedback posts one score to the project's feedback route with the secret key", async () => {
  const before = stats();
  const returned = feedback({
    traceId: TRACE,
    name: "helpful",
    value: 1,
    comment: "good answer",
    idempotencyKey: "thumbs-4bf9",
  });
  expect(returned).toBeUndefined();
  await settled();
  const [request] = posted();
  expect(posted()).toHaveLength(1);
  expect(request?.method).toBe("POST");
  expect(request?.headers.authorization).toBe(`Bearer ${collector.key}`);
  expect(request?.headers["content-type"]).toBe("application/json");
  expect(request?.headers["idempotency-key"]).toBe("thumbs-4bf9");
  expect(body(request)).toEqual({
    trace_id: TRACE,
    name: "helpful",
    value: 1,
    comment: "good answer",
  });
  expect(stats().feedbackSent - before.feedbackSent).toBe(1);
  expect(stats().feedbackFailed).toBe(before.feedbackFailed);
});

test("every field goes out under the route's name, and each call without a key gets its own", async () => {
  feedback({
    traceId: TRACE,
    spanId: "00f067aa0ba902b7",
    name: "tone",
    value: "friendly",
    scoreId: "reviewer-2",
    dataType: "categorical",
    metadata: { source: "thumbs", rating: 4 },
    retracted: false,
  });
  feedback({ sessionId: "conversation-17", name: "resolved", value: true });
  await vi.waitFor(() => expect(posted()).toHaveLength(2), { timeout: 10_000 });
  const [span, session] = posted();
  expect(body(span)).toEqual({
    trace_id: TRACE,
    span_id: "00f067aa0ba902b7",
    name: "tone",
    value: "friendly",
    score_id: "reviewer-2",
    data_type: "categorical",
    metadata: { source: "thumbs", rating: 4 },
    retracted: false,
  });
  expect(body(session)).toEqual({ session_id: "conversation-17", name: "resolved", value: true });
  const keys = posted().map((request) => request.headers["idempotency-key"]);
  for (const key of keys) {
    expect(key).toMatch(/^[0-9a-f]{32}$/);
  }
  expect(new Set(keys).size).toBe(2);
});

test("spans that ended before the call reach Crucible before the score does", async () => {
  observe("answer", { type: "agent" }, () => undefined);
  feedback({ traceId: TRACE, name: "helpful", value: 1 });
  await vi.waitFor(() => expect(posted()).toHaveLength(1), { timeout: 10_000 });
  const paths = collector.requests.map((request) => request.path);
  expect(paths.indexOf("/v1/traces")).toBeGreaterThanOrEqual(0);
  expect(paths.indexOf("/v1/traces")).toBeLessThan(paths.indexOf(ROUTE));
});

test("a 503 with Retry-After is retried with the same key and body", async () => {
  const before = stats();
  collector.feedbackAnswers.push({ status: 503, headers: { "retry-after": "0" } });
  feedback({ traceId: TRACE, name: "helpful", value: 0.5 });
  await vi.waitFor(() => expect(posted()).toHaveLength(2), { timeout: 10_000 });
  await flush();
  const [first, second] = posted();
  expect(first?.headers["idempotency-key"]).toBe(second?.headers["idempotency-key"]);
  expect(first?.body.equals(second?.body ?? Buffer.alloc(0))).toBe(true);
  expect(stats().feedbackSent - before.feedbackSent).toBe(1);
  expect(stats().feedbackFailed).toBe(before.feedbackFailed);
});

test("a 409 conflict is retried and a reused key is not", async () => {
  const before = stats();
  collector.feedbackAnswers.push(
    { status: 409, headers: { "retry-after": "0" }, body: problem("conflict") },
    { status: 201 },
    { status: 409, body: problem("idempotency_key_reused") },
  );
  feedback({ traceId: TRACE, name: "first", value: 1 });
  feedback({ traceId: TRACE, name: "second", value: 1 });
  await vi.waitFor(() => expect(posted()).toHaveLength(3), { timeout: 10_000 });
  await flush();
  expect(posted().map((request) => (body(request) as { name: string }).name)).toEqual([
    "first",
    "first",
    "second",
  ]);
  expect(stats().feedbackSent - before.feedbackSent).toBe(1);
  expect(stats().feedbackFailed - before.feedbackFailed).toBe(1);
});

test("a refusal is not retried and counts as failed", async () => {
  const before = stats();
  collector.feedbackAnswers.push({ status: 422, body: '{"code":"validation_failed"}' });
  feedback({ traceId: TRACE, name: "bad#name", value: 1 });
  await settled();
  expect(posted()).toHaveLength(1);
  expect(stats().feedbackFailed - before.feedbackFailed).toBe(1);
  expect(stats().feedbackSent).toBe(before.feedbackSent);
});

test("the first refusal and the first drop are each reported once, with what to change", async () => {
  await shutdown();
  const warnings: string[] = [];
  const spy = vi.spyOn(process, "emitWarning").mockImplementation((warning) => {
    warnings.push(String(warning));
  });
  const saved = process.env.CRUCIBLE_PROJECT_ID;
  delete process.env.CRUCIBLE_PROJECT_ID;
  try {
    init({ endpoint: collector.url, secretKey: collector.key, projectId: PROJECT });
    const notFound = '{"code":"not_found","detail":"no resource exists at this path"}';
    collector.feedbackAnswers.push(
      { status: 404, body: notFound },
      { status: 404, body: notFound },
    );
    feedback({ traceId: TRACE, name: "helpful", value: 1 });
    feedback({ traceId: TRACE, name: "helpful", value: 2 });
    await flush();
    expect(stats().feedbackFailed).toBe(2);
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain(
      `${ROUTE} answered 404 (not_found: no resource exists at this path)`,
    );
    expect(warnings[0]).toContain("apiUrl (CRUCIBLE_API_URL)");
    await shutdown();
    init({ endpoint: collector.url, secretKey: collector.key });
    feedback({ traceId: TRACE, name: "helpful", value: 1 });
    feedback({ traceId: TRACE, name: "helpful", value: 2 });
    expect(stats().feedbackDropped).toBe(2);
    expect(warnings).toHaveLength(2);
    expect(warnings[1]).toContain("CRUCIBLE_PROJECT_ID");
  } finally {
    spy.mockRestore();
    if (saved !== undefined) {
      process.env.CRUCIBLE_PROJECT_ID = saved;
    }
    await shutdown();
    init({
      endpoint: collector.url,
      secretKey: collector.key,
      projectId: PROJECT,
      flushIntervalMs: 60_000,
      exportTimeoutMs: 5_000,
    });
  }
});

test("a connection that fails never reaches the caller: three attempts, then counted as failed", async () => {
  const before = stats();
  collector.feedbackAnswers.push({ status: 0 }, { status: 0 }, { status: 0 });
  expect(() => feedback({ traceId: TRACE, name: "helpful", value: 1 })).not.toThrow();
  await vi.waitFor(() => expect(stats().feedbackFailed - before.feedbackFailed).toBe(1), {
    timeout: 15_000,
  });
  expect(posted()).toHaveLength(3);
  expect(stats().feedbackSent).toBe(before.feedbackSent);
});

test("feedback returns at once while the API is not answering", async () => {
  const before = stats();
  const handled = (): number =>
    stats().feedbackSent + stats().feedbackFailed - before.feedbackSent - before.feedbackFailed;
  collector.mode = "stall";
  try {
    const started = performance.now();
    for (let i = 0; i < 100; i += 1) {
      feedback({ traceId: TRACE, name: `stalled-${i}`, value: i });
    }
    expect(performance.now() - started).toBeLessThan(100);
    await sleep(200);
    expect(handled()).toBe(0);
  } finally {
    collector.mode = "accept";
  }
  // The stalled post times out and is retried; the rest follow.
  await vi.waitFor(() => expect(handled()).toBe(100), { timeout: 30_000 });
  expect(stats().feedbackSent - before.feedbackSent).toBe(100);
});

test("a full queue drops the score and counts it", async () => {
  const before = stats();
  for (let i = 0; i < 1_100; i += 1) {
    feedback({ traceId: TRACE, name: `burst-${i}`, value: i });
  }
  expect(stats().feedbackDropped - before.feedbackDropped).toBe(76);
  await vi.waitFor(() => expect(stats().feedbackSent - before.feedbackSent).toBe(1_024), {
    timeout: 60_000,
  });
});

test("init refuses a project id that is not a UUID and says where to find it", async () => {
  await shutdown();
  try {
    expect(() =>
      init({ endpoint: collector.url, secretKey: collector.key, projectId: "default" }),
    ).toThrow(/projectId .*UUID.*create-key/);
    expect(() =>
      init({ endpoint: collector.url, secretKey: collector.key, apiUrl: "ftp://crucible" }),
    ).toThrow(/apiUrl.*CRUCIBLE_API_URL/);
  } finally {
    init({
      endpoint: collector.url,
      secretKey: collector.key,
      projectId: PROJECT,
      flushIntervalMs: 60_000,
      exportTimeoutMs: 5_000,
    });
  }
});

test("without a project id, feedback is dropped and counted", async () => {
  await shutdown();
  const saved = process.env.CRUCIBLE_PROJECT_ID;
  delete process.env.CRUCIBLE_PROJECT_ID;
  try {
    init({ endpoint: collector.url, secretKey: collector.key });
    feedback({ traceId: TRACE, name: "helpful", value: 1 });
    await flush();
    expect(posted()).toHaveLength(0);
    expect(stats().feedbackDropped).toBe(1);
  } finally {
    if (saved !== undefined) {
      process.env.CRUCIBLE_PROJECT_ID = saved;
    }
    await shutdown();
    init({
      endpoint: collector.url,
      secretKey: collector.key,
      projectId: PROJECT,
      flushIntervalMs: 60_000,
      exportTimeoutMs: 5_000,
    });
  }
});

test("the API URL defaults to the endpoint without /v1/traces, and apiUrl overrides it", async () => {
  await shutdown();
  try {
    init({
      endpoint: `${collector.url}/v1/traces`,
      secretKey: collector.key,
      projectId: PROJECT,
    });
    feedback({ traceId: TRACE, name: "helpful", value: 1 });
    await settled();
    expect(posted()).toHaveLength(1);
    await shutdown();
    collector.requests.length = 0;
    init({
      endpoint: "http://127.0.0.1:9/unused",
      apiUrl: collector.url,
      secretKey: collector.key,
      projectId: PROJECT,
      exportTimeoutMs: 1_000,
    });
    feedback({ traceId: TRACE, name: "helpful", value: 1 });
    await vi.waitFor(() => expect(posted()).toHaveLength(1), { timeout: 10_000 });
  } finally {
    await shutdown();
    init({
      endpoint: collector.url,
      secretKey: collector.key,
      projectId: PROJECT,
      flushIntervalMs: 60_000,
      exportTimeoutMs: 5_000,
    });
  }
});

test("shutdown sends what is queued before it returns, and feedback after it is not sent", async () => {
  feedback({ traceId: TRACE, name: "last-words", value: 1 });
  await shutdown();
  feedback({ traceId: TRACE, name: "too-late", value: 1 });
  await sleep(100);
  expect(posted().map((request) => (body(request) as { name: string }).name)).toEqual([
    "last-words",
  ]);
  init({
    endpoint: collector.url,
    secretKey: collector.key,
    projectId: PROJECT,
    flushIntervalMs: 60_000,
    exportTimeoutMs: 5_000,
  });
  // Counters start again at init.
  expect(stats().feedbackSent).toBe(0);
});
