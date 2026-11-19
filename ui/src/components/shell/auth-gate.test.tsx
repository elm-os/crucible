import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { createQueryClient } from "@/components/providers";
import { currentAuth, setAuth } from "@/lib/auth";
import { loadPage } from "@/lib/navigate";
import { PROJECT_A, PROJECT_B, sessionInfo } from "@/test/session";
import { AppShell } from "./app-shell";

vi.mock("next/navigation", () => import("@/test/navigation"));
vi.mock("@/lib/navigate", () => ({ loadPage: vi.fn() }));

const agent = createAgent();
const fetchMock = vi.fn<typeof fetch>();
const OPTIONS = { providers: [], password_login_enabled: true, setup_required: false };

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/traces/");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.mocked(loadPage).mockReset();
  vi.unstubAllGlobals();
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ code, status, request_id: "req-1" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

function renderShell() {
  const client = createQueryClient();
  // Auto-retry counts are covered in errors.test.ts; here every failure should land at once.
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <AppShell>
        <p>Page body</p>
      </AppShell>
    </QueryClientProvider>,
  );
}

describe("signed in", () => {
  beforeEach(() => {
    setAuth({ mode: "session", session: sessionInfo({ csrf_token: "csrf-9" }) });
  });

  test("the user's name and Sign out replace Connect; an agent can switch projects", async () => {
    window.history.replaceState(null, "", "/trace/?trace=abc&span=def&range=7d");
    renderShell();
    expect(screen.getByText("Page body")).toBeTruthy();
    expect(screen.getByText("Ada Lovelace")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Connect" })).toBeNull();

    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    for (const label of ["Pages", "Project", "Time range", "Sign out"]) {
      expect(labels, `${label} is missing from the snapshot`).toContain(label);
    }

    const result = await agent.act({
      steps: [{ ref: "Project", action: "setValue", args: ["Search"] }],
    });
    expect(result.ok, result.error?.message).toBe(true);
    // Another project's trace and span ids mean nothing, so only the project and range stay.
    expect(window.location.pathname).toBe("/trace/");
    expect(window.location.search).toBe(`?project=${PROJECT_B}&range=7d`);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("without a project in the URL, the session's first project is the default", () => {
    renderShell();
    const project = agent.snapshot().find((entry) => entry.label === "Project");
    expect(project?.value).toBe(PROJECT_A);
  });

  test("sign out sends the session's token, then loads the sign-in page", async () => {
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    renderShell();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
    });
    await waitFor(() => expect(loadPage).toHaveBeenCalledWith("/login/"));
    const [url, init] = fetchMock.mock.calls[0] ?? [];
    expect(url).toBe("/api/v1/auth/logout");
    expect(new Headers(init?.headers).get("x-crucible-csrf")).toBe("csrf-9");
  });

  test("a sign-out that fails says so and stays", async () => {
    fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    renderShell();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Sign out" }));
    });
    expect((await screen.findByRole("alert")).textContent).toContain("Couldn't sign out.");
    expect(loadPage).not.toHaveBeenCalled();
  });
});

describe("the sign-in check", () => {
  beforeEach(() => {
    setAuth({ mode: "unknown" });
  });

  test("a server without a control plane shows the page and the Connect dialog", async () => {
    fetchMock.mockResolvedValueOnce(problem(404, "feature_disabled"));
    renderShell();
    expect(screen.queryByText("Page body")).toBeNull();
    expect(await screen.findByText("Page body")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Connect" })).toBeTruthy();
    expect(currentAuth()).toEqual({ mode: "keys" });
  });

  test("with a control plane, the page waits for the session", async () => {
    fetchMock.mockResolvedValueOnce(json(OPTIONS)).mockResolvedValueOnce(json(sessionInfo()));
    renderShell();
    expect(await screen.findByText("Page body")).toBeTruthy();
    expect(screen.getByText("Ada Lovelace")).toBeTruthy();
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toEqual([
      "/api/v1/auth/providers",
      "/api/v1/auth/session",
    ]);
  });

  test("without a session the browser goes to sign in, and the page never renders", async () => {
    window.history.replaceState(null, "", "/traces/?project=p1");
    fetchMock
      .mockResolvedValueOnce(json(OPTIONS))
      .mockResolvedValueOnce(problem(401, "unauthenticated"));
    renderShell();
    await waitFor(() =>
      expect(loadPage).toHaveBeenCalledWith("/login/?next=%2Ftraces%2F%3Fproject%3Dp1"),
    );
    expect(await screen.findByText("You're signed out.")).toBeTruthy();
    // The static export keeps the trailing slash (trailingSlash: true); here Link may drop it.
    const href = new URL(
      screen.getByRole("link", { name: "Sign in" }).getAttribute("href") ?? "",
      "http://localhost",
    );
    expect(href.pathname).toMatch(/^\/login\/?$/);
    expect(href.searchParams.get("next")).toBe("/traces/?project=p1");
    expect(screen.queryByText("Page body")).toBeNull();
  });

  test("a check that fails shows an error whose retry checks again", async () => {
    fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    renderShell();
    expect(await screen.findByText("Couldn't reach Crucible.")).toBeTruthy();
    expect(screen.queryByText("Page body")).toBeNull();
    fetchMock.mockResolvedValueOnce(problem(404, "feature_disabled"));
    await act(async () => screen.getByRole("button", { name: "Retry connection" }).click());
    expect(await screen.findByText("Page body")).toBeTruthy();
  });
});
