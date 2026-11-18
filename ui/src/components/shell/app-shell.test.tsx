import { act, fireEvent, render, screen } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { beforeEach, expect, test, vi } from "vitest";
import { rangeStorageKey } from "@/lib/time-range";
import { AppShell } from "./app-shell";

vi.mock("next/navigation", () => import("@/test/navigation"));

const agent = createAgent();

beforeEach(() => {
  window.history.replaceState(null, "", "/traces/?project=p1");
});

function renderShell() {
  return render(
    <AppShell>
      <p>Page body</p>
    </AppShell>,
  );
}

test("an agent finds every shell control by name and can change the time range", async () => {
  renderShell();
  const components = agent
    .snapshot()
    .filter((entry) => !entry.offPage && entry.component !== "more");
  for (const entry of components) {
    expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
  }
  const labels = components.map((entry) => entry.label);
  for (const label of ["Pages", "Project", "Time range"]) {
    expect(labels, `${label} is missing from the snapshot`).toContain(label);
  }

  const result = await agent.act({
    steps: [{ ref: "Time range", action: "setValue", args: ["7d"] }],
  });
  expect(result.ok, result.error?.message).toBe(true);
  expect(new URLSearchParams(window.location.search).get("range")).toBe("7d");
  expect(localStorage.getItem(rangeStorageKey("p1"))).toBe("7d");
});

test("arrow keys move the range, and nav links keep the project and range", async () => {
  renderShell();
  const control = screen.getByRole("radiogroup", { name: "Time range" });
  const current = screen.getByRole("radio", { name: "24h" });
  expect(current.getAttribute("aria-checked")).toBe("true");
  expect(control.contains(current)).toBe(true);

  await act(async () => {
    current.focus();
    fireEvent.keyDown(current, { key: "ArrowRight" });
  });

  expect(screen.getByRole("radio", { name: "7d" }).getAttribute("aria-checked")).toBe("true");
  expect(new URLSearchParams(window.location.search).get("range")).toBe("7d");
  const sessions = screen.getByRole("link", { name: "Sessions" });
  expect(new URL(sessions.getAttribute("href") ?? "", "http://localhost").search).toBe(
    "?project=p1&range=7d",
  );
});

test("the nav marks the current area, including detail pages", () => {
  window.history.replaceState(null, "", "/trace/?project=p1&trace=abc");
  renderShell();
  const traces = screen.getByRole("treeitem", { name: "Traces" });
  expect(traces.getAttribute("aria-current")).toBe("page");
  expect(screen.getByRole("treeitem", { name: "Home" }).getAttribute("aria-current")).toBeNull();
});
