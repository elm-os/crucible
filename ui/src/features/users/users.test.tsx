import { act, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { WHOLE, json, page, problem, renderWithClient, requests, user } from "@/test/render";
import { UserDetail } from "./user-detail";
import { UsersSection } from "./users-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const NO_ANSWER = () => {};

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

describe("users", () => {
  beforeEach(() => window.history.replaceState(null, "", "/users/?project=p-1"));

  test("loading, then one row per user with its session count, linking to the user", async () => {
    let answer: (response: Response) => void = NO_ANSWER;
    fetchMock.mockReturnValue(new Promise((resolve) => (answer = resolve)));
    renderWithClient(<UsersSection />);
    expect(screen.getByRole("region", { name: "Users" }).getAttribute("aria-busy")).toBe("true");
    await act(async () => answer(json(page([user()]))));
    const link = await screen.findByRole("link", { name: "ada" });
    expect(link.getAttribute("href")).toMatch(/^\/user\/?\?project=p-1&user=ada$/);
    const headers = screen.getAllByRole("columnheader").map((cell) => cell.textContent);
    expect(headers).toEqual([
      "Last seen",
      "User",
      "Traces",
      "Sessions",
      "Spans",
      "Cost",
      "Tokens",
      "Status",
    ]);
    expect(screen.getByText("1 error")).toBeTruthy();
  });

  test("error with retry, and the empty window's wider range", async () => {
    fetchMock.mockResolvedValueOnce(problem(500, "internal_error"));
    fetchMock.mockImplementation(async () => json(page([])));
    renderWithClient(<UsersSection />);
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    await act(async () => screen.getByRole("button", { name: "Retry users" }).click());
    expect(await screen.findByText("No users in the last 24 hours.")).toBeTruthy();
    await act(async () => screen.getByRole("button", { name: "Show last 30 days" }).click());
    expect(await screen.findByText("No users in the last 30 days.")).toBeTruthy();
  });
});

function serveRoutes(routes: Record<string, () => Response>) {
  fetchMock.mockImplementation(async (input) => {
    const url = new URL(String(input), "http://localhost");
    const route = Object.keys(routes).find((suffix) => url.pathname.endsWith(suffix));
    return route ? routes[route]!() : problem(404, "not_found");
  });
}

describe("a user", () => {
  test("the summary, the user's sessions and the user's traces are three sections", async () => {
    window.history.replaceState(null, "", "/user/?project=p-1&user=ada");
    serveRoutes({
      "/users/ada": () => json({ user: user(), meta: WHOLE }),
      "/users/ada/sessions": () =>
        json(
          page([
            {
              session_id: "chat-42",
              first_trace_at: "2026-11-17T13:00:00.000000Z",
              last_trace_at: "2026-11-17T13:30:00.000000Z",
              trace_count: 2,
              saturated: false,
            },
          ]),
        ),
      "/traces": () => json(page([])),
    });
    renderWithClient(<UserDetail />);
    expect(await screen.findByRole("link", { name: "chat-42" })).toBeTruthy();
    expect(await screen.findByText("Sessions")).toBeTruthy();
    expect(await screen.findByText("No traces of this user in the last 24 hours.")).toBeTruthy();
    const traces = requests(fetchMock.mock.calls).find((url) => url.pathname.endsWith("/traces"));
    expect(traces?.searchParams.getAll("user_id")).toEqual(["ada"]);
  });

  test("a failing sessions section leaves the summary and the traces standing", async () => {
    window.history.replaceState(null, "", "/user/?project=p-1&user=ada");
    serveRoutes({
      "/users/ada": () => json({ user: user(), meta: WHOLE }),
      "/users/ada/sessions": () => problem(503, "telemetry_store_unavailable"),
      "/traces": () => json(page([])),
    });
    renderWithClient(<UserDetail />);
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Retry sessions of this user" })).toBeTruthy();
    expect(await screen.findByText("Last seen")).toBeTruthy();
    expect(await screen.findByText("No traces of this user in the last 24 hours.")).toBeTruthy();
  });

  test("without a user in the link the page points back to the list", () => {
    window.history.replaceState(null, "", "/user/?project=p-1");
    renderWithClient(<UserDetail />);
    expect(screen.getByText("No user selected.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
