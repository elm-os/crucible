import { onlineManager, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { TraceListItem } from "@/api/generated/TraceListItem";
import type { TraceListPage } from "@/api/generated/TraceListPage";
import { createQueryClient } from "@/components/providers";
import { TracesSection } from "./traces-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const agent = createAgent();

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

function item(fields: Partial<TraceListItem> = {}): TraceListItem {
  return {
    facts: { tags: [], bookmarked: false, public: null, scores: [], correction: null },
    trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
    name: "POST /checkout",
    trace_name: "checkout agent",
    head_kind: "root",
    start_time: "2026-11-17T14:00:00.000000Z",
    duration_us: 1_250_000,
    status_code: "unset",
    level: "default",
    span_count: 12,
    error_count: 0,
    total_cost_usd: 0.0042,
    input_tokens: 1_200,
    output_tokens: 300,
    total_tokens: 1_500,
    user_id: "",
    session_id: "",
    environment: "prod",
    release: "",
    tags: [],
    saturated: false,
    ...fields,
  };
}

function page(data: TraceListItem[], fields: Partial<TraceListPage> = {}): TraceListPage {
  return {
    data,
    next_cursor: null,
    has_more: false,
    head_cursor: null,
    meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
    ...fields,
  };
}

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
    expect(screen.queryByText(/administrator this code/)).toBeNull();
    expect(screen.queryByText(/Failed to fetch/)).toBeNull();
  });

  test("a static host answers 404 with HTML", async () => {
    fetchMock.mockResolvedValue(html(404));
    renderSection();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.getByText("Try again in a moment.")).toBeTruthy();
    expect(screen.queryByText(/nginx/)).toBeNull();
    expect(screen.queryByText(/No traces/)).toBeNull();
  });

  test("a single-page fallback answers 200 with HTML", async () => {
    fetchMock.mockResolvedValue(html(200));
    renderSection();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.queryByText(/No traces/)).toBeNull();
  });

  test("a proxy answers 502 with a request id, which is shown for an administrator", async () => {
    fetchMock.mockResolvedValue(html(502, { "x-request-id": "0193a5b2-7c1d-7e4f" }));
    renderSection();
    expect(await screen.findByText("0193a5b2-7c1d-7e4f")).toBeTruthy();
    expect(
      screen.getByText(/If this keeps happening, give your administrator this code/),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy code" })).toBeTruthy();
    expect(screen.queryByText(/Reference/)).toBeNull();
  });

  test("retry refetches only this section", async () => {
    fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    fetchMock.mockResolvedValue(json(page([])));
    renderSection();
    const retry = await screen.findByRole("button", { name: "Retry recent traces" });
    await act(async () => retry.click());
    expect(await screen.findByText("No traces in the last 24 hours.")).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

describe("with the API answering", () => {
  test("rows render every column, and the request carries the project and an RFC 3339 window", async () => {
    window.history.replaceState(null, "", "/traces/?project=p-1&range=7d");
    fetchMock.mockResolvedValue(json(page([item({ error_count: 2 })])));
    renderSection();
    const link = await screen.findByRole("link", { name: "checkout agent" });
    // The static export keeps the trailing slash (trailingSlash: true); here Link may drop it.
    const href = new URL(link.getAttribute("href") ?? "", "http://localhost");
    expect(href.pathname).toMatch(/^\/trace\/?$/);
    expect(Object.fromEntries(href.searchParams)).toEqual({
      project: "p-1",
      trace: "4bf92f3577b34da6a3ce929d0e0e4736",
    });
    const headers = screen.getAllByRole("columnheader").map((cell) => cell.textContent);
    expect(headers).toEqual(["Started", "Name", "Duration", "Spans", "Cost", "Status"]);
    const status = screen.getByText("2 errors");
    expect(status.getAttribute("data-status")).toBe("error");
    expect(status.querySelector("svg")).toBeTruthy();
    expect(screen.getByText("1.3 s")).toBeTruthy();
    expect(screen.getByText("12")).toBeTruthy();
    expect(screen.getByText("$0.0042")).toBeTruthy();
    expect(screen.queryByText(/may be missing/)).toBeNull();

    const url = new URL(String(fetchMock.mock.calls[0]?.[0]), "http://localhost");
    expect(url.pathname).toBe("/api/v1/projects/p-1/traces");
    expect([...url.searchParams.keys()]).toEqual(["from", "to"]);
    const from = Date.parse(url.searchParams.get("from") ?? "");
    const to = Date.parse(url.searchParams.get("to") ?? "");
    expect(url.searchParams.get("to")).toMatch(/^\d{4}-\d\d-\d\dT\d\d:\d\d:00\.000000Z$/);
    expect(to - from).toBe(7 * 24 * 60 * 60 * 1000);
  });

  test("a trace without errors is OK, and an unnamed trace falls back to its head span", async () => {
    fetchMock.mockResolvedValue(
      json(page([item({ trace_name: "", total_cost_usd: 0, duration_us: 400 })])),
    );
    renderSection();
    expect(await screen.findByRole("link", { name: "POST /checkout" })).toBeTruthy();
    expect(screen.getByText("OK").getAttribute("data-status")).toBe("ok");
    expect(screen.getByText("$0")).toBeTruthy();
    expect(screen.getByText("<1 ms")).toBeTruthy();
  });

  test("a head span that failed without error-level spans still shows the error", async () => {
    fetchMock.mockResolvedValue(json(page([item({ status_code: "error" })])));
    renderSection();
    const status = await screen.findByText("Error");
    expect(status.getAttribute("data-status")).toBe("error");
  });

  test("a partial page says traces may be missing, and a lagging one that the newest may be", async () => {
    fetchMock.mockResolvedValueOnce(
      json(
        page([item()], {
          meta: {
            as_of: "2026-11-17T14:00:05.000000Z",
            partial: true,
            partial_reason: "store_unavailable",
            consistency: "strong",
          },
        }),
      ),
    );
    const { unmount } = renderSection();
    expect(
      await screen.findByText("Some traces may be missing. Part of the data couldn't be read."),
    ).toBeTruthy();
    expect(screen.queryByText(/store_unavailable/)).toBeNull();
    unmount();

    fetchMock.mockResolvedValueOnce(
      json(
        page([item()], {
          meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "lagging" },
        }),
      ),
    );
    renderSection();
    expect(await screen.findByText("The newest traces may not be shown yet.")).toBeTruthy();
  });

  test("an empty page that is not whole offers to check again instead of saying there are no traces", async () => {
    fetchMock.mockResolvedValueOnce(
      json(
        page([], {
          meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "lagging" },
        }),
      ),
    );
    fetchMock.mockResolvedValueOnce(json(page([item()])));
    renderSection();
    const empty = await screen.findByText("No traces to show yet.");
    expect(empty.closest("[data-empty]")?.getAttribute("data-empty")).toBe("incomplete");
    expect(screen.queryByText(/No traces in/)).toBeNull();
    await act(async () => screen.getByRole("button", { name: "Check again" }).click());
    expect(await screen.findByRole("link", { name: "checkout agent" })).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  test("an agent finds the section's controls by name and can widen the range", async () => {
    fetchMock.mockImplementation(async () => json(page([])));
    renderSection();
    await screen.findByRole("button", { name: "Show last 30 days" });
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    expect(components.map((entry) => entry.label)).toContain("Show last 30 days");

    const result = await agent.act({ steps: [{ ref: "Show last 30 days", action: "press" }] });
    expect(result.ok, result.error?.message).toBe(true);
    expect(await screen.findByText("No traces in the last 30 days.")).toBeTruthy();
    expect(new URLSearchParams(window.location.search).get("range")).toBe("30d");
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
    fetchMock.mockImplementation(async () => json(page([])));
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
