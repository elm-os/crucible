import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { createQueryClient } from "@/components/providers";
import { loadPage } from "@/lib/navigate";
import { sessionInfo } from "@/test/session";
import { InviteForm } from "./invite-form";
import { ResetForm } from "./reset-form";
import { SetupForm } from "./setup-form";

vi.mock("next/navigation", () => import("@/test/navigation"));
vi.mock("@/lib/navigate", () => ({ loadPage: vi.fn() }));

const agent = createAgent();
const fetchMock = vi.fn<typeof fetch>();
const PASSWORD = "correct horse battery staple";

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.mocked(loadPage).mockReset();
  vi.unstubAllGlobals();
});

function renderForm(form: ReactNode, path: string) {
  window.history.replaceState(null, "", path);
  return render(<QueryClientProvider client={createQueryClient()}>{form}</QueryClientProvider>);
}

function expectNamed(labels: string[]) {
  const components = agent
    .snapshot()
    .filter((entry) => !entry.offPage && entry.component !== "more");
  for (const entry of components) {
    expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
  }
  const found = components.map((entry) => entry.label);
  for (const label of labels)
    expect(found, `${label} is missing from the snapshot`).toContain(label);
}

function sentBody(): unknown {
  return JSON.parse(String(fetchMock.mock.calls[0]?.[1]?.body));
}

async function act(steps: { ref: string; action: string; args?: unknown[] }[]) {
  const result = await agent.act({ steps });
  expect(result.ok, result.error?.message).toBe(true);
}

const json = (body: unknown) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

describe("first-run setup", () => {
  test("an agent fills every field; a short password is refused before anything is sent", async () => {
    renderForm(<SetupForm />, "/setup/");
    expectNamed([
      "Setup token",
      "Organization name",
      "Your name",
      "Email",
      "Password",
      "Create owner account",
    ]);
    const fill = [
      { ref: "Setup token", action: "setValue", args: ["setup-token-1"] },
      { ref: "Organization name", action: "setValue", args: ["Analytical Engines"] },
      { ref: "Your name", action: "setValue", args: ["Ada Lovelace"] },
      { ref: "Email", action: "setValue", args: ["ada@example.com"] },
    ];
    await act([
      ...fill,
      { ref: "Password", action: "setValue", args: ["too short"] },
      { ref: "Create owner account", action: "press" },
    ]);
    expect(await screen.findByText("Use 12 to 128 characters.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();

    fetchMock.mockResolvedValueOnce(json(sessionInfo()));
    await act([
      { ref: "Password", action: "setValue", args: [PASSWORD] },
      { ref: "Create owner account", action: "press" },
    ]);
    await waitFor(() => expect(loadPage).toHaveBeenCalledWith("/"));
    expect(sentBody()).toEqual({
      token: "setup-token-1",
      org_name: "Analytical Engines",
      name: "Ada Lovelace",
      email: "ada@example.com",
      password: PASSWORD,
    });
  });
});

describe("first-run setup from the server log's link", () => {
  test("the link's token is filled in and sent", async () => {
    renderForm(<SetupForm />, "/setup/?token=setup-token-2");
    fetchMock.mockResolvedValueOnce(json(sessionInfo()));
    await act([
      { ref: "Organization name", action: "setValue", args: ["Analytical Engines"] },
      { ref: "Your name", action: "setValue", args: ["Ada Lovelace"] },
      { ref: "Email", action: "setValue", args: ["ada@example.com"] },
      { ref: "Password", action: "setValue", args: [PASSWORD] },
      { ref: "Create owner account", action: "press" },
    ]);
    await waitFor(() => expect(loadPage).toHaveBeenCalledWith("/"));
    expect(sentBody()).toMatchObject({ token: "setup-token-2", org_name: "Analytical Engines" });
  });
});

describe("invitations", () => {
  test("an agent joins with the link's token", async () => {
    renderForm(<InviteForm />, "/invite/?token=invite-token-1");
    expectNamed(["Your name", "Password", "Accept invitation"]);
    fetchMock.mockResolvedValueOnce(json(sessionInfo()));
    await act([
      { ref: "Your name", action: "setValue", args: ["Grace Hopper"] },
      { ref: "Password", action: "setValue", args: [PASSWORD] },
      { ref: "Accept invitation", action: "press" },
    ]);
    await waitFor(() => expect(loadPage).toHaveBeenCalledWith("/"));
    expect(sentBody()).toEqual({
      token: "invite-token-1",
      name: "Grace Hopper",
      password: PASSWORD,
    });
  });
});

describe("password reset", () => {
  test("an agent asks for a link, and the page names the administrator's command", async () => {
    renderForm(<ResetForm />, "/reset/");
    expectNamed(["Email", "Request a reset link"]);
    fetchMock.mockResolvedValueOnce(new Response(null, { status: 202 }));
    await act([
      { ref: "Email", action: "setValue", args: ["ada@example.com"] },
      { ref: "Request a reset link", action: "press" },
    ]);
    expect(await screen.findByRole("heading", { name: "Request received" })).toBeTruthy();
    expect(screen.getByText("crucible admin reset-password ada@example.com")).toBeTruthy();
    expect(sentBody()).toEqual({ email: "ada@example.com" });
  });

  test("an agent sets a new password with the link's token; a short one is refused first", async () => {
    renderForm(<ResetForm />, "/reset/?token=reset-token-1");
    expectNamed(["New password", "Change password"]);
    await act([
      { ref: "New password", action: "setValue", args: ["too short"] },
      { ref: "Change password", action: "press" },
    ]);
    expect(await screen.findByText("Use 12 to 128 characters.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();

    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await act([
      { ref: "New password", action: "setValue", args: [PASSWORD] },
      { ref: "Change password", action: "press" },
    ]);
    expect(await screen.findByRole("heading", { name: "Password changed" })).toBeTruthy();
    expect(sentBody()).toEqual({ token: "reset-token-1", new_password: PASSWORD });
  });
});
