import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Span } from "@/api/generated/Span";
import type { TraceDetail as TraceDetailBody } from "@/api/generated/TraceDetail";
import { createQueryClient } from "@/components/providers";
import { TraceDetail } from "./trace-detail";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", `/trace/?project=p-1&trace=${TRACE}`);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function renderPage() {
  const client = createQueryClient();
  client.setDefaultOptions({
    queries: { ...client.getDefaultOptions().queries, retry: false },
  });
  return render(
    <QueryClientProvider client={client}>
      <TraceDetail />
    </QueryClientProvider>,
  );
}

const json = (body: unknown, status = 200, type = "application/json") =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": type } });

function span(fields: Partial<Span>): Span {
  return {
    span_id: "00f067aa0ba902b7",
    parent_span_id: null,
    name: "agent run",
    observation_type: "agent",
    start_time: "2026-11-17T14:00:00.000000Z",
    end_time: "2026-11-17T14:00:01.250000Z",
    duration_us: 1_250_000,
    status_code: "ok",
    level: "default",
    model: "",
    total_cost_usd: 0,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    child_count: null,
    saturated: false,
    ...fields,
  };
}

const SPANS = [
  span({}),
  span({
    span_id: "00f067aa0ba902b8",
    parent_span_id: "00f067aa0ba902b7",
    name: "search tool",
    observation_type: "tool",
    start_time: "2026-11-17T14:00:00.012500Z",
    duration_us: 300,
    status_code: "error",
  }),
  span({
    span_id: "00f067aa0ba902b9",
    parent_span_id: "00f067aa0ba902b7",
    name: "",
    start_time: "2026-11-17T14:00:00.900000Z",
    duration_us: 40_000,
    status_code: "unset",
  }),
];

function detail(fields: Partial<TraceDetailBody> = {}): TraceDetailBody {
  return {
    trace_id: TRACE,
    span_count: SPANS.length,
    paged: false,
    spans: SPANS,
    meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
    saturated: false,
    ...fields,
  };
}

function cells(): string[][] {
  return screen
    .getAllByRole("row")
    .slice(1)
    .map((row) => [...row.querySelectorAll("td")].map((cell) => cell.textContent ?? ""));
}

describe("the trace page", () => {
  test("without a trace in the URL, it points back to the list", () => {
    window.history.replaceState(null, "", "/trace/?project=p-1");
    renderPage();
    expect(screen.getByText("No trace selected.")).toBeTruthy();
    const href = screen.getByRole("link", { name: "Open the traces list" }).getAttribute("href");
    expect(href).toMatch(/^\/traces\/?\?project=p-1$/);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("lists the spans with name, start offset, duration and status", async () => {
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    expect(await screen.findByText("agent run")).toBeTruthy();
    expect(cells()).toEqual([
      ["agent run", "0 ms", "1.3 s", "OK"],
      ["search tool", "+13 ms", "<1 ms", "Error"],
      ["Unnamed span", "+900 ms", "40 ms", "Unset"],
    ]);
    for (const [label, status] of [
      ["OK", "ok"],
      ["Error", "error"],
      ["Unset", "unset"],
    ]) {
      expect(screen.getByText(label).getAttribute("data-status")).toBe(status);
    }
    const url = new URL(String(fetchMock.mock.calls[0]?.[0]), "http://localhost");
    expect(url.pathname).toBe(`/api/v1/projects/p-1/traces/${TRACE}`);
    expect(url.search).toBe("");
  });

  test("a paged trace says how many spans it shows", async () => {
    fetchMock.mockResolvedValue(json(detail({ paged: true, span_count: 12_000 })));
    renderPage();
    expect(
      await screen.findByText(
        "Showing the root spans and their children, 3 of 12000. Deeper spans will load with the span tree.",
      ),
    ).toBeTruthy();
  });

  test("an empty answer that may be incomplete offers to check again, distinct from not found", async () => {
    fetchMock.mockResolvedValueOnce(
      json(
        detail({
          spans: [],
          span_count: 0,
          meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "lagging" },
        }),
      ),
    );
    fetchMock.mockResolvedValueOnce(json(detail()));
    renderPage();
    const empty = await screen.findByText("This trace has no spans to show yet.");
    expect(empty.closest("[data-empty]")?.getAttribute("data-empty")).toBe("incomplete");
    expect(screen.queryByText("This isn't in this project.")).toBeNull();
    await act(async () => screen.getByRole("button", { name: "Check again" }).click());
    expect(await screen.findByText("agent run")).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  test("a partial trace says spans may be missing", async () => {
    fetchMock.mockResolvedValue(
      json(
        detail({
          meta: {
            as_of: "2026-11-17T14:00:05.000000Z",
            partial: true,
            partial_reason: "store_unavailable",
            consistency: "strong",
          },
        }),
      ),
    );
    renderPage();
    expect(
      await screen.findByText("Some spans may be missing. Part of the data couldn't be read."),
    ).toBeTruthy();
  });

  test("a trace that does not exist uses the not-found copy", async () => {
    fetchMock.mockResolvedValue(
      json(
        { code: "not_found", title: "Not found", request_id: "r2" },
        404,
        "application/problem+json",
      ),
    );
    renderPage();
    expect(await screen.findByText("This isn't in this project.")).toBeTruthy();
    expect(screen.getByText("r2")).toBeTruthy();
  });

  test("a store outage is a section error with a retry", async () => {
    fetchMock.mockResolvedValue(
      json(
        { code: "telemetry_store_unavailable", status: 503, request_id: "r3" },
        503,
        "application/problem+json",
      ),
    );
    renderPage();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Retry spans" })).toBeTruthy();
  });
});
