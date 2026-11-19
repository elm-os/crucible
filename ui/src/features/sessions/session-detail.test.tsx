import { screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import {
  LAGGING,
  WHOLE,
  json,
  page,
  problem,
  renderWithClient,
  requests,
  session,
} from "@/test/render";
import { SessionDetail } from "./session-detail";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function answer(summary: Response, traces: Response) {
  fetchMock.mockImplementation(async (input) => {
    const url = new URL(String(input), "http://localhost");
    return url.pathname.endsWith("/traces") ? traces.clone() : summary.clone();
  });
}

describe("a session", () => {
  test("success: the summary and the session's traces load in their own sections", async () => {
    window.history.replaceState(null, "", "/session/?project=p-1&session=chat%2042");
    answer(json({ session: session({ session_id: "chat 42" }), meta: WHOLE }), json(page([])));
    renderWithClient(<SessionDetail />);
    expect(await screen.findByText("Traces")).toBeTruthy();
    expect(screen.getByRole("link", { name: "ada" }).getAttribute("href")).toMatch(/user=ada/);
    expect(await screen.findByText("No traces of this session in the last 24 hours.")).toBeTruthy();
    const urls = requests(fetchMock.mock.calls).map((url) => url.pathname + url.search);
    expect(urls.some((u) => u.startsWith("/api/v1/projects/p-1/sessions/chat%2042?"))).toBe(true);
    const traces = requests(fetchMock.mock.calls).find((url) => url.pathname.endsWith("/traces"));
    expect(traces?.searchParams.getAll("session_id")).toEqual(["chat 42"]);
  });

  test("error: a session that is not there fails its summary only; the traces section still answers", async () => {
    window.history.replaceState(null, "", "/session/?project=p-1&session=gone");
    answer(problem(404, "not_found"), json(page([])));
    renderWithClient(<SessionDetail />);
    expect(await screen.findByText("This isn't in this project.")).toBeTruthy();
    expect(await screen.findByText("No traces of this session in the last 24 hours.")).toBeTruthy();
  });

  test("empty: a session the server could not see whole is incomplete, not missing", async () => {
    window.history.replaceState(null, "", "/session/?project=p-1&session=new");
    answer(json({ session: null, meta: LAGGING }), json(page([])));
    renderWithClient(<SessionDetail />);
    const empty = await screen.findByText("This session has no data to show yet.");
    expect(empty.closest("[data-empty]")?.getAttribute("data-empty")).toBe("incomplete");
    expect(screen.getByRole("button", { name: "Check again" })).toBeTruthy();
  });

  test("without a session in the link the page points back to the list and requests nothing", () => {
    window.history.replaceState(null, "", "/session/?project=p-1");
    renderWithClient(<SessionDetail />);
    expect(screen.getByText("No session selected.")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Open the sessions list" })).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
