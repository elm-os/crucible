import { describe, expect, test } from "vitest";
import {
  asUiError,
  fromHttpFailure,
  type HttpFailure,
  networkError,
  retryDelayMs,
  shouldRetry,
  UiError,
} from "./errors";
import type { ProblemBody } from "./types";

const RAW = "SELECT secret FROM span_core: ClickHouse exception at node ch-3";

function problem(fields: ProblemBody): ProblemBody {
  return { type: "urn:crucible:problem:x", title: RAW, detail: RAW, ...fields };
}

function http(
  status: number,
  body: ProblemBody | null,
  headers: Partial<HttpFailure> = {},
): UiError {
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
    ["forbidden", 403, "forbidden", "Your role can't see this.", 0],
    ["not_found", 404, "not_found", "This isn't in this project.", 0],
    ["validation_failed", 422, "invalid", "This view's filters aren't valid.", 0],
    ["conflict", 409, "invalid", "This view's filters aren't valid.", 0],
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

  // The copy shows once the automatic retries have run out, so it says what to do next.
  test("429 honours Retry-After in the copy and the retry delay", () => {
    const error = http(429, null, { retryAfterHeader: "5" });
    expect(error.kind).toBe("rate_limited");
    expect(error.detail).toBe("Wait 5 seconds, then try again.");
    expect(http(429, null, { retryAfterHeader: "1" }).detail).toBe(
      "Wait 1 second, then try again.",
    );
    expect(http(429, null).detail).toBe("Wait a minute, then try again.");
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
      expect(error.detail).toBe("Try again in a moment.");
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

// Every code of contract 12.3, with its status, and whether its failure is unexpected: only those show
// the request id, which an administrator looks up in the server log.
const CODES: [string, number, boolean][] = [
  ["unauthenticated", 401, false],
  ["invalid_credentials", 401, false],
  ["forbidden", 403, false],
  ["csrf_invalid", 403, false],
  ["origin_invalid", 403, false],
  ["not_found", 404, false],
  ["method_not_allowed", 405, true],
  ["request_timeout", 408, true],
  ["conflict", 409, false],
  ["idempotency_key_reused", 409, false],
  ["precondition_failed", 412, false],
  ["payload_too_large", 413, false],
  ["unsupported_media_type", 415, true],
  ["validation_failed", 422, false],
  ["token_invalid", 422, false],
  ["precondition_required", 428, true],
  ["rate_limited", 429, false],
  ["query_too_broad", 422, false],
  ["query_timeout", 504, false],
  ["query_memory_exceeded", 422, false],
  ["cursor_invalid", 400, false],
  ["control_plane_unavailable", 503, true],
  ["telemetry_store_unavailable", 503, true],
  ["ingest_unavailable", 503, true],
  ["feature_disabled", 404, false],
  ["ssrf_blocked", 422, false],
  ["media_not_supported", 501, false],
  ["media_too_large", 413, false],
  ["media_hash_mismatch", 422, false],
  ["sql_invalid", 422, false],
  ["sql_forbidden", 403, false],
  ["sql_time_window_required", 422, false],
  ["sql_quota_exceeded", 429, false],
  ["sql_workspace_unavailable", 503, false],
  ["export_expired", 410, false],
  ["internal_error", 500, true],
];

describe("the reference shows only for an unexpected failure", () => {
  test.each(CODES)("%s (%i): unexpected %s", (code, status, unexpected) => {
    const error = http(status, problem({ code, status, request_id: "req_9" }));
    expect(error.requestId).toBe("req_9");
    expect(error.reference).toBe(unexpected ? "req_9" : null);
  });

  test("a code this page doesn't know is unexpected, whatever its status", () => {
    for (const status of [400, 409, 422, 500]) {
      const error = http(status, problem({ code: "brand_new", status, request_id: "req_9" }));
      expect(error.reference).toBe("req_9");
    }
  });

  test("an answer that isn't a Crucible problem shows the header's id when there is one", () => {
    expect(http(502, null, { requestIdHeader: "req_proxy" }).reference).toBe("req_proxy");
    expect(http(404, null, { requestIdHeader: "req_static" }).reference).toBe("req_static");
    expect(http(502, null).reference).toBeNull();
  });

  test("a 401 or 429 that isn't a Crucible problem is still an outcome, not a fault", () => {
    expect(http(401, null, { requestIdHeader: "req_proxy" }).reference).toBeNull();
    expect(http(429, null, { requestIdHeader: "req_proxy" }).reference).toBeNull();
  });

  test("a network failure has no id at all", () => {
    expect(networkError().reference).toBeNull();
  });

  test("the rule holds when an error is rebuilt from another, as a page's own error class does", () => {
    const fault = http(500, problem({ code: "internal_error", request_id: "req_9" }));
    const rebuilt = new UiError(fault);
    expect(rebuilt.reference).toBe("req_9");
    const outcome = http(403, problem({ code: "forbidden", request_id: "req_9" }));
    expect(new UiError(outcome).reference).toBeNull();
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
