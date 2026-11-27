import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, expect, test, vi } from "vitest";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { sessionInfo } from "@/test/session";
import { AppShell } from "./app-shell";

vi.mock("next/navigation", () => import("@/test/navigation"));

const agent = createAgent();

afterEach(() => {
  vi.unstubAllGlobals();
});

function renderShell() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <AppShell>
        <p>Page body</p>
      </AppShell>
    </QueryClientProvider>,
  );
}

const header = () => within(screen.getByRole("banner"));

async function openFromKeyboard() {
  const trigger = header().getByRole("button", { name: "Feedback" });
  await act(async () => {
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
  });
  return { trigger, menu: screen.getByRole("menu", { name: "Feedback" }) };
}

test("Feedback opens two GitHub links, each in a new tab with no referrer and no query", async () => {
  renderShell();
  const { trigger, menu } = await openFromKeyboard();
  expect(trigger.getAttribute("aria-expanded")).toBe("true");
  const group = within(menu).getByRole("group", { name: "Opens GitHub in a new tab" });
  const links = within(group).getAllByRole("menuitem");
  expect(links.map((link) => [link.textContent, link.getAttribute("href")])).toEqual([
    ["Ask a question or suggest an idea", "https://github.com/elm-os/crucible/discussions"],
    ["Report a bug", "https://github.com/elm-os/crucible/issues/new/choose"],
  ]);
  for (const link of links) {
    expect(link.tagName).toBe("A");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noreferrer");
  }
  expect(document.activeElement).toBe(links[0]);
});

test("Escape closes the menu and focus returns to Feedback", async () => {
  renderShell();
  const { trigger } = await openFromKeyboard();
  await act(async () => {
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Escape" });
  });
  expect(screen.queryByRole("menu")).toBeNull();
  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(trigger);
});

test("an agent finds Feedback by name and reads both links once it opens", async () => {
  renderShell();
  const result = await agent.act({ steps: [{ ref: "Feedback", action: "setOpen", args: [true] }] });
  expect(result.ok, result.error?.message).toBe(true);
  const menu = agent.snapshot().find((entry) => !entry.offPage && entry.label === "Feedback");
  expect(menu?.component).toBe("menu");
  expect(menu?.buttons).toEqual(["Ask a question or suggest an idea", "Report a bug"]);
});

test("Feedback stays in the header signed in, without a control plane, and when the server can't be reached", async () => {
  const keys = renderShell();
  expect(header().getByRole("button", { name: "Feedback" })).toBeTruthy();
  expect(header().getByRole("button", { name: "Connect" })).toBeTruthy();
  keys.unmount();

  setAuth({ mode: "session", session: sessionInfo() });
  const signedIn = renderShell();
  expect(header().getByRole("button", { name: "Feedback" })).toBeTruthy();
  expect(header().getByRole("button", { name: "Ada Lovelace" })).toBeTruthy();
  signedIn.unmount();

  setAuth({ mode: "unknown" });
  vi.stubGlobal("fetch", vi.fn<typeof fetch>().mockRejectedValue(new TypeError("Failed to fetch")));
  renderShell();
  expect(await screen.findByRole("button", { name: "Retry connection" })).toBeTruthy();
  expect(header().getByRole("button", { name: "Feedback" })).toBeTruthy();
});
