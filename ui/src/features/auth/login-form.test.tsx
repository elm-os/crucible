import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { SignInOptions } from "@/api/generated/SignInOptions";
import { createQueryClient } from "@/components/providers";
import { loadPage } from "@/lib/navigate";
import { sessionInfo } from "@/test/session";
import { LoginForm } from "./login-form";

vi.mock("next/navigation", () => import("@/test/navigation"));
vi.mock("@/lib/navigate", () => ({ loadPage: vi.fn() }));

const agent = createAgent();
const fetchMock = vi.fn<typeof fetch>();
const SERVER_TEXT = "raw server detail";

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
  new Response(
    JSON.stringify({ code, status, title: SERVER_TEXT, detail: SERVER_TEXT, request_id: "req-7" }),
    { status, headers: { "content-type": "application/problem+json", "retry-after": "30" } },
  );

function options(fields: Partial<SignInOptions> = {}): SignInOptions {
  return {
    providers: [{ id: "p-1", kind: "oidc", display_name: "Continue with Acme SSO" }],
    password_login_enabled: true,
    setup_required: false,
    ...fields,
  };
}

function renderLogin(search = "") {
  window.history.replaceState(null, "", `/login/${search}`);
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <LoginForm />
    </QueryClientProvider>,
  );
}

async function signInAs(email: string, password: string) {
  const result = await agent.act({
    steps: [
      { ref: "Email", action: "setValue", args: [email] },
      { ref: "Password", action: "setValue", args: [password] },
      { ref: "Sign in", action: "press" },
    ],
  });
  expect(result.ok, result.error?.message).toBe(true);
}

describe("password sign-in", () => {
  test("an agent finds every control, and signing in goes back to the page that asked", async () => {
    fetchMock.mockResolvedValueOnce(json(options()));
    renderLogin("?next=%2Ftraces%2F%3Fproject%3Dp1");
    await screen.findByLabelText("Email");
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    for (const label of ["Email", "Password", "Sign in", "Continue with Acme SSO"]) {
      expect(labels, `${label} is missing from the snapshot`).toContain(label);
    }

    fetchMock.mockResolvedValueOnce(json(sessionInfo()));
    await signInAs("  ada@example.com ", "correct horse battery staple");
    await waitFor(() => expect(loadPage).toHaveBeenCalledWith("/traces/?project=p1"));
    const [url, init] = fetchMock.mock.calls[1] ?? [];
    expect(url).toBe("/api/v1/auth/login");
    expect(new Headers(init?.headers).get("x-crucible-csrf")).toBe("prelogin");
    expect(JSON.parse(String(init?.body))).toEqual({
      email: "ada@example.com",
      password: "correct horse battery staple",
    });
  });

  test("a next that leaves this server is replaced by the start page", async () => {
    fetchMock.mockResolvedValueOnce(json(options()));
    renderLogin("?next=%2F%2Fevil.example%2Fsteal");
    await screen.findByLabelText("Email");
    expect(screen.getByRole("link", { name: "Continue with Acme SSO" }).getAttribute("href")).toBe(
      "/api/v1/auth/oauth/p-1/start?next=%2F",
    );
    fetchMock.mockResolvedValueOnce(json(sessionInfo()));
    await signInAs("ada@example.com", "correct horse battery staple");
    await waitFor(() => expect(loadPage).toHaveBeenCalledWith("/"));
  });

  test.each([
    [401, "invalid_credentials", "That email and password don't match an account."],
    [429, "rate_limited", "Too many attempts."],
    [403, "origin_invalid", "Crucible refused a request from this address."],
    [503, "control_plane_unavailable", "Crucible can't reach its database."],
  ])("a %i %s says why, in the page's own words", async (status, code, title) => {
    fetchMock.mockResolvedValueOnce(json(options()));
    renderLogin();
    await screen.findByLabelText("Email");
    fetchMock.mockResolvedValueOnce(problem(status, code));
    await signInAs("ada@example.com", "wrong password here");
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain(title);
    expect(alert.textContent).toContain("req-7");
    expect(alert.textContent).not.toContain(SERVER_TEXT);
    if (code === "rate_limited") expect(alert.textContent).toContain("Wait 30 seconds");
    expect(loadPage).not.toHaveBeenCalled();
  });
});

describe("what the page offers", () => {
  test("provider sign-in starts at the server with this page's next", async () => {
    fetchMock.mockResolvedValueOnce(json(options({ password_login_enabled: false })));
    renderLogin("?next=%2Fsettings%2Fmembers%2F");
    const link = await screen.findByRole("link", { name: "Continue with Acme SSO" });
    expect(link.getAttribute("href")).toBe(
      "/api/v1/auth/oauth/p-1/start?next=%2Fsettings%2Fmembers%2F",
    );
    expect(screen.queryByLabelText("Email")).toBeNull();
    expect(screen.queryByRole("link", { name: "Forgot your password?" })).toBeNull();
  });

  test("a server that has no owner yet points to setup", async () => {
    fetchMock.mockResolvedValueOnce(json(options({ setup_required: true })));
    renderLogin();
    expect(await screen.findByText("Crucible isn't set up yet.")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Set up Crucible" })).toBeTruthy();
    expect(screen.queryByLabelText("Email")).toBeNull();
  });

  test("a server without a control plane has no sign-in", async () => {
    fetchMock.mockResolvedValueOnce(problem(404, "feature_disabled"));
    renderLogin();
    expect(await screen.findByText("This server has no sign-in.")).toBeTruthy();
    expect(screen.queryByLabelText("Email")).toBeNull();
  });

  test("options that fail to load say so, and Try again loads them", async () => {
    fetchMock.mockResolvedValueOnce(problem(503, "control_plane_unavailable"));
    renderLogin();
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Crucible can't reach its database.",
    );
    fetchMock.mockResolvedValueOnce(json(options()));
    await act(async () => screen.getByRole("button", { name: "Try again" }).click());
    expect(await screen.findByLabelText("Email")).toBeTruthy();
  });
});

describe("a provider sign-in that came back with an error", () => {
  test.each([
    ["oauth_denied", "Sign-in was cancelled."],
    ["oauth_state_invalid", "That sign-in attempt expired."],
    ["oauth_exchange_failed", "The sign-in provider didn't confirm who you are."],
    ["email_unverified", "Your email isn't verified with the sign-in provider."],
    ["not_invited", "This account hasn't been invited."],
    ["account_disabled", "This account is disabled."],
    ["provider_disabled", "That sign-in option is turned off."],
    ["sso_required", "Your organization signs in with its own provider."],
    ["something_new", "Couldn't sign you in."],
    ["constructor", "Couldn't sign you in."],
  ])("?error=%s says %j", async (code, title) => {
    fetchMock.mockResolvedValueOnce(json(options()));
    renderLogin(`?error=${code}`);
    expect((await screen.findByRole("alert")).textContent).toContain(title);
    expect(screen.queryByText(code)).toBeNull();
  });
});
