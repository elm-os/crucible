import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { setAuth } from "@/lib/auth";
import { clearConnection, saveConnection } from "@/lib/connection";
import { PROJECT_A, sessionInfo } from "@/test/session";
import { listRuns, runSql } from "./api";

const KEY = `csk_${"7".repeat(43)}`;
const REQUEST = { sql: "SELECT 1", from: "2026-11-17T00:00:00Z", to: "2026-11-18T00:00:00Z" };
const fetchMock = vi.fn<typeof fetch>();

function sent(index = 0): Headers {
  return new Headers(fetchMock.mock.calls[index]?.[1]?.headers);
}

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  fetchMock.mockImplementation(() =>
    Promise.resolve(
      new Response("{}", { status: 200, headers: { "content-type": "application/json" } }),
    ),
  );
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  clearConnection();
  setAuth({ mode: "unknown" });
});

test("a session sends its CSRF token on writes and never a key", async () => {
  saveConnection({ project: PROJECT_A, key: KEY });
  setAuth({ mode: "session", session: sessionInfo({ csrf_token: "csrf-1" }) });
  await runSql(PROJECT_A, REQUEST, new AbortController().signal);
  expect(sent().get("x-crucible-csrf")).toBe("csrf-1");
  expect(sent().get("authorization")).toBeNull();
  await listRuns(PROJECT_A);
  expect(sent(1).get("x-crucible-csrf")).toBeNull();
  expect(sent(1).get("authorization")).toBeNull();
});

test("without a control plane the Connect dialog's key is sent", async () => {
  saveConnection({ project: PROJECT_A, key: KEY });
  setAuth({ mode: "keys" });
  await runSql(PROJECT_A, REQUEST, new AbortController().signal);
  expect(sent().get("authorization")).toBe(`Bearer ${KEY}`);
  expect(sent().get("x-crucible-csrf")).toBeNull();
});
