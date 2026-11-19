import { act, screen } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { LAGGING, json, page, problem, renderWithClient, requests, session } from "@/test/render";
import { SessionsSection } from "./sessions-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const NO_ANSWER = () => {};
const agent = createAgent();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/sessions/?project=p-1");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

describe("sessions", () => {
  test("loading: the section is busy until the list arrives", async () => {
    let answer: (response: Response) => void = NO_ANSWER;
    fetchMock.mockReturnValue(new Promise((resolve) => (answer = resolve)));
    renderWithClient(<SessionsSection />);
    const region = screen.getByRole("region", { name: "Sessions" });
    expect(region.getAttribute("aria-busy")).toBe("true");
    await act(async () => answer(json(page([session()]))));
    expect(await screen.findByRole("link", { name: "chat-42" })).toBeTruthy();
    expect(region.getAttribute("aria-busy")).toBeNull();
  });

  test("success: one row per session linking to it, and the request carries the window", async () => {
    fetchMock.mockResolvedValue(json(page([session({ error_count: 2 })])));
    renderWithClient(<SessionsSection />);
    const link = await screen.findByRole("link", { name: "chat-42" });
    const href = new URL(link.getAttribute("href") ?? "", "http://localhost");
    expect(href.pathname).toMatch(/^\/session\/?$/);
    expect(Object.fromEntries(href.searchParams)).toEqual({ project: "p-1", session: "chat-42" });
    const headers = screen.getAllByRole("columnheader").map((cell) => cell.textContent);
    expect(headers).toEqual([
      "Last seen",
      "Session",
      "Traces",
      "Spans",
      "Cost",
      "Tokens",
      "Status",
    ]);
    expect(screen.getByText("2 errors").getAttribute("data-status")).toBe("error");
    expect(screen.getByText("1,200")).toBeTruthy();
    const [url] = requests(fetchMock.mock.calls);
    expect(url?.pathname).toBe("/api/v1/projects/p-1/sessions");
    expect([...(url?.searchParams.keys() ?? [])]).toEqual(["from", "to"]);
  });

  test("paging: Load more reads the next page with the first page's window and cursor", async () => {
    fetchMock.mockResolvedValueOnce(json(page([session()], { next_cursor: "c-2" })));
    fetchMock.mockResolvedValueOnce(json(page([session({ session_id: "chat-7" })])));
    renderWithClient(<SessionsSection />);
    const more = await screen.findByRole("button", { name: "Load more sessions" });
    await act(async () => more.click());
    expect(await screen.findByRole("link", { name: "chat-7" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Load more sessions" })).toBeNull();
    const [first, second] = requests(fetchMock.mock.calls);
    expect(second?.searchParams.get("cursor")).toBe("c-2");
    expect(second?.searchParams.get("from")).toBe(first?.searchParams.get("from"));
    expect(second?.searchParams.get("to")).toBe(first?.searchParams.get("to"));
  });

  test("paging: a cursor the server no longer accepts offers to start over", async () => {
    fetchMock.mockResolvedValueOnce(json(page([session()], { next_cursor: "c-2" })));
    fetchMock.mockResolvedValueOnce(problem(400, "cursor_invalid"));
    fetchMock.mockResolvedValue(json(page([session({ session_id: "chat-9" })])));
    renderWithClient(<SessionsSection />);
    await act(async () =>
      (await screen.findByRole("button", { name: "Load more sessions" })).click(),
    );
    const restart = await screen.findByRole("button", { name: "Show the list from the top" });
    await act(async () => restart.click());
    expect(await screen.findByRole("link", { name: "chat-9" })).toBeTruthy();
  });

  test("error: a failed list is an in-section error with a retry, without server text", async () => {
    fetchMock.mockResolvedValueOnce(problem(503, "telemetry_store_unavailable"));
    fetchMock.mockResolvedValue(json(page([session()])));
    renderWithClient(<SessionsSection />);
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.queryByText(/raw server title/)).toBeNull();
    await act(async () => screen.getByRole("button", { name: "Retry sessions" }).click());
    expect(await screen.findByRole("link", { name: "chat-42" })).toBeTruthy();
  });

  test("empty: an empty window offers a wider range, and the widest invites the first session", async () => {
    fetchMock.mockImplementation(async () => json(page([])));
    renderWithClient(<SessionsSection />);
    const widen = await screen.findByRole("button", { name: "Show last 30 days" });
    expect(
      screen
        .getByText("No sessions in the last 24 hours.")
        .closest("[data-empty]")
        ?.getAttribute("data-empty"),
    ).toBe("filtered");
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components)
      expect(entry.label, `${entry.component} has no name`).toBeTruthy();
    await act(async () => widen.click());
    const firstUse = await screen.findByText("No sessions in the last 30 days.");
    expect(firstUse.closest("[data-empty]")?.getAttribute("data-empty")).toBe("first-use");
  });

  test("empty: an empty page that may be incomplete offers to check again", async () => {
    fetchMock.mockResolvedValueOnce(json(page([], { meta: LAGGING })));
    fetchMock.mockResolvedValueOnce(json(page([session()])));
    renderWithClient(<SessionsSection />);
    const empty = await screen.findByText("No sessions to show yet.");
    expect(empty.closest("[data-empty]")?.getAttribute("data-empty")).toBe("incomplete");
    await act(async () => screen.getByRole("button", { name: "Check again" }).click());
    expect(await screen.findByRole("link", { name: "chat-42" })).toBeTruthy();
  });
});
