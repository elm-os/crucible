import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { ApiKey } from "@/api/generated/ApiKey";
import type { Invitation } from "@/api/generated/Invitation";
import type { OrgMember } from "@/api/generated/OrgMember";
import type { OrgRole } from "@/api/generated/OrgRole";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { PROJECT_A, PROJECT_B, sessionInfo } from "@/test/session";
import { ApiKeysSettings } from "./api-keys";
import { MembersSettings } from "./members";

vi.mock("next/navigation", () => import("@/test/navigation"));

// Steps on controls marked `data-agent-confirm` run only once a person approves.
let approve = false;
const agent = createAgent({ allow: (step) => !step.confirm || approve });
const fetchMock = vi.fn<typeof fetch>();
const SELF = sessionInfo().user.id;

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
const problem = (status: number, code: string) => () =>
  new Response(JSON.stringify({ code, status, request_id: "req-9" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  approve = false;
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function renderPage(page: ReactNode) {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(<QueryClientProvider client={client}>{page}</QueryClientProvider>);
}

function signIn(role: OrgRole) {
  const session = sessionInfo({ csrf_token: "csrf-5" });
  setAuth({ mode: "session", session: { ...session, org: { ...session.org, role } } });
}

function labels(): string[] {
  const components = agent
    .snapshot()
    .filter((entry) => !entry.offPage && entry.component !== "more");
  for (const entry of components) {
    expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
  }
  return components.map((entry) => entry.label ?? "");
}

function dialogRef(title: string): string {
  const ref = agent.snapshot().find((entry) => entry.label === title)?.ref;
  if (ref === undefined) throw new Error(`no dialog named ${title}`);
  return ref;
}

const member = (fields: Partial<OrgMember>): OrgMember => ({
  user_id: "u-2",
  email: "grace@example.com",
  name: "Grace Hopper",
  role: "member",
  status: "active",
  ...fields,
});

const MEMBERS = {
  data: [
    member({ user_id: SELF, email: "ada@example.com", name: "Ada Lovelace", role: "owner" }),
    member({}),
    member({ user_id: "u-3", email: "edsger@example.com", name: "Edsger Dijkstra", role: "admin" }),
  ],
};

const invitation = (fields: Partial<Invitation>): Invitation => ({
  id: "i-1",
  email: "barbara@example.com",
  org_role: "viewer",
  project_roles: [],
  expires_at: "2099-01-01T00:00:00.000000Z",
  accepted_at: null,
  revoked_at: null,
  ...fields,
});

const INVITATIONS = {
  data: [
    invitation({}),
    invitation({
      id: "i-2",
      email: "alan@example.com",
      accepted_at: "2026-11-01T00:00:00.000000Z",
    }),
  ],
};

describe("members", () => {
  test("an owner's agent changes a role, and removing someone needs a person", async () => {
    signIn("owner");
    serve({
      "GET /api/v1/org/members": json(MEMBERS),
      "GET /api/v1/org/invitations": json(INVITATIONS),
      "PATCH /api/v1/org/members/u-2": json(member({ role: "admin" })),
      "DELETE /api/v1/org/members/u-2": empty,
    });
    renderPage(<MembersSettings />);
    await screen.findByRole("cell", { name: "Grace Hopper" });
    await screen.findByRole("cell", { name: "barbara@example.com" });

    // Row triggers open one shared dialog each, which the snapshot names by its title.
    const found = labels();
    for (const label of [
      "Role for Grace Hopper",
      "Role for Edsger Dijkstra",
      "Invite member",
      "Remove this member?",
      "Revoke this invitation?",
    ]) {
      expect(found, `${label} is missing from the snapshot`).toContain(label);
    }
    expect(screen.getByRole("button", { name: "Remove Grace Hopper" })).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "Revoke the invitation for barbara@example.com" }),
    ).toBeTruthy();
    // The owner's own row, and an accepted invitation, offer nothing to change.
    expect(found).not.toContain("Role for Ada Lovelace");
    expect(screen.queryByRole("button", { name: "Remove Ada Lovelace" })).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Revoke the invitation for alan@example.com" }),
    ).toBeNull();

    const role = await agent.act({
      steps: [{ ref: "Role for Grace Hopper", action: "setValue", args: ["Admin"] }],
    });
    expect(role.ok, role.error?.message).toBe(true);
    await waitFor(() => expect(requests("PATCH /api/v1/org/members/u-2")).toHaveLength(1));
    const [patch] = requests("PATCH /api/v1/org/members/u-2");
    expect(JSON.parse(String(patch?.body))).toEqual({ role: "admin" });
    expect(new Headers(patch?.headers).get("x-crucible-csrf")).toBe("csrf-5");

    // The rows share one dialog: pressing a row's trigger text opens it for that row.
    const dialog = dialogRef("Remove this member?");
    const remove = [
      { ref: dialog, action: "press", args: ["Remove Grace Hopper"] },
      { ref: dialog, action: "press", args: ["Remove member"] },
    ];
    const refused = await agent.act({ steps: remove });
    expect(refused.error?.message).toContain("said no");
    expect(requests("DELETE /api/v1/org/members/u-2")).toHaveLength(0);

    approve = true;
    const removed = await agent.act({ steps: remove.slice(1) });
    expect(removed.ok, removed.error?.message).toBe(true);
    await waitFor(() => expect(requests("DELETE /api/v1/org/members/u-2")).toHaveLength(1));
  });

  test("an invitation's link shows once, until the dialog closes", async () => {
    signIn("admin");
    serve({
      "GET /api/v1/org/members": json(MEMBERS),
      "GET /api/v1/org/invitations": json({ data: [] }),
      "POST /api/v1/org/invitations": json(
        {
          invitation: invitation({ email: "barbara@example.com" }),
          accept_path: "/invite/?token=tok-1",
        },
        201,
      ),
    });
    renderPage(<MembersSettings />);
    expect(await screen.findByText("No invitations yet.")).toBeTruthy();
    const invited = await agent.act({
      steps: [
        { ref: "Invite member", action: "press" },
        { ref: "Email", action: "setValue", args: ["barbara@example.com"] },
        { ref: "Create invitation", action: "press" },
      ],
    });
    expect(invited.ok, invited.error?.message).toBe(true);
    const link = `${window.location.origin}/invite/?token=tok-1`;
    expect(await screen.findByText(link)).toBeTruthy();
    const [post] = requests("POST /api/v1/org/invitations");
    expect(JSON.parse(String(post?.body))).toEqual({
      email: "barbara@example.com",
      org_role: "member",
    });

    await act(async () => screen.getByRole("button", { name: "Done" }).click());
    await waitFor(() => expect(screen.queryByText(link)).toBeNull());
    await act(async () => screen.getByRole("button", { name: "Invite member" }).click());
    expect(await screen.findByRole("dialog", { name: "Invite a member" })).toBeTruthy();
    expect(screen.queryByText(link)).toBeNull();
  });

  test("a role change the server refuses says why, and the menu keeps the current role", async () => {
    signIn("owner");
    serve({
      "GET /api/v1/org/members": json(MEMBERS),
      "GET /api/v1/org/invitations": json({ data: [] }),
      "PATCH /api/v1/org/members/u-2": problem(403, "forbidden"),
    });
    renderPage(<MembersSettings />);
    await screen.findByRole("cell", { name: "Grace Hopper" });
    const result = await agent.act({
      steps: [{ ref: "Role for Grace Hopper", action: "setValue", args: ["Admin"] }],
    });
    expect(result.ok, result.error?.message).toBe(true);
    expect((await screen.findByRole("alert")).textContent).toContain("Your role can't do this.");
    expect(screen.getByRole("alert").textContent).not.toContain("req-9");
    expect(agent.snapshot().find((entry) => entry.label === "Role for Grace Hopper")?.value).toBe(
      "member",
    );
  });

  test("members and viewers see both lists without controls", async () => {
    signIn("viewer");
    serve({
      "GET /api/v1/org/members": json(MEMBERS),
      "GET /api/v1/org/invitations": json(INVITATIONS),
    });
    renderPage(<MembersSettings />);
    await screen.findByRole("cell", { name: "Grace Hopper" });
    await screen.findByRole("cell", { name: "barbara@example.com" });
    const found = labels();
    expect(found.filter((label) => /^(Role for|Remove|Revoke|Invite)/.test(label))).toEqual([]);
    expect(screen.queryAllByRole("button", { name: /^(Remove|Revoke|Invite)/ })).toEqual([]);
  });

  test("without a control plane nothing is fetched, and the page says which setting adds members", () => {
    renderPage(<MembersSettings />);
    expect(screen.getByText("Members need a control plane.")).toBeTruthy();
    expect(screen.getByText(/postgres\.url/)).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

const KEY: ApiKey = {
  id: "k-1",
  public_key: `cpk_${"a".repeat(43)}`,
  scopes: ["ingest", "read", "write"],
  note: "checkout-service",
  expires_at: null,
  last_used_at: null,
  revoked_at: null,
  version: 3,
};

const KEYS_PATH = `/api/v1/projects/${PROJECT_A}/api-keys`;

function renderKeys(routes: Record<string, Handler>) {
  window.history.replaceState(null, "", `/settings/api-keys/?project=${PROJECT_A}`);
  signIn("owner");
  serve({ [`GET ${KEYS_PATH}`]: json({ data: [KEY] }), ...routes });
  renderPage(<ApiKeysSettings />);
}

describe("API keys", () => {
  test("a new key's secret shows once, until the dialog closes", async () => {
    const secret = `csk_${"s".repeat(43)}`;
    renderKeys({
      [`POST ${KEYS_PATH}`]: json(
        { key: { ...KEY, id: "k-2", note: "batch" }, secret_key: secret },
        201,
      ),
    });
    expect(await screen.findByText("checkout-service")).toBeTruthy();
    const found = labels();
    for (const label of ["Create key", "Revoke this key?"]) {
      expect(found, `${label} is missing from the snapshot`).toContain(label);
    }

    const created = await agent.act({
      steps: [
        { ref: "Create key", action: "press" },
        { ref: "Note", action: "setValue", args: ["batch"] },
        { ref: "Create key", action: "press" },
      ],
    });
    expect(created.ok, created.error?.message).toBe(true);
    expect(await screen.findByText(secret)).toBeTruthy();
    expect(screen.getByText(`${KEY.public_key}`, { selector: "code" })).toBeTruthy();
    expect(screen.getByText("Copy your secret key")).toBeTruthy();
    expect(JSON.parse(String(requests(`POST ${KEYS_PATH}`)[0]?.body))).toEqual({ note: "batch" });
    await act(async () => screen.getByRole("button", { name: "Done" }).click());
    await waitFor(() => expect(screen.queryByText(secret)).toBeNull());
    await act(async () => screen.getByRole("button", { name: "Create key" }).click());
    expect(
      await screen.findByRole("dialog", { name: "Create an API key for Checkout" }),
    ).toBeTruthy();
    expect(screen.queryByText(secret)).toBeNull();
  });

  test("revoking sends the key's version and needs a person", async () => {
    const other = { ...KEY, id: "k-9", public_key: `cpk_${"b".repeat(43)}`, note: "search" };
    renderKeys({
      [`GET ${KEYS_PATH}`]: json({ data: [KEY, other] }),
      [`DELETE ${KEYS_PATH}/k-1`]: empty,
    });
    expect(await screen.findByText("checkout-service")).toBeTruthy();
    expect(screen.getByRole("button", { name: `Revoke ${KEY.public_key}` })).toBeTruthy();
    const dialog = dialogRef("Revoke this key?");
    const revoke = [
      { ref: dialog, action: "press", args: [`Revoke ${KEY.public_key}`] },
      { ref: dialog, action: "press", args: ["Revoke key"] },
    ];
    const refused = await agent.act({ steps: revoke });
    expect(refused.error?.message).toContain("said no");
    expect(requests(`DELETE ${KEYS_PATH}/k-1`)).toHaveLength(0);
    approve = true;
    const revoked = await agent.act({ steps: revoke.slice(1) });
    expect(revoked.ok, revoked.error?.message).toBe(true);
    await waitFor(() => expect(requests(`DELETE ${KEYS_PATH}/k-1`)).toHaveLength(1));
    const [sent] = requests(`DELETE ${KEYS_PATH}/k-1`);
    expect(new Headers(sent?.headers).get("if-match")).toBe('"3"');
  });

  test("a key that changed since the list loaded refreshes the list, and the dialog says so", async () => {
    approve = true;
    const other = { ...KEY, id: "k-9", public_key: `cpk_${"b".repeat(43)}`, note: "search" };
    renderKeys({
      [`GET ${KEYS_PATH}`]: json({ data: [KEY, other] }),
      [`DELETE ${KEYS_PATH}/k-1`]: problem(412, "precondition_failed"),
    });
    expect(await screen.findByText("checkout-service")).toBeTruthy();
    const reads = requests(`GET ${KEYS_PATH}`).length;
    const dialog = dialogRef("Revoke this key?");
    // The bridge reports the press as done; the rejection lands in the confirm button's state.
    await agent.act({
      steps: [
        { ref: dialog, action: "press", args: [`Revoke ${KEY.public_key}`] },
        { ref: dialog, action: "press", args: ["Revoke key"] },
      ],
    });
    expect((await screen.findByRole("alert")).textContent).toContain(
      "This key changed since the list loaded.",
    );
    expect(requests(`GET ${KEYS_PATH}`).length).toBe(reads + 1);
  });

  test("without the manage permission on the project nothing is fetched", () => {
    window.history.replaceState(null, "", `/settings/api-keys/?project=${PROJECT_B}`);
    signIn("member");
    renderPage(<ApiKeysSettings />);
    expect(screen.getByText("Only project admins manage API keys.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("a project outside the session, and no projects at all, each say so", () => {
    window.history.replaceState(null, "", "/settings/api-keys/?project=elsewhere");
    signIn("owner");
    const { unmount } = renderPage(<ApiKeysSettings />);
    expect(screen.getByText("This project isn't available to you.")).toBeTruthy();
    unmount();

    setAuth({ mode: "session", session: sessionInfo({ projects: [] }) });
    window.history.replaceState(null, "", "/settings/api-keys/");
    renderPage(<ApiKeysSettings />);
    expect(screen.getByText("This organization has no projects yet.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("without a control plane the page says keys come from the keys file", () => {
    renderPage(<ApiKeysSettings />);
    expect(screen.getByText("API keys need a control plane.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
