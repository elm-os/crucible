import { describe, expect, test } from "vitest";
import {
  asUiError,
  fromHttpFailure,
  type HttpFailure,
  networkError,
  retryDelayMs,
  shouldRetry,
  type UiError,
} from "./errors";
import type { Problem } from "./types";

const RAW = "SELECT secret FROM span_core: ClickHouse exception at node ch-3";

function problem(fields: Problem): Problem {
  return { type: "urn:crucible:problem:x", title: RAW, detail: RAW, ...fields };
}

function http(status: number, body: Problem | null, headers: Partial<HttpFailure> = {}): UiError {
  return fromHttpFailure({
    status,
    problem: body,
    requestIdHeader: null,
    retryAfterHeader: null,
    ...headers,
  });
}

function autoRetries(error: UiError): number {
  let count = 0;
  while (shouldRetry(count, error)) count++;
  return count;
}

describe("error anatomy table", () => {
  test.each([
    ["query_timeout", 504, "too_heavy", "This query was too heavy for the selected range.", 0],
    [
      "query_memory_exceeded",
      503,
      "too_heavy",
      "This query was too heavy for the selected range.",
      0,
    ],
    ["query_too_broad", 400, "too_broad", "The range is longer than this project allows.", 0],
    ["cursor_invalid", 400, "cursor_reset", "List refreshed.", 0],
    ["forbidden", 403, "forbidden", "Your role can't do this.", 0],
    ["not_found", 404, "not_found", "This isn't in this project.", 0],
    ["validation_failed", 422, "invalid", "Some fields need attention.", 0],
    ["conflict", 409, "invalid", "Some fields need attention.", 0],
    ["telemetry_store_unavailable", 503, "unknown", "This section couldn't load.", 1],
  ])("%s (%i) maps to %s", (code, status, kind, title, retries) => {
    const error = http(status, problem({ code, status, request_id: "req_1" }));
    expect(error.kind).toBe(kind);
    expect(error.title).toBe(title);
    expect(error.code).toBe(code);
    expect(error.requestId).toBe("req_1");
    expect(autoRetries(error)).toBe(retries);
  });

  test("a network failure retries twice with backoff and has no invented reference", () => {
    const error = networkError();
    expect(error.kind).toBe("network");
    expect(error.title).toBe("Couldn't reach Crucible.");
    expect(error.status).toBeNull();
    expect(error.requestId).toBeNull();
    expect(autoRetries(error)).toBe(2);
    expect([0, 1, 2].map((attempt) => retryDelayMs(attempt, error))).toEqual([1000, 2000, 4000]);
  });

  test("401 means the session ended, whoever answered", () => {
    expect(http(401, null).kind).toBe("unauthenticated");
    expect(http(401, problem({ code: "unauthenticated" })).action).toBe("sign_in");
  });

  test("429 honours Retry-After in the copy and the retry delay", () => {
    const error = http(429, null, { retryAfterHeader: "5" });
    expect(error.kind).toBe("rate_limited");
    expect(error.detail).toBe("Retrying in 5 s.");
    expect(retryDelayMs(0, error)).toBe(5000);
    expect(autoRetries(error)).toBe(2);
    expect(http(429, problem({ retry_after_s: 7 })).retryAfterS).toBe(7);
  });

  test("409 and 422 keep field pointers and codes but drop server messages", () => {
    const error = http(
      422,
      problem({
        code: "validation_failed",
        errors: [{ pointer: "/name", code: "too_long", message: RAW }, { message: RAW }],
      }),
    );
    expect(error.fieldErrors).toEqual([{ pointer: "/name", code: "too_long" }]);
  });
});

describe("responses that are not from the Crucible API", () => {
  test.each([403, 404, 500, 502, 200])(
    "a %i without a problem body is a generic section failure",
    (status) => {
      const error = http(status, null);
      expect(error.kind).toBe("unknown");
      expect(error.title).toBe("This section couldn't load.");
      expect(error.detail).toBe("Nothing was changed. Try again in a moment.");
      expect(autoRetries(error)).toBe(1);
    },
  );

  test("a 403 from CSRF or origin checks does not blame the role", () => {
    expect(http(403, problem({ code: "csrf_invalid" })).kind).toBe("unknown");
  });
});

describe("reference ids", () => {
  test("the problem body wins over the header, and the header is used without a body", () => {
    expect(
      http(500, problem({ request_id: "body-id" }), { requestIdHeader: "header-id" }).requestId,
    ).toBe("body-id");
    expect(
      http(502, null, { requestIdHeader: "0193a5b2-7c1d-7e4f-9a3b-2c5d8e1f4a6b" }).requestId,
    ).toBe("0193a5b2-7c1d-7e4f-9a3b-2c5d8e1f4a6b");
  });

  test("an id outside the documented format is dropped rather than shown", () => {
    expect(http(500, problem({ request_id: "<img src=x onerror=alert(1)>" })).requestId).toBeNull();
    expect(http(500, null, { requestIdHeader: "a".repeat(65) }).requestId).toBeNull();
  });
});

test("no mapped error carries server text", () => {
  const body = problem({
    code: "query_timeout",
    request_id: "req_2",
    errors: [{ pointer: "/q", code: "bad", message: RAW }],
  });
  for (const status of [400, 401, 403, 404, 409, 422, 429, 500, 503]) {
    const error = http(status, body);
    const shown = JSON.stringify({ ...error, message: error.message });
    expect(shown).not.toContain("ClickHouse");
    expect(shown).not.toContain("SELECT");
  }
});

test("anything that is not a UiError becomes the generic failure", () => {
  const error = asUiError(new TypeError(RAW));
  expect(error.kind).toBe("unknown");
  expect(error.message).not.toContain(RAW);
});
