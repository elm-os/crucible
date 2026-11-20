import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { setAuth } from "@/lib/auth";
import { clearConnection, saveConnection } from "@/lib/connection";
import { loadPage } from "@/lib/navigate";
import { PROJECT_A, sessionInfo } from "@/test/session";
import { listRuns, runSql } from "./api";

vi.mock("@/lib/navigate", () => ({ loadPage: vi.fn() }));

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
  vi.mocked(loadPage).mockReset();
  vi.unstubAllGlobals();
  clearConnection();
  setAuth({ mode: "unknown" });
});

const unauthenticated = () =>
  Promise.resolve(
    new Response(JSON.stringify({ code: "unauthenticated", status: 401, request_id: "req-1" }), {
      status: 401,
      headers: { "content-type": "application/problem+json" },
    }),
  );

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

test("a session that ended sends the browser to sign in, coming back to this page", async () => {
  window.history.replaceState(null, "", `/sql/?project=${PROJECT_A}&range=7d`);
  setAuth({ mode: "session", session: sessionInfo() });
  fetchMock.mockImplementation(unauthenticated);
  await expect(listRuns(PROJECT_A)).rejects.toMatchObject({ kind: "signed_out" });
  expect(loadPage).toHaveBeenCalledWith(
    `/login/?next=${encodeURIComponent(`/sql/?project=${PROJECT_A}&range=7d`)}`,
  );
});

test("signed in, a 401 from something else offers sign-in without leaving the page", async () => {
  setAuth({ mode: "session", session: sessionInfo() });
  fetchMock.mockResolvedValue(new Response("Unauthorized", { status: 401 }));
  await expect(runSql(PROJECT_A, REQUEST, new AbortController().signal)).rejects.toMatchObject({
    kind: "signed_out",
  });
  expect(loadPage).not.toHaveBeenCalled();
});

test("without a control plane, a 401 keeps the Connect flow", async () => {
  saveConnection({ project: PROJECT_A, key: KEY });
  setAuth({ mode: "keys" });
  fetchMock.mockImplementation(unauthenticated);
  await expect(listRuns(PROJECT_A)).rejects.toMatchObject({ kind: "unauthenticated" });
  expect(loadPage).not.toHaveBeenCalled();
});
