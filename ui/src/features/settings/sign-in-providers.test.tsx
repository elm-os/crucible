import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { AuthProvider } from "@/api/generated/AuthProvider";
import type { OrgRole } from "@/api/generated/OrgRole";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { sessionInfo } from "@/test/session";
import { providerState, SignInSettings } from "./sign-in-providers";

vi.mock("next/navigation", () => import("@/test/navigation"));

// Steps on controls marked `data-agent-confirm` run only once a person approves.
let approve = false;
const agent = createAgent({ allow: (step) => !step.confirm || approve });
const fetchMock = vi.fn<typeof fetch>();
const PROVIDERS = "/api/v1/org/auth-providers";
const SSO_ID = "0193a1b2-0000-7000-8000-0000000000c1";
const CALLBACK = `https://crucible.example.com/api/v1/auth/oauth/${SSO_ID}/callback`;

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
const empty = () => new Response(null, { status: 204 });
const problem =
  (status: number, code: string, errors?: { pointer: string; code: string }[]) => () =>
    new Response(
      JSON.stringify({ code, status, title: "raw server title", request_id: "req-9", errors }),
      { status, headers: { "content-type": "application/problem+json" } },
    );
const bodyOf = (init: RequestInit | undefined): unknown => JSON.parse(String(init?.body));

const provider = (fields: Partial<AuthProvider> = {}): AuthProvider => ({
  id: SSO_ID,
  kind: "oidc",
  display_name: "Continue with Acme SSO",
  issuer: "https://login.acme.example",
  client_id: "crucible-web",
  scopes: [],
  allowed_domains: ["acme.example"],
  enabled: true,
  seeded: false,
  redirect_uri: CALLBACK,
  ...fields,
});

const GITHUB = provider({
  id: "0193a1b2-0000-7000-8000-0000000000c2",
  kind: "github",
  display_name: "Continue with GitHub",
  issuer: "",
  client_id: "Iv1.0123456789abcdef",
  allowed_domains: [],
  redirect_uri:
    "https://crucible.example.com/api/v1/auth/oauth/0193a1b2-0000-7000-8000-0000000000c2/callback",
});

let wide = true;

function signIn(role: OrgRole = "owner") {
  const session = sessionInfo({ csrf_token: "csrf-4" });
  setAuth({ mode: "session", session: { ...session, org: { ...session.org, role } } });
}

function renderPage() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <SignInSettings />
    </QueryClientProvider>,
  );
}

const press = (label: string) => agent.act({ steps: [{ ref: label, action: "press" }] });
const fill = (label: string, value: string) =>
  agent.act({ steps: [{ ref: label, action: "setValue", args: [value] }] });

function dialogRef(title: string): string {
  const dialog = agent.snapshot().find((entry) => entry.label === title)?.ref;
  if (dialog === undefined) throw new Error(`no dialog named ${title}`);
  return dialog;
}

// Presses a control inside the dialog with this title.
const pressIn = (title: string, label: string) =>
  agent.act({ steps: [{ ref: dialogRef(title), action: "press", args: [label] }] });

const row = (name: string) => {
  const item = document.querySelector<HTMLElement>(`[data-provider="${name}"]`);
  if (item === null) throw new Error(`no row for ${name}`);
  return within(item);
};

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

describe("provider state", () => {
  test("the sign-in page offers a provider only when it is on and has a callback URL", () => {
    expect(providerState({ enabled: true, redirect_uri: CALLBACK })).toBe("on");
    expect(providerState({ enabled: true, redirect_uri: "" })).toBe("needs_public_url");
    expect(providerState({ enabled: false, redirect_uri: CALLBACK })).toBe("off");
    expect(providerState({ enabled: false, redirect_uri: "" })).toBe("off");
  });
});

describe("sign-in providers", () => {
  test("success: each provider with its state, details and callback URL, never its secret", async () => {
    signIn();
    serve({
      [`GET ${PROVIDERS}`]: json({
        data: [
          provider(),
          { ...GITHUB, enabled: false },
          provider({
            id: "0193a1b2-0000-7000-8000-0000000000c3",
            display_name: "Continue with Corp",
            seeded: true,
            redirect_uri:
              "https://crucible.example.com/api/v1/auth/oauth/0193a1b2-0000-7000-8000-0000000000c3/callback",
          }),
        ],
      }),
    });
    renderPage();
    await screen.findByText(CALLBACK);
    const sso = row("Continue with Acme SSO");
    expect(sso.getByText("On")).toBeTruthy();
    expect(sso.getByText("https://login.acme.example")).toBeTruthy();
    expect(sso.getByText("crucible-web")).toBeTruthy();
    expect(sso.getByText("acme.example")).toBeTruthy();
    expect(sso.getByText("Stored sealed; never shown")).toBeTruthy();
    expect(
      sso.getByRole("button", { name: "Copy callback URL of Continue with Acme SSO" }),
    ).toBeTruthy();
    expect(row("Continue with GitHub").getByText("Turned off")).toBeTruthy();
    expect(row("Continue with GitHub").getByText("Any address the provider verifies")).toBeTruthy();
    expect(row("Continue with GitHub").getByRole("button", { name: /Turn on/ })).toBeTruthy();
    // A provider from the server configuration is read-only here.
    const seeded = row("Continue with Corp");
    expect(
      seeded.getByText("From the server configuration, so it changes only there."),
    ).toBeTruthy();
    expect(seeded.queryByRole("button", { name: /Edit|Turn off|Delete|Replace/ })).toBeNull();
    expect(screen.queryByText(/server.public_url is set/)).toBeNull();
  });

  test("without server.public_url the page says why no provider is offered and what to set", async () => {
    signIn();
    serve({ [`GET ${PROVIDERS}`]: json({ data: [provider({ redirect_uri: "" })] }) });
    renderPage();
    await screen.findByText("The sign-in page offers no provider until server.public_url is set.");
    expect(screen.getByText(/CRUCIBLE_SERVER_PUBLIC_URL/)).toBeTruthy();
    const sso = row("Continue with Acme SSO");
    expect(sso.getByText("Not offered: server.public_url isn't set")).toBeTruthy();
    expect(sso.getByText("Shows here once server.public_url is set.")).toBeTruthy();
    expect(sso.queryByRole("button", { name: /Copy callback URL/ })).toBeNull();
  });

  test("empty: first use invites the first provider", async () => {
    signIn();
    serve({ [`GET ${PROVIDERS}`]: json({ data: [] }) });
    renderPage();
    await screen.findByText("No sign-in providers yet.");
    expect(screen.getByRole("button", { name: "Add provider" })).toBeTruthy();
  });

  test("adding an OpenID Connect provider sends what was entered and shows the callback URL", async () => {
    signIn();
    let stored: AuthProvider[] = [];
    serve({
      [`GET ${PROVIDERS}`]: () => json({ data: stored })(),
      [`POST ${PROVIDERS}`]: () => {
        stored = [provider({ scopes: ["groups"] })];
        return json(stored[0], 201)();
      },
    });
    renderPage();
    await screen.findByText("No sign-in providers yet.");
    await act(() => press("Add provider"));
    await screen.findByRole("dialog", { name: "Add a sign-in provider" });
    // GitHub is chosen first, and the button text follows the kind until it is edited.
    expect(screen.getByLabelText("Button text")).toHaveProperty("value", "Continue with GitHub");
    await act(() => fill("Provider", "OpenID Connect"));
    expect(screen.getByLabelText("Button text")).toHaveProperty("value", "");
    await act(() => fill("Button text", "Continue with Acme SSO"));
    await act(() => fill("Issuer URL", "https://login.acme.example"));
    await act(() => fill("Client ID", " crucible-web "));
    await act(() => fill("Client secret", "s3cret-value"));
    await act(() => fill("Extra scopes", "groups"));
    await act(() => fill("Allowed email domains", "@Acme.example"));

    const refused = await pressIn("Add a sign-in provider", "Add OpenID Connect");
    expect(refused.ok).toBe(false);
    expect(requests(`POST ${PROVIDERS}`)).toHaveLength(0);
    approve = true;
    await act(() => pressIn("Add a sign-in provider", "Add OpenID Connect"));
    await screen.findByRole("dialog", { name: "Continue with Acme SSO is added" });
    const [post] = requests(`POST ${PROVIDERS}`);
    expect(new Headers(post?.headers).get("x-crucible-csrf")).toBe("csrf-4");
    expect(bodyOf(post)).toEqual({
      kind: "oidc",
      display_name: "Continue with Acme SSO",
      issuer: "https://login.acme.example",
      client_id: "crucible-web",
      client_secret: "s3cret-value",
      scopes: ["groups"],
      allowed_domains: ["acme.example"],
    });
    expect(
      screen.getByText("Register this address with the provider as a redirect URI."),
    ).toBeTruthy();
    expect(screen.getAllByText(CALLBACK).length).toBeGreaterThan(0);
    expect(document.body.textContent).not.toContain("s3cret-value");
  });

  test("adding GitHub sends no issuer or scopes, and the server's refusal marks its field", async () => {
    signIn();
    let refuse = true;
    serve({
      [`GET ${PROVIDERS}`]: json({ data: [] }),
      [`POST ${PROVIDERS}`]: () =>
        refuse
          ? problem(422, "validation_failed", [{ pointer: "/allowed_domains", code: "invalid" }])()
          : json(GITHUB, 201)(),
    });
    renderPage();
    await screen.findByText("No sign-in providers yet.");
    await act(() => press("Add provider"));
    await screen.findByRole("dialog", { name: "Add a sign-in provider" });
    expect(screen.queryByLabelText("Issuer URL")).toBeNull();
    expect(screen.queryByLabelText("Extra scopes")).toBeNull();
    await act(() => fill("Client ID", "Iv1.0123456789abcdef"));
    await act(() => fill("Client secret", "gh-secret"));
    await act(() => fill("Allowed email domains", "example.com"));
    approve = true;
    await act(() => pressIn("Add a sign-in provider", "Add GitHub"));
    // The field's error, not the copy the page announces to screen readers.
    await screen.findByText(
      "The server refused these domains. Enter bare domains, like example.com.",
      { selector: "[data-slot=field-error]" },
    );
    expect(document.body.textContent).not.toContain("raw server title");

    refuse = false;
    await act(() => pressIn("Add a sign-in provider", "Add GitHub"));
    await screen.findByRole("dialog", { name: "Continue with GitHub is added" });
    expect(bodyOf(requests(`POST ${PROVIDERS}`).at(-1))).toEqual({
      kind: "github",
      display_name: "Continue with GitHub",
      client_id: "Iv1.0123456789abcdef",
      client_secret: "gh-secret",
      allowed_domains: ["example.com"],
    });
    expect(
      screen.getByText(
        "In the GitHub OAuth app, set the authorization callback URL to this address.",
      ),
    ).toBeTruthy();
  });

  test("a domain list that isn't one says so before anything is sent", async () => {
    signIn();
    serve({ [`GET ${PROVIDERS}`]: json({ data: [] }) });
    renderPage();
    await screen.findByText("No sign-in providers yet.");
    await act(() => press("Add provider"));
    await screen.findByRole("dialog", { name: "Add a sign-in provider" });
    await act(() => fill("Client ID", "Iv1.0123456789abcdef"));
    await act(() => fill("Client secret", "gh-secret"));
    await act(() => fill("Allowed email domains", "ada@example.com"));
    await screen.findByText("Enter domains only, like example.com, separated by commas.", {
      selector: "[data-slot=field-error]",
    });
    approve = true;
    await act(() => pressIn("Add a sign-in provider", "Add GitHub"));
    expect(requests(`POST ${PROVIDERS}`)).toHaveLength(0);
  });

  test("editing sends only what changed and never a secret, and the row shows the change", async () => {
    signIn();
    serve({
      [`GET ${PROVIDERS}`]: json({ data: [provider()] }),
      [`PATCH ${PROVIDERS}/${SSO_ID}`]: json(
        provider({ allowed_domains: ["acme.example", "acme.test"] }),
      ),
    });
    renderPage();
    await screen.findByText(CALLBACK);
    await act(() => press("Edit Continue with Acme SSO"));
    await screen.findByRole("dialog", { name: "Change Continue with Acme SSO" });
    expect(screen.getByLabelText("Allowed email domains")).toHaveProperty("value", "acme.example");
    expect(screen.queryByLabelText("Client secret")).toBeNull();
    await act(() => fill("Allowed email domains", "acme.example, acme.test"));
    approve = true;
    await act(() => pressIn("Change Continue with Acme SSO", "Save changes"));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const [patch] = requests(`PATCH ${PROVIDERS}/${SSO_ID}`);
    expect(new Headers(patch?.headers).get("x-crucible-csrf")).toBe("csrf-4");
    expect(bodyOf(patch)).toEqual({ allowed_domains: ["acme.example", "acme.test"] });
    expect(row("Continue with Acme SSO").getByText("acme.example, acme.test")).toBeTruthy();
  });

  test("replacing the secret needs a value and sends only the secret", async () => {
    signIn();
    serve({
      [`GET ${PROVIDERS}`]: json({ data: [provider()] }),
      [`PATCH ${PROVIDERS}/${SSO_ID}`]: json(provider()),
    });
    renderPage();
    await screen.findByText(CALLBACK);
    await act(() => press("Replace secret of Continue with Acme SSO"));
    const title = "New client secret for Continue with Acme SSO";
    await screen.findByRole("dialog", { name: title });
    approve = true;
    await act(() => pressIn(title, "Save secret"));
    expect(requests(`PATCH ${PROVIDERS}/${SSO_ID}`)).toHaveLength(0);
    await act(() => fill("New client secret", "  rotated-secret  "));
    await act(() => pressIn(title, "Save secret"));
    await screen.findByRole("dialog", { name: "Client secret replaced" });
    expect(bodyOf(requests(`PATCH ${PROVIDERS}/${SSO_ID}`)[0])).toEqual({
      client_secret: "rotated-secret",
    });
    expect(document.body.textContent).not.toContain("rotated-secret");
  });

  test("turning a provider off needs a person, then the row says so", async () => {
    signIn();
    serve({
      [`GET ${PROVIDERS}`]: json({ data: [provider()] }),
      [`PATCH ${PROVIDERS}/${SSO_ID}`]: json(provider({ enabled: false })),
    });
    renderPage();
    await screen.findByText(CALLBACK);
    const turnOff = { steps: [{ ref: "Turn off Continue with Acme SSO", action: "press" }] };
    expect((await agent.act(turnOff)).ok).toBe(false);
    expect(requests(`PATCH ${PROVIDERS}/${SSO_ID}`)).toHaveLength(0);
    approve = true;
    await act(() => agent.act(turnOff));
    await waitFor(() => expect(row("Continue with Acme SSO").getByText("Turned off")).toBeTruthy());
    expect(bodyOf(requests(`PATCH ${PROVIDERS}/${SSO_ID}`)[0])).toEqual({ enabled: false });
  });

  test("a provider someone else deleted says so and leaves the list", async () => {
    signIn();
    let stored = [provider()];
    serve({
      [`GET ${PROVIDERS}`]: () => json({ data: stored })(),
      [`PATCH ${PROVIDERS}/${SSO_ID}`]: () => {
        stored = [];
        return problem(404, "not_found")();
      },
    });
    renderPage();
    await screen.findByText(CALLBACK);
    approve = true;
    await act(() => press("Turn off Continue with Acme SSO"));
    await screen.findByText("No sign-in providers yet.");
  });

  test("deleting asks first, then the provider leaves the list", async () => {
    signIn();
    let stored = [provider(), GITHUB];
    serve({
      [`GET ${PROVIDERS}`]: () => json({ data: stored })(),
      [`DELETE ${PROVIDERS}/${SSO_ID}`]: () => {
        stored = [GITHUB];
        return empty();
      },
    });
    renderPage();
    await screen.findByText(CALLBACK);
    // One confirmation serves every row; each row's trigger opens it for that row.
    const dialog = dialogRef("Delete this provider?");
    await act(() =>
      agent.act({
        steps: [{ ref: dialog, action: "press", args: ["Delete Continue with Acme SSO"] }],
      }),
    );
    await screen.findByRole("alertdialog", { name: "Delete Continue with Acme SSO?" });
    expect(requests(`DELETE ${PROVIDERS}/${SSO_ID}`)).toHaveLength(0);
    const confirm = { steps: [{ ref: dialog, action: "press", args: ["Delete provider"] }] };
    expect((await agent.act(confirm)).ok).toBe(false);
    expect(requests(`DELETE ${PROVIDERS}/${SSO_ID}`)).toHaveLength(0);
    approve = true;
    await act(() => agent.act(confirm));
    await waitFor(() => expect(screen.queryByText(CALLBACK)).toBeNull());
    expect(requests(`DELETE ${PROVIDERS}/${SSO_ID}`)).toHaveLength(1);
    expect(document.querySelector('[data-provider="Continue with GitHub"]')).not.toBeNull();
  });

  test("error: the section says so with a retry that reloads it", async () => {
    signIn();
    let fail = true;
    serve({
      [`GET ${PROVIDERS}`]: () =>
        fail ? problem(503, "control_plane_unavailable")() : json({ data: [provider()] })(),
    });
    renderPage();
    const retry = await screen.findByRole("button", { name: "Retry sign-in providers" });
    expect(document.body.textContent).not.toContain("raw server title");
    fail = false;
    await act(async () => retry.click());
    await screen.findByText(CALLBACK);
  });

  test("a member is told who manages providers, and nothing is requested", () => {
    signIn("member");
    renderPage();
    expect(
      screen.getByText("Only organization owners and admins manage sign-in providers."),
    ).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("on a narrow screen the list reads with its callback URLs, and editing waits for a wider one", async () => {
    wide = false;
    signIn();
    serve({ [`GET ${PROVIDERS}`]: json({ data: [provider()] }) });
    renderPage();
    await screen.findByText(CALLBACK);
    expect(screen.getByRole("button", { name: /Copy callback URL/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Add provider" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Edit|Turn off|Delete|Replace/ })).toBeNull();
    expect(screen.getByText("Editing needs a wider screen.")).toBeTruthy();
  });

  test("without a control plane the page says which setting turns it on and requests nothing", () => {
    setAuth({ mode: "keys" });
    renderPage();
    expect(screen.getByText("Sign-in providers need a control plane.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("an agent finds every control by name", async () => {
    signIn();
    serve({ [`GET ${PROVIDERS}`]: json({ data: [provider(), GITHUB] }) });
    renderPage();
    await screen.findByText(CALLBACK);
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    for (const label of [
      "Add provider",
      "Edit Continue with Acme SSO",
      "Replace secret of Continue with Acme SSO",
      "Turn off Continue with Acme SSO",
      "Delete this provider?",
      "Copy callback URL of Continue with Acme SSO",
      "Edit Continue with GitHub",
    ]) {
      expect(labels, `${label} is missing from the snapshot`).toContain(label);
    }
    // The shared confirmation's triggers carry their rows in their names.
    expect(screen.getByRole("button", { name: "Delete Continue with Acme SSO" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Delete Continue with GitHub" })).toBeTruthy();
  });
});
