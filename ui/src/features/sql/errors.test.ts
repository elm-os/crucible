import { describe, expect, test } from "vitest";
import { type HttpFailure, UiError } from "@/api/errors";
import type { ProblemBody } from "@/api/types";
import { SqlError, sqlErrorFrom, unavailableNotice, workspaceNotice } from "./errors";

function failure(status: number, problem: ProblemBody | null): HttpFailure {
  return { status, problem, requestIdHeader: "req-1", retryAfterHeader: null };
}

const FIX =
  "Remove the SETTINGS clause: the workspace fixes its settings and limits for every query.";

describe("sqlErrorFrom", () => {
  test("sql_invalid keeps the fix sentence, ClickHouse's message and the position", () => {
    const error = sqlErrorFrom(
      failure(422, {
        code: "sql_invalid",
        detail: "Correct the syntax near FORM.",
        line: 2,
        column: 1,
        errors: [{ pointer: "/sql", code: "clickhouse", message: "Syntax error: failed at FORM" }],
      }),
    );
    expect(error).toBeInstanceOf(SqlError);
    const sql = error as SqlError;
    expect(sql.title).toBe("The query has an error.");
    expect(sql.detail).toBe("Correct the syntax near FORM.");
    expect(sql.clickhouse).toBe("Syntax error: failed at FORM");
    expect([sql.line, sql.column]).toEqual([2, 1]);
    expect(sql.requestId).toBe("req-1");
    expect(sql.action).toBe("none");
  });

  test.each([
    ["sql_forbidden", 403, "The workspace doesn't allow this query."],
    ["sql_time_window_required", 422, "The query needs the time window."],
    ["sql_quota_exceeded", 429, "This project has reached its SQL limit for now."],
    ["query_too_broad", 422, "The query would read too much."],
    ["query_timeout", 504, "The query took too long."],
    ["query_memory_exceeded", 422, "The query needed too much memory."],
    ["validation_failed", 422, "The request needs a correction."],
    ["forbidden", 403, "Your role can't do this."],
    ["conflict", 409, "The run was cancelled."],
    ["precondition_failed", 412, "This saved query changed since you opened it."],
  ])("%s has its own title and shows the server's fix", (code, status, title) => {
    const error = sqlErrorFrom(failure(status, { code, detail: FIX }));
    expect(error.title).toBe(title);
    expect(error.detail).toBe(FIX);
    expect(error.code).toBe(code);
    expect(error.reference).toBeNull();
  });

  test("a database outage keeps the page's own words, and gives the code for an administrator", () => {
    const error = sqlErrorFrom(
      failure(503, {
        code: "control_plane_unavailable",
        detail: "The control plane database did not answer. Try again shortly.",
      }),
    );
    expect(error.title).toBe("Saved queries and history are unavailable right now.");
    expect(error.detail).toBe("Crucible can't reach its database. Try again shortly.");
    expect(error.reference).toBe("req-1");
  });

  test("a code without a fix sentence falls back to the page's own", () => {
    const error = sqlErrorFrom(failure(403, { code: "forbidden" }));
    expect(error.detail).toContain("Viewers can run the project's saved queries.");
    expect((error as SqlError).clickhouse).toBeNull();
    expect((error as SqlError).line).toBeNull();
  });

  test("sql_quota_exceeded carries the seconds to wait", () => {
    const error = sqlErrorFrom(
      failure(429, { code: "sql_quota_exceeded", detail: FIX, retry_after_s: 12 }),
    );
    expect(error.retryAfterS).toBe(12);
    expect(error.detail).toBe(FIX);
  });

  test("a malformed position or errors list is ignored", () => {
    const error = sqlErrorFrom(
      failure(422, { code: "sql_invalid", line: "2", column: 0, errors: [{ code: "other" }] }),
    ) as SqlError;
    expect([error.line, error.column, error.clickhouse]).toEqual([null, null, null]);
  });

  test("other failures keep the shared copy and never show server text", () => {
    const internal = sqlErrorFrom(failure(500, { code: "internal_error", detail: "boom" }));
    expect(internal).not.toBeInstanceOf(SqlError);
    expect(internal.title).toBe("This section couldn't load.");
    expect(internal.detail).not.toContain("boom");
    expect(sqlErrorFrom(failure(401, { code: "unauthenticated" })).action).toBe("sign_in");
    expect(sqlErrorFrom(failure(502, null))).toBeInstanceOf(UiError);
  });
});

describe("workspace unavailable", () => {
  test("each known reason has operator guidance", () => {
    expect(workspaceNotice("isolation_probe_failed", "server text")).toEqual({
      reason: "isolation_probe_failed",
      title: "The SQL workspace is switched off.",
      detail: expect.stringContaining("An operator should read the health page"),
    });
    expect(workspaceNotice("not_configured", null).title).toBe(
      "The SQL workspace is not set up on this server.",
    );
    expect(workspaceNotice("probe_pending", null).detail).toContain("Check again in a moment.");
    expect(workspaceNotice("probe_unavailable", null).title).toBe(
      "The SQL workspace can't reach ClickHouse.",
    );
  });

  test("an unknown reason shows the server's fix, or a fallback", () => {
    expect(workspaceNotice("identity_dropping", "Try again in a moment.").detail).toBe(
      "Try again in a moment.",
    );
    expect(workspaceNotice(null, null)).toEqual({
      reason: null,
      title: "The SQL workspace is unavailable.",
      detail: "Try again shortly.",
    });
  });

  test("a 503 problem becomes the page notice by its reason", () => {
    const error = sqlErrorFrom(
      failure(503, { code: "sql_workspace_unavailable", reason: "not_configured", detail: "x" }),
    );
    expect(unavailableNotice(error)?.title).toBe("The SQL workspace is not set up on this server.");
    expect(unavailableNotice(sqlErrorFrom(failure(503, { code: "internal_error" })))).toBeNull();
    expect(unavailableNotice(new Error("other"))).toBeNull();
  });
});
