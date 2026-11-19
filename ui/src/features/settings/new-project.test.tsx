import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { OrgRole } from "@/api/generated/OrgRole";
import type { Project } from "@/api/generated/Project";
import { createQueryClient } from "@/components/providers";
import { currentAuth, setAuth } from "@/lib/auth";
import { loadPage } from "@/lib/navigate";
import { sessionInfo, sessionProject } from "@/test/session";
import { NewProjectControl } from "./new-project";

const { push } = vi.hoisted(() => ({ push: vi.fn() }));
vi.mock("next/navigation", async () => ({
  ...(await import("@/test/navigation")),
  useRouter: () => ({ push, replace: vi.fn(), prefetch: vi.fn() }),
}));
vi.mock("@/lib/navigate", () => ({ loadPage: vi.fn() }));

const agent = createAgent();
const fetchMock = vi.fn<typeof fetch>();
const NEW_ID = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a3d";
const SERVER_TEXT = "raw server detail";

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  push.mockReset();
  vi.mocked(loadPage).mockReset();
  vi.unstubAllGlobals();
});

function signIn(role: OrgRole = "owner") {
  const session = sessionInfo({ csrf_token: "csrf-7" });
  setAuth({ mode: "session", session: { ...session, org: { ...session.org, role } } });
}

function renderControl() {
  return render(
    <QueryClientProvider client={createQueryClient()}>
      <NewProjectControl />
    </QueryClientProvider>,
  );
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const problem = (status: number, code: string, pointer?: string) =>
  new Response(
    JSON.stringify({
      code,
      status,
      title: SERVER_TEXT,
      detail: SERVER_TEXT,
      request_id: "req-3",
      ...(pointer ? { errors: [{ pointer, code: "invalid", message: SERVER_TEXT }] } : {}),
    }),
    { status, headers: { "content-type": "application/problem+json" } },
  );

const created: Project = {
  id: NEW_ID,
  name: "Payments API",
  slug: "payments",
  role: "admin",
  version: 1,
};

function input(label: string): HTMLInputElement {
  return screen.getByLabelText(label);
}

async function act(steps: { ref: string; action: string; args?: unknown[] }[]) {
  const result = await agent.act({ steps });
  expect(result.ok, result.error?.message).toBe(true);
}

async function fillAndCreate(name = "Payments API", slug?: string) {
  await act([
    { ref: "New project", action: "press" },
    { ref: "Name", action: "setValue", args: [name] },
    ...(slug === undefined ? [] : [{ ref: "Slug", action: "setValue", args: [slug] }]),
    { ref: "Create project", action: "press" },
  ]);
}

describe("who can create projects", () => {
  test.each(["owner", "admin"] as const)("an org %s gets New project", (role) => {
    signIn(role);
    renderControl();
    expect(screen.getByRole("button", { name: "New project" })).toBeTruthy();
  });

  test.each(["member", "viewer"] as const)("an org %s doesn't", (role) => {
    signIn(role);
    renderControl();
    expect(screen.queryByRole("button", { name: "New project" })).toBeNull();
  });

  test("a server without a control plane has no projects to create", () => {
    renderControl();
    expect(screen.queryByRole("button", { name: "New project" })).toBeNull();
  });
});

describe("creating a project", () => {
  test("an agent creates one: the slug follows the name until edited, then the session refreshes and its API keys page opens", async () => {
    window.history.replaceState(null, "", "/settings/members/?range=7d");
    signIn();
    renderControl();
    await act([
      { ref: "New project", action: "press" },
      { ref: "Name", action: "setValue", args: ["Payments API"] },
    ]);
    expect(input("Slug").value).toBe("payments-api");
    await act([
      { ref: "Slug", action: "setValue", args: ["Payments"] },
      { ref: "Name", action: "setValue", args: ["Payments API v2"] },
    ]);
    expect(input("Slug").value).toBe("payments");

    const session = sessionInfo();
    fetchMock
      .mockResolvedValueOnce(json({ ...created, name: "Payments API v2" }, 201))
      .mockResolvedValueOnce(
        json({ ...session, projects: [...session.projects, sessionProject({ id: NEW_ID })] }),
      );
    await act([{ ref: "Create project", action: "press" }]);
    await waitFor(() =>
      expect(push).toHaveBeenCalledWith(`/settings/api-keys?project=${NEW_ID}&range=7d`),
    );

    const [url, init] = fetchMock.mock.calls[0] ?? [];
    expect(url).toBe("/api/v1/projects");
    expect(init?.method).toBe("POST");
    expect(new Headers(init?.headers).get("x-crucible-csrf")).toBe("csrf-7");
    expect(JSON.parse(String(init?.body))).toEqual({ name: "Payments API v2", slug: "payments" });
    expect(String(fetchMock.mock.calls[1]?.[0])).toBe("/api/v1/auth/session");
    const auth = currentAuth();
    expect(auth.mode === "session" && auth.session.projects.map((project) => project.id)).toContain(
      NEW_ID,
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(screen.getByText("Created Payments API v2. Its API keys page is open.")).toBeTruthy();
  });

  test("a slug that breaks the rules is refused before anything is sent", async () => {
    signIn();
    renderControl();
    await fillAndCreate("Payments API", "payments_api");
    expect(await screen.findByText(/Use only lowercase letters, digits and hyphens/)).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("when the session can't be refreshed, the new project's API keys page loads fresh", async () => {
    window.history.replaceState(null, "", "/settings/api-keys/");
    signIn();
    renderControl();
    fetchMock
      .mockResolvedValueOnce(json(created, 201))
      .mockRejectedValueOnce(new TypeError("Failed to fetch"));
    await fillAndCreate();
    await waitFor(() =>
      expect(loadPage).toHaveBeenCalledWith(`/settings/api-keys?project=${NEW_ID}`),
    );
    expect(push).not.toHaveBeenCalled();
  });
});

describe("what a refusal says", () => {
  test.each([
    [409, "conflict", undefined, "Another project already uses that slug.", "Slug"],
    [422, "validation_failed", "/name", "Some fields need attention.", "Name"],
    [422, "validation_failed", "/slug", "Some fields need attention.", "Slug"],
    [403, "forbidden", undefined, "Your role can't create projects.", null],
  ])("a %i %s (%s) says %j and marks the field", async (status, code, pointer, title, field) => {
    signIn();
    renderControl();
    fetchMock.mockResolvedValueOnce(problem(status, code, pointer));
    await fillAndCreate();
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain(title);
    expect(alert.textContent).toContain("req-3");
    expect(screen.queryByText(SERVER_TEXT)).toBeNull();
    for (const label of ["Name", "Slug"]) {
      expect(input(label).getAttribute("aria-invalid"), label).toBe(
        label === field ? "true" : null,
      );
    }
    expect(push).not.toHaveBeenCalled();
  });

  test("typing again clears the refusal", async () => {
    signIn();
    renderControl();
    fetchMock.mockResolvedValueOnce(problem(409, "conflict"));
    await fillAndCreate();
    expect(await screen.findByText("Choose a slug no other project uses.")).toBeTruthy();
    await act([{ ref: "Slug", action: "setValue", args: ["payments-2"] }]);
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(input("Slug").getAttribute("aria-invalid")).toBeNull();
  });
});
