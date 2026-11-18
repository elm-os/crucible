import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { setAuth } from "@/lib/auth";
import { loadPage } from "@/lib/navigate";
import { sessionInfo } from "@/test/session";
import {
  acceptInvitation,
  confirmPasswordReset,
  createInvitation,
  getSession,
  getSignInMode,
  listTraces,
  login,
  logout,
  removeOrgMember,
  requestPasswordReset,
  revokeApiKey,
  setupOwner,
} from "./client";
import { UiError } from "./errors";

vi.mock("@/lib/navigate", () => ({ loadPage: vi.fn() }));

const KEY = `csk_${"7".repeat(43)}`;
const WINDOW = { from: "2026-11-17T00:00:00Z", to: "2026-11-18T00:00:00Z" };
const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.mocked(loadPage).mockReset();
  vi.unstubAllGlobals();
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ code, status, title: "server text", request_id: "req-1" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

function sent(call = 0): { url: string; init: RequestInit; headers: Headers } {
  const [url, init = {}] = fetchMock.mock.calls[call] ?? [];
  return { url: String(url), init, headers: new Headers(init.headers) };
}

function signIn() {
  setAuth({ mode: "session", session: sessionInfo({ csrf_token: "csrf-1" }) });
}

describe("who a request says it comes from", () => {
  test("without a control plane, telemetry reads send the Connect dialog's key", async () => {
    localStorage.setItem("crucible.connection.key", KEY);
    fetchMock.mockResolvedValue(json({ data: [] }));
    await listTraces("p1", WINDOW);
    expect(sent().headers.get("authorization")).toBe(`Bearer ${KEY}`);
    expect(sent().headers.get("x-crucible-csrf")).toBeNull();
  });

  test("signed in, telemetry reads send the cookie only, even with a key left in the browser", async () => {
    localStorage.setItem("crucible.connection.key", KEY);
    signIn();
    fetchMock.mockResolvedValue(json({ data: [] }));
    await listTraces("p1", WINDOW);
    expect(sent().headers.get("authorization")).toBeNull();
    expect(sent().init.credentials).toBe("same-origin");
    expect(sent().init.method).toBe("GET");
  });

  test("session writes carry the session's CSRF token and never a key", async () => {
    localStorage.setItem("crucible.connection.key", KEY);
    signIn();
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await removeOrgMember("u 1");
    expect(sent().url).toBe("/api/v1/org/members/u%201");
    expect(sent().init.method).toBe("DELETE");
    expect(sent().headers.get("x-crucible-csrf")).toBe("csrf-1");
    expect(sent().headers.get("authorization")).toBeNull();

    fetchMock.mockResolvedValueOnce(json({ invitation: {}, accept_path: "/invite/?token=t" }, 201));
    await createInvitation({ email: "grace@example.com", org_role: "member" });
    expect(sent(1).init.method).toBe("POST");
    expect(sent(1).headers.get("x-crucible-csrf")).toBe("csrf-1");
    expect(sent(1).headers.get("content-type")).toBe("application/json");
    expect(JSON.parse(String(sent(1).init.body))).toEqual({
      email: "grace@example.com",
      org_role: "member",
    });
  });

  test("the sign-in forms send prelogin, even with a session in memory", async () => {
    signIn();
    const session = sessionInfo();
    const calls: [string, () => Promise<unknown>, Response][] = [
      ["/api/v1/auth/login", () => login({ email: "a@example.com", password: "p" }), json(session)],
      [
        "/api/v1/auth/setup",
        () =>
          setupOwner({
            token: "t",
            org_name: "O",
            email: "a@example.com",
            name: "A",
            password: "p",
          }),
        json(session),
      ],
      [
        "/api/v1/auth/invitations/accept",
        () => acceptInvitation({ token: "t", name: "A", password: "p" }),
        json(session),
      ],
      [
        "/api/v1/auth/password/reset-request",
        () => requestPasswordReset({ email: "a@example.com" }),
        new Response(null, { status: 202 }),
      ],
      [
        "/api/v1/auth/password/reset-confirm",
        () => confirmPasswordReset({ token: "t", new_password: "p" }),
        new Response(null, { status: 204 }),
      ],
    ];
    for (const [index, [path, run, answer]] of calls.entries()) {
      fetchMock.mockResolvedValueOnce(answer);
      await run();
      expect(sent(index).url).toBe(path);
      expect(sent(index).init.method).toBe("POST");
      expect(sent(index).headers.get("x-crucible-csrf")).toBe("prelogin");
      expect(sent(index).headers.get("authorization")).toBeNull();
    }
  });

  test("revoking a key sends its version as If-Match", async () => {
    signIn();
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await revokeApiKey("p1", { id: "k1", version: 3 });
    expect(sent().url).toBe("/api/v1/projects/p1/api-keys/k1");
    expect(sent().init.method).toBe("DELETE");
    expect(sent().headers.get("if-match")).toBe('"3"');
  });

  test("sign-out sends the token and accepts an empty answer", async () => {
    signIn();
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await expect(logout()).resolves.toBeUndefined();
    expect(sent().url).toBe("/api/v1/auth/logout");
    expect(sent().headers.get("x-crucible-csrf")).toBe("csrf-1");
  });
});

describe("an ended session", () => {
  test("a 401 `unauthenticated` sends the browser to sign in, coming back to this page", async () => {
    window.history.replaceState(null, "", "/traces/?project=p1&range=7d");
    signIn();
    fetchMock.mockResolvedValue(problem(401, "unauthenticated"));
    const error = await listTraces("p1", WINDOW).catch((reason: unknown) => reason);
    expect(error).toBeInstanceOf(UiError);
    expect((error as UiError).kind).toBe("signed_out");
    expect(loadPage).toHaveBeenCalledWith("/login/?next=%2Ftraces%2F%3Fproject%3Dp1%26range%3D7d");
  });

  test("the session check sends a browser without a session to sign in", async () => {
    window.history.replaceState(null, "", "/settings/members/");
    setAuth({ mode: "unknown" });
    fetchMock.mockResolvedValue(problem(401, "unauthenticated"));
    await expect(getSession()).rejects.toMatchObject({ kind: "signed_out" });
    expect(loadPage).toHaveBeenCalledWith("/login/?next=%2Fsettings%2Fmembers%2F");
  });

  test("a wrong password is not an ended session", async () => {
    signIn();
    fetchMock.mockImplementation(async () => problem(401, "invalid_credentials"));
    await expect(login({ email: "a@example.com", password: "x" })).rejects.toMatchObject({
      code: "invalid_credentials",
    });
    // A session call can answer `invalid_credentials` too, like a password change with the wrong
    // current password.
    await expect(removeOrgMember("u1")).rejects.toMatchObject({ code: "invalid_credentials" });
    expect(loadPage).not.toHaveBeenCalled();
  });

  test("without a control plane, a 401 keeps the Connect flow", async () => {
    fetchMock.mockResolvedValue(problem(401, "unauthenticated"));
    await expect(listTraces("p1", WINDOW)).rejects.toMatchObject({ kind: "unauthenticated" });
    expect(loadPage).not.toHaveBeenCalled();
  });

  test("signed in, a 401 from something else offers sign-in without leaving the page", async () => {
    signIn();
    fetchMock.mockResolvedValue(new Response("Unauthorized", { status: 401 }));
    await expect(listTraces("p1", WINDOW)).rejects.toMatchObject({ kind: "signed_out" });
    expect(loadPage).not.toHaveBeenCalled();
  });
});

describe("whether the server has a control plane", () => {
  const options = { providers: [], password_login_enabled: true, setup_required: false };

  test("sign-in options mean sessions; the check sends no key", async () => {
    localStorage.setItem("crucible.connection.key", KEY);
    fetchMock.mockResolvedValue(json(options));
    await expect(getSignInMode()).resolves.toEqual({ mode: "session", options });
    expect(sent().url).toBe("/api/v1/auth/providers");
    expect(sent().headers.get("authorization")).toBeNull();
  });

  test("404 `feature_disabled` means no control plane: the Connect dialog applies", async () => {
    fetchMock.mockResolvedValue(problem(404, "feature_disabled"));
    await expect(getSignInMode()).resolves.toEqual({ mode: "keys" });
  });

  test.each([
    ["a 404 that isn't feature_disabled", () => problem(404, "not_found"), "not_found"],
    ["a control plane that is down", () => problem(503, "control_plane_unavailable"), "unknown"],
    [
      "a static host answering HTML",
      () =>
        new Response("<!doctype html>", { status: 200, headers: { "content-type": "text/html" } }),
      "unknown",
    ],
  ])("%s leaves the mode unknown", async (_, answer, kind) => {
    fetchMock.mockResolvedValue(answer());
    await expect(getSignInMode()).rejects.toMatchObject({ kind });
  });

  test("an unreachable server leaves the mode unknown", async () => {
    fetchMock.mockRejectedValue(new TypeError("Failed to fetch"));
    await expect(getSignInMode()).rejects.toMatchObject({ kind: "network" });
  });
});
