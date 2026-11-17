import { onlineManager, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { createQueryClient } from "@/components/providers";
import { TracesSection } from "./traces-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/traces/");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  onlineManager.setOnline(true);
});

function renderSection() {
  const client = createQueryClient();
  // Auto-retry counts are covered in errors.test.ts; here every failure should land at once.
  client.setDefaultOptions({
    queries: { ...client.getDefaultOptions().queries, retry: false },
  });
  return render(
    <QueryClientProvider client={client}>
      <TracesSection />
    </QueryClientProvider>,
  );
}

const html = (status: number, headers: Record<string, string> = {}) =>
  new Response("<!doctype html><title>Not Found</title><h1>nginx says no</h1>", {
    status,
    headers: { "content-type": "text/html", ...headers },
  });

const json = (body: unknown, status = 200, type = "application/json") =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": type } });

describe("with no backend, the section shows a designed error, never a blank or empty list", () => {
  test("a browser that reports offline still fetches and shows the error", async () => {
    onlineManager.setOnline(false);
    fetchMock.mockRejectedValue(new TypeError("Failed to fetch"));
    renderSection();
    expect(await screen.findByText("Couldn't reach Crucible.")).toBeTruthy();
  });

  test("the fetch rejects", async () => {
    fetchMock.mockRejectedValue(new TypeError("Failed to fetch"));
    renderSection();
    expect(await screen.findByText("Couldn't reach Crucible.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Retry recent traces" })).toBeTruthy();
    expect(screen.queryByText(/Reference/)).toBeNull();
    expect(screen.queryByText(/Failed to fetch/)).toBeNull();
  });

  test("a static host answers 404 with HTML", async () => {
    fetchMock.mockResolvedValue(html(404));
    renderSection();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.getByText("Nothing was changed. Try again in a moment.")).toBeTruthy();
    expect(screen.queryByText(/nginx/)).toBeNull();
    expect(screen.queryByText(/No traces/)).toBeNull();
  });

  test("a single-page fallback answers 200 with HTML", async () => {
    fetchMock.mockResolvedValue(html(200));
    renderSection();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.queryByText(/No traces/)).toBeNull();
  });

  test("a proxy answers 502 with a request id, which is shown as the reference", async () => {
    fetchMock.mockResolvedValue(html(502, { "x-request-id": "0193a5b2-7c1d-7e4f" }));
    renderSection();
    expect(await screen.findByText("0193a5b2-7c1d-7e4f")).toBeTruthy();
    expect(screen.getByText(/Reference/)).toBeTruthy();
  });

  test("retry refetches only this section", async () => {
    fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    fetchMock.mockResolvedValue(json({ data: [], next_cursor: null, has_more: false }));
    renderSection();
    const retry = await screen.findByRole("button", { name: "Retry recent traces" });
    await act(async () => retry.click());
    expect(await screen.findByText("No traces in the last 24 hours.")).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

describe("with the API answering", () => {
  test("rows render, and the request carries the project and an RFC 3339 window", async () => {
    window.history.replaceState(null, "", "/traces/?project=p-1&range=7d");
    fetchMock.mockResolvedValue(
      json({
        data: [
          {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
            name: "checkout agent",
            start_time: "2026-11-17T14:00:00.000000Z",
            duration_us: 1_250_000,
            span_count: 12,
            error_count: 2,
          },
        ],
        next_cursor: null,
        has_more: false,
      }),
    );
    renderSection();
    const link = await screen.findByRole("link", { name: "checkout agent" });
    expect(link.getAttribute("href")).toContain("trace=4bf92f3577b34da6a3ce929d0e0e4736");
    expect(screen.getByText("2 errors")).toBeTruthy();
    expect(screen.getByText("1.3 s")).toBeTruthy();

    const url = new URL(String(fetchMock.mock.calls[0]?.[0]), "http://localhost");
    expect(url.pathname).toBe("/api/v1/projects/p-1/traces");
    const from = Date.parse(url.searchParams.get("from") ?? "");
    const to = Date.parse(url.searchParams.get("to") ?? "");
    expect(url.searchParams.get("to")).toMatch(/^\d{4}-\d\d-\d\dT\d\d:\d\d:00\.000000Z$/);
    expect(to - from).toBe(7 * 24 * 60 * 60 * 1000);
  });

  test("an entity 404 problem uses the not-found copy", async () => {
    fetchMock.mockResolvedValue(
      json(
        { code: "not_found", title: "project 9 missing", request_id: "r1" },
        404,
        "application/problem+json",
      ),
    );
    renderSection();
    expect(await screen.findByText("This isn't in this project.")).toBeTruthy();
    expect(screen.queryByText(/project 9 missing/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Retry recent traces" })).toBeNull();
  });

  test("an empty window offers a wider range, and the widest range invites the first trace", async () => {
    fetchMock.mockImplementation(async () =>
      json({ data: [], next_cursor: null, has_more: false }),
    );
    renderSection();
    const widen = await screen.findByRole("button", { name: "Show last 30 days" });
    expect(
      screen
        .getByText("No traces in the last 24 hours.")
        .closest("[data-empty]")
        ?.getAttribute("data-empty"),
    ).toBe("filtered");

    await act(async () => widen.click());

    const firstUse = await screen.findByText("No traces in the last 30 days.");
    expect(firstUse.closest("[data-empty]")?.getAttribute("data-empty")).toBe("first-use");
    expect(new URLSearchParams(window.location.search).get("range")).toBe("30d");
  });
});
