import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { OrgRole } from "@/api/generated/OrgRole";
import type { Project } from "@/api/generated/Project";
import { createQueryClient } from "@/components/providers";
import { ProjectSwitcher } from "@/components/shell/project-switcher";
import { currentAuth, setAuth } from "@/lib/auth";
import { PROJECT_A, PROJECT_B, sessionInfo } from "@/test/session";
import { ProjectSettings } from "./project";

vi.mock("next/navigation", () => import("@/test/navigation"));

const agent = createAgent();
const fetchMock = vi.fn<typeof fetch>();
const SERVER_TEXT = "raw server detail";
const LIST = "/api/v1/projects";
const ONE = `/api/v1/projects/${PROJECT_A}`;

type Handler = (init: RequestInit | undefined) => Response;

function serve(routes: Record<string, Handler>) {
  fetchMock.mockImplementation(async (input, init) => {
    const route = `${init?.method ?? "GET"} ${String(input)}`;
    const handler = routes[route];
    if (!handler) throw new Error(`no route for ${route}`);
    return handler(init);
  });
}

function requests(route: string): RequestInit[] {
  return fetchMock.mock.calls
    .filter(([input, init]) => `${init?.method ?? "GET"} ${String(input)}` === route)
    .map(([, init]) => init ?? {});
}

const json =
  (body: unknown, status = 200) =>
  () =>
    new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const problem = (status: number, code: string, pointer?: string) => () =>
  new Response(
    JSON.stringify({
      code,
      status,
      title: SERVER_TEXT,
      detail: SERVER_TEXT,
      request_id: "req-7",
      ...(pointer ? { errors: [{ pointer, code: "invalid", message: SERVER_TEXT }] } : {}),
    }),
    { status, headers: { "content-type": "application/problem+json" } },
  );

const project = (fields: Partial<Project> = {}): Project => ({
  id: PROJECT_A,
  name: "Checkout",
  slug: "checkout",
  role: "admin",
  version: 3,
  ...fields,
});

const SEARCH = project({
  id: PROJECT_B,
  name: "Search",
  slug: "search",
  role: "viewer",
  version: 1,
});

function signIn(fields: { role?: OrgRole; projects?: [] } = {}) {
  const session = sessionInfo({ csrf_token: "csrf-5" });
  setAuth({
    mode: "session",
    session: {
      ...session,
      org: { ...session.org, role: fields.role ?? "owner" },
      projects: fields.projects ?? session.projects,
    },
  });
}

function renderPage(selected = PROJECT_A) {
  window.history.replaceState(null, "", `/settings/project/?project=${selected}`);
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <ProjectSettings />
    </QueryClientProvider>,
  );
}

async function run(steps: { ref: string; action: string; args?: unknown[] }[]) {
  const result = await act(() => agent.act({ steps }));
  expect(result.ok, result.error?.message).toBe(true);
}

const rename = (name: string) =>
  run([
    { ref: "Project name", action: "setValue", args: [name] },
    { ref: "Rename", action: "press" },
  ]);

const nameField = () => screen.getByLabelText<HTMLInputElement>("Project name");
const sessionName = (id: string) => {
  const auth = currentAuth();
  return auth.mode === "session" ? auth.session.projects.find((row) => row.id === id)?.name : null;
};

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(window.navigator, "clipboard");
});

describe("project details", () => {
  test("loading: the details section is busy while the list loads, and the pointers already show", () => {
    signIn();
    fetchMock.mockReturnValue(new Promise<Response>(() => {}));
    renderPage();
    expect(screen.getByRole("region", { name: "Project details" }).getAttribute("aria-busy")).toBe(
      "true",
    );
    expect(screen.getByRole("link", { name: "Retention" })).toBeTruthy();
  });

  test("success: the name, slug and id, the pointers, and every control named for an agent", async () => {
    signIn();
    serve({ [`GET ${LIST}`]: json({ data: [project(), SEARCH] }) });
    renderPage();
    expect(await screen.findByDisplayValue("Checkout")).toBe(nameField());
    expect(screen.getByText("checkout", { selector: "dd" })).toBeTruthy();
    expect(screen.getByText(PROJECT_A, { selector: "span" })).toBeTruthy();
    expect(screen.getByText(`/api/v1/projects/${PROJECT_A}/traces`)).toBeTruthy();
    expect(screen.getByText("crucible export --project checkout")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Retention" }).getAttribute("href")).toBe(
      `/settings/retention?project=${PROJECT_A}`,
    );
    expect(screen.getByText("DELETE /api/v1/projects/{project_id}")).toBeTruthy();
    // Nothing changed yet, so there is nothing to rename.
    expect(screen.getByRole("button", { name: "Rename" }).getAttribute("aria-disabled")).toBe(
      "true",
    );

    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    for (const label of ["Project name", "Rename", "Copy id"]) {
      expect(labels, `${label} is missing from the snapshot`).toContain(label);
    }
  });

  test("error: the details say so with a retry that reads the list again", async () => {
    signIn();
    let fail = true;
    serve({
      [`GET ${LIST}`]: () =>
        fail ? problem(503, "control_plane_unavailable")() : json({ data: [project()] })(),
    });
    renderPage();
    const retry = await screen.findByRole("button", { name: "Retry project details" });
    expect(screen.getByText("This section couldn't load.")).toBeTruthy();
    expect(screen.getByText("req-7")).toBeTruthy();
    expect(screen.queryByText(SERVER_TEXT)).toBeNull();
    fail = false;
    await act(async () => retry.click());
    expect(await screen.findByDisplayValue("Checkout")).toBeTruthy();
    expect(requests(`GET ${LIST}`)).toHaveLength(2);
  });

  test("empty: a project the list no longer has says so", async () => {
    signIn();
    serve({ [`GET ${LIST}`]: json({ data: [SEARCH] }) });
    renderPage();
    expect(await screen.findByText("This project isn't available anymore.")).toBeTruthy();
    expect(screen.queryByLabelText("Project name")).toBeNull();
  });

  test.each([
    ["owner", "This organization has no projects yet."],
    ["member", "You aren't in any project yet."],
  ] as const)(
    "empty: an org %s with no projects is told what to do, and nothing is requested",
    (role, title) => {
      signIn({ role, projects: [] });
      renderPage();
      expect(screen.getByText(title)).toBeTruthy();
      expect(fetchMock).not.toHaveBeenCalled();
    },
  );

  test("a project outside the session, or a server without a control plane, requests nothing", () => {
    signIn();
    const { unmount } = renderPage("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0fff");
    expect(screen.getByText("This project isn't available to you.")).toBeTruthy();
    unmount();
    setAuth({ mode: "keys" });
    renderPage();
    expect(screen.getByText("Project settings need a control plane.")).toBeTruthy();
    expect(screen.getByText(/CRUCIBLE_POSTGRES_URL/)).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("read-only: a viewer sees the details and who can rename, without the control", async () => {
    signIn();
    serve({ [`GET ${LIST}`]: json({ data: [project(), SEARCH] }) });
    renderPage(PROJECT_B);
    expect(await screen.findByText("Search", { selector: "dd" })).toBeTruthy();
    expect(
      screen.getByText("Only project admins rename a project. Ask an admin of Search."),
    ).toBeTruthy();
    expect(screen.queryByLabelText("Project name")).toBeNull();
    expect(screen.queryByRole("button", { name: "Rename" })).toBeNull();
    expect(screen.getByRole("button", { name: "Copy id" })).toBeTruthy();
  });

  test("copy id: says it copied, or how to copy by hand where the browser has no clipboard", async () => {
    signIn();
    serve({ [`GET ${LIST}`]: json({ data: [project()] }) });
    renderPage();
    await screen.findByDisplayValue("Checkout");
    await run([{ ref: "Copy id", action: "press" }]);
    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      "Couldn't copy here. Select the id and copy it yourself.",
    );

    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(window.navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    await run([{ ref: "Copy id", action: "press" }]);
    expect(await screen.findByText("Copied.")).toBeTruthy();
    expect(writeText).toHaveBeenCalledWith(PROJECT_A);
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

describe("renaming", () => {
  test("success: sends the name with If-Match and CSRF, says so, and the project menu gets the new name", async () => {
    signIn();
    serve({
      [`GET ${LIST}`]: json({ data: [project(), SEARCH] }),
      [`PATCH ${ONE}`]: json(project({ name: "Payments", version: 4 })),
    });
    renderPage();
    await screen.findByDisplayValue("Checkout");
    await rename("  Payments ");
    expect(await screen.findByText(/Renamed\. The project menu shows the new name\./)).toBeTruthy();
    const [patch] = requests(`PATCH ${ONE}`);
    expect(JSON.parse(String(patch?.body))).toEqual({ name: "Payments" });
    expect(new Headers(patch?.headers).get("if-match")).toBe('"3"');
    expect(new Headers(patch?.headers).get("x-crucible-csrf")).toBe("csrf-5");
    expect(nameField().value).toBe("Payments");
    expect(sessionName(PROJECT_A)).toBe("Payments");
    expect(screen.getByRole("button", { name: "Rename" }).getAttribute("aria-disabled")).toBe(
      "true",
    );

    // Editing again clears the confirmation.
    await run([{ ref: "Project name", action: "setValue", args: ["Payments EU"] }]);
    await waitFor(() => expect(screen.queryByText(/Renamed\./)).toBeNull());
  });

  test("412: someone else's rename reloads the project and says so, and the next rename sends the new version", async () => {
    signIn();
    let stored = [project(), SEARCH];
    serve({
      [`GET ${LIST}`]: () => json({ data: stored })(),
      [`PATCH ${ONE}`]: (init) => {
        if (new Headers(init?.headers).get("if-match") === '"3"') {
          stored = [project({ name: "Checkout EU", version: 4 }), SEARCH];
          return problem(412, "precondition_failed")();
        }
        return json(project({ name: "Payments", version: 5 }))();
      },
    });
    renderPage();
    await screen.findByDisplayValue("Checkout");
    await rename("Payments");
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Someone renamed this project since the page loaded.");
    expect(alert.textContent).not.toContain("req-7");
    expect(nameField().value).toBe("Checkout EU");
    expect(requests(`GET ${LIST}`)).toHaveLength(2);
    expect(sessionName(PROJECT_A)).toBe("Checkout EU");

    await rename("Payments");
    expect(await screen.findByText(/Renamed\./)).toBeTruthy();
    expect(new Headers(requests(`PATCH ${ONE}`)[1]?.headers).get("if-match")).toBe('"4"');
    expect(screen.queryByRole("alert")).toBeNull();
  });

  test("a rename made in another session reaches the project menu once the list shows it", async () => {
    signIn();
    serve({
      [`GET ${LIST}`]: json({ data: [project({ name: "Checkout EU", version: 4 }), SEARCH] }),
    });
    window.history.replaceState(null, "", `/settings/project/?project=${PROJECT_A}`);
    render(
      <QueryClientProvider client={createQueryClient()}>
        <ProjectSwitcher />
        <ProjectSettings />
      </QueryClientProvider>,
    );
    const menu = screen.getByRole("combobox", { name: "Project" });
    expect(menu.textContent).toBe("Checkout");
    expect(await screen.findByDisplayValue("Checkout EU")).toBe(nameField());
    await waitFor(() => expect(menu.textContent).toBe("Checkout EU"));
    expect(sessionName(PROJECT_A)).toBe("Checkout EU");
    expect(sessionName(PROJECT_B)).toBe("Search");
  });

  test("403: a role that lost the admin role is told who renames, and nothing else changes", async () => {
    signIn();
    serve({
      [`GET ${LIST}`]: json({ data: [project()] }),
      [`PATCH ${ONE}`]: problem(403, "forbidden"),
    });
    renderPage();
    await screen.findByDisplayValue("Checkout");
    await rename("Payments");
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Only project admins rename a project.");
    expect(alert.textContent).not.toContain("req-7");
    expect(screen.queryByText(SERVER_TEXT)).toBeNull();
    expect(sessionName(PROJECT_A)).toBe("Checkout");
  });

  test("validation: a blank name is refused before sending, and a name the server refuses marks the field", async () => {
    signIn();
    serve({
      [`GET ${LIST}`]: json({ data: [project()] }),
      [`PATCH ${ONE}`]: problem(422, "validation_failed", "/name"),
    });
    renderPage();
    await screen.findByDisplayValue("Checkout");
    await rename("   ");
    expect(await screen.findAllByText("Enter a name for the project.")).not.toHaveLength(0);
    expect(requests(`PATCH ${ONE}`)).toHaveLength(0);

    await rename("Payments");
    expect(await screen.findAllByText("Use 1 to 200 characters.")).not.toHaveLength(0);
    expect(nameField().getAttribute("aria-invalid")).toBe("true");
    expect(screen.getByRole("alert").textContent).toContain("Some fields need attention.");
    expect(screen.queryByText(SERVER_TEXT)).toBeNull();

    await run([{ ref: "Project name", action: "setValue", args: ["Payments 2"] }]);
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(nameField().getAttribute("aria-invalid")).toBeNull();
  });
});
