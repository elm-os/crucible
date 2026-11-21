import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Org } from "@/api/generated/Org";
import type { OrgRole } from "@/api/generated/OrgRole";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { sessionInfo } from "@/test/session";
import { parseDomains } from "./domains";
import { OrganizationSettings } from "./organization";

vi.mock("next/navigation", () => import("@/test/navigation"));

// Steps on controls marked `data-agent-confirm` run only once a person approves.
let approve = false;
const agent = createAgent({ allow: (step) => !step.confirm || approve });
const fetchMock = vi.fn<typeof fetch>();
const ORG = "/api/v1/org";

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
const problem =
  (status: number, code: string, errors?: { pointer: string; code: string }[]) => () =>
    new Response(
      JSON.stringify({ code, status, title: "raw server title", request_id: "req-9", errors }),
      { status, headers: { "content-type": "application/problem+json" } },
    );

const org = (fields: Partial<Org> = {}, settings: Partial<Org["settings"]> = {}): Org => ({
  id: "0193a1b2-0000-7000-8000-0000000000aa",
  name: "Analytical Engines",
  settings: {
    signup_mode: "invite_only",
    allowed_email_domains: [],
    password_login_enabled: true,
    project_access: "all",
    ...settings,
  },
  version: 3,
  ...fields,
});

let wide = true;

function signIn(role: OrgRole = "owner") {
  const session = sessionInfo({ csrf_token: "csrf-3" });
  setAuth({ mode: "session", session: { ...session, org: { ...session.org, role } } });
}

function renderPage() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <OrganizationSettings />
    </QueryClientProvider>,
  );
}

const press = (label: string) => agent.act({ steps: [{ ref: label, action: "press" }] });
const fill = (label: string, value: string) =>
  agent.act({ steps: [{ ref: label, action: "setValue", args: [value] }] });

const bodyOf = (init: RequestInit | undefined): unknown => JSON.parse(String(init?.body));

beforeEach(() => {
  wide = true;
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("matchMedia", () => ({
    matches: wide,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
});

afterEach(() => {
  approve = false;
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

describe("domains", () => {
  test("are read as people write them", () => {
    expect(parseDomains("")).toEqual([]);
    expect(parseDomains("  ")).toEqual([]);
    expect(parseDomains("Example.COM, @research.example.com;ops.example.com\nexample.com")).toEqual(
      ["example.com", "research.example.com", "ops.example.com"],
    );
    expect(parseDomains("example.com example.org")).toEqual(["example.com", "example.org"]);
  });

  test("an address or a URL isn't a domain", () => {
    expect(parseDomains("ada@example.com")).toBe("invalid");
    expect(parseDomains("https://example.com")).toBe("invalid");
    expect(parseDomains("example.com, @")).toBe("invalid");
  });
});

describe("organization settings", () => {
  test("success: the stored settings, and a save sends only what changed, with If-Match", async () => {
    signIn();
    serve({
      [`GET ${ORG}`]: json(org({}, { allowed_email_domains: ["example.com"] })),
      [`PATCH ${ORG}`]: json(
        org(
          { version: 4 },
          { allowed_email_domains: ["example.com", "example.org"], project_access: "explicit" },
        ),
      ),
    });
    renderPage();
    const name = await screen.findByLabelText("Organization name");
    expect(name).toHaveProperty("value", "Analytical Engines");
    expect(screen.getByLabelText("Allowed email domains")).toHaveProperty("value", "example.com");
    expect(screen.getByRole("radio", { name: "Every project" })).toHaveProperty(
      "ariaChecked",
      "true",
    );
    expect(screen.getByText("People join by invitation. There is no open sign-up.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Save settings" })).toHaveProperty(
      "ariaDisabled",
      "true",
    );

    await act(() => fill("Allowed email domains", "Example.com, @example.org"));
    await act(() => fill("Project access", "Only projects they're added to"));
    // Explicit access names the route that adds people to a project, which has no page.
    await screen.findByText("POST /api/v1/projects/{project_id}/members");
    await act(() => press("Save settings"));
    await screen.findByText("Saved.");
    const [patch] = requests(`PATCH ${ORG}`);
    expect(new Headers(patch?.headers).get("if-match")).toBe('"3"');
    expect(new Headers(patch?.headers).get("x-crucible-csrf")).toBe("csrf-3");
    expect(bodyOf(patch)).toEqual({
      allowed_email_domains: ["example.com", "example.org"],
      project_access: "explicit",
    });
  });

  test("a renamed organization sends the trimmed name and nothing else", async () => {
    signIn("admin");
    serve({
      [`GET ${ORG}`]: json(org()),
      [`PATCH ${ORG}`]: json(org({ name: "Difference Engines", version: 4 })),
    });
    renderPage();
    await screen.findByLabelText("Organization name");
    await act(() => fill("Organization name", "  Difference Engines "));
    await act(() => press("Save settings"));
    await screen.findByText("Saved.");
    expect(bodyOf(requests(`PATCH ${ORG}`)[0])).toEqual({ name: "Difference Engines" });
    expect(screen.getByLabelText("Organization name")).toHaveProperty(
      "value",
      "Difference Engines",
    );
  });

  test("a version that moved on reloads the stored settings and says why until the form is edited", async () => {
    signIn();
    let stored = org();
    serve({
      [`GET ${ORG}`]: () => json(stored)(),
      [`PATCH ${ORG}`]: () => {
        stored = org({ name: "Difference Engines", version: 8 });
        return problem(412, "precondition_failed")();
      },
    });
    renderPage();
    await screen.findByLabelText("Organization name");
    await act(() => fill("Organization name", "Babbage and Co"));
    await act(() => press("Save settings"));
    await waitFor(() =>
      expect(screen.getByLabelText("Organization name")).toHaveProperty(
        "value",
        "Difference Engines",
      ),
    );
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("These settings changed since the page loaded.");
    expect(alert.textContent).toContain("Enter your change again, then save.");
    expect(alert.textContent).toContain("req-9");
    expect(alert.textContent).not.toContain("raw server title");

    await act(() => fill("Organization name", "Babbage and Co"));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  });

  test("turning password sign-in off needs a person, and a refusal says what to turn on first", async () => {
    signIn();
    let refuse = true;
    serve({
      [`GET ${ORG}`]: json(org()),
      [`PATCH ${ORG}`]: () =>
        refuse
          ? problem(409, "conflict")()
          : json(org({ version: 4 }, { password_login_enabled: false }))(),
    });
    renderPage();
    const toggle = await screen.findByRole("switch", { name: "Password sign-in" });
    expect(toggle).toHaveProperty("ariaChecked", "true");
    await act(() =>
      agent.act({ steps: [{ ref: "Password sign-in", action: "setChecked", args: [false] }] }),
    );
    expect(toggle).toHaveProperty("ariaChecked", "false");
    expect((await press("Save settings")).ok).toBe(false);
    expect(requests(`PATCH ${ORG}`)).toHaveLength(0);
    approve = true;
    await act(() => press("Save settings"));
    // The server keeps password sign-in on while no provider is offered: the page says what to do.
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Password sign-in is the only way to sign in.");
    expect(alert.textContent).toContain("Turn on a sign-in provider on the Sign-in page first");
    expect(alert.textContent).not.toContain("raw server title");
    expect(
      screen.getByText("Turn on a sign-in provider first.", {
        selector: "[data-slot=field-error]",
      }),
    ).toBeTruthy();
    expect(bodyOf(requests(`PATCH ${ORG}`)[0])).toEqual({ password_login_enabled: false });
    expect(new Headers(requests(`PATCH ${ORG}`)[0]?.headers).get("if-match")).toBe('"3"');
    expect(screen.getByRole("link", { name: "Sign-in page" }).getAttribute("href")).toMatch(
      /^\/settings\/sign-in\/?$/,
    );

    refuse = false;
    await act(() => press("Save settings"));
    await screen.findByText("Saved.");
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByRole("switch", { name: "Password sign-in" })).toHaveProperty(
      "ariaChecked",
      "false",
    );
  });

  test("a domain list that isn't one says so before anything is sent", async () => {
    signIn();
    serve({ [`GET ${ORG}`]: json(org()) });
    renderPage();
    await screen.findByLabelText("Allowed email domains");
    await act(() => fill("Allowed email domains", "ada@example.com"));
    await screen.findByText("Enter domains only, like example.com, separated by commas.", {
      selector: "[data-slot=field-error]",
    });
    await act(() => press("Save settings"));
    expect(requests(`PATCH ${ORG}`)).toHaveLength(0);
  });

  test("a field the server refuses is marked, with the server's text kept out", async () => {
    signIn();
    serve({
      [`GET ${ORG}`]: json(org()),
      [`PATCH ${ORG}`]: problem(422, "validation_failed", [
        { pointer: "/allowed_email_domains", code: "invalid" },
      ]),
    });
    renderPage();
    await screen.findByLabelText("Allowed email domains");
    await act(() => fill("Allowed email domains", "example.com"));
    await act(() => press("Save settings"));
    // The field's error, not the copy the page announces to screen readers.
    await screen.findByText(
      "The server refused these domains. Enter bare domains, like example.com.",
      { selector: "[data-slot=field-error]" },
    );
    expect(screen.getByRole("alert").textContent).toContain("Some fields need attention.");
    expect(document.body.textContent).not.toContain("raw server title");
  });

  test("error: the section says so with a retry that reloads it", async () => {
    signIn();
    let fail = true;
    serve({
      [`GET ${ORG}`]: () => (fail ? problem(503, "control_plane_unavailable")() : json(org())()),
    });
    renderPage();
    const retry = await screen.findByRole("button", { name: "Retry organization settings" });
    expect(screen.queryByLabelText("Organization name")).toBeNull();
    expect(document.body.textContent).not.toContain("raw server title");
    fail = false;
    await act(async () => retry.click());
    await screen.findByLabelText("Organization name");
  });

  test("a member reads the settings and gets no save control", async () => {
    signIn("member");
    serve({ [`GET ${ORG}`]: json(org({}, { project_access: "explicit" })) });
    renderPage();
    const name = await screen.findByLabelText("Organization name");
    expect(name).toHaveProperty("readOnly", true);
    expect(screen.getByLabelText("Allowed email domains")).toHaveProperty("readOnly", true);
    expect(screen.getByRole("radiogroup", { name: "Project access" }).dataset.readonly).toBe("");
    expect(screen.getByRole("switch", { name: "Password sign-in" }).dataset.readonly).toBe("");
    expect(screen.queryByRole("button", { name: "Save settings" })).toBeNull();
    await screen.findByText("Only organization owners and admins change these settings.");
  });

  test("on a narrow screen an owner reads the settings, and editing waits for a wider one", async () => {
    wide = false;
    signIn();
    serve({ [`GET ${ORG}`]: json(org()) });
    renderPage();
    const name = await screen.findByLabelText("Organization name");
    expect(name).toHaveProperty("readOnly", true);
    expect(screen.queryByRole("button", { name: "Save settings" })).toBeNull();
    await screen.findByText("Editing needs a wider screen.");
  });

  test("without a control plane the page says which setting turns it on and requests nothing", () => {
    setAuth({ mode: "keys" });
    renderPage();
    expect(screen.getByText("Organization settings need a control plane.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("an agent finds every setting by name", async () => {
    signIn();
    serve({ [`GET ${ORG}`]: json(org()) });
    renderPage();
    await screen.findByLabelText("Organization name");
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    for (const label of [
      "Organization name",
      "Project access",
      "Password sign-in",
      "Allowed email domains",
      "Save settings",
    ]) {
      expect(labels, `${label} is missing from the snapshot`).toContain(label);
    }
  });
});
