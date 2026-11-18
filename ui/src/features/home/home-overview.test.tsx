import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import type { MetricsSeries } from "@/api/generated/MetricsSeries";
import { createQueryClient } from "@/components/providers";
import { HomeOverview } from "./home-overview";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/home/?project=p-1");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": status === 200 ? "application/json" : "application/problem+json" },
  });

function series(measure: string, aggregation: string, values: (number | null)[]): MetricsSeries {
  return {
    measure,
    aggregation,
    unit: measure === "total_cost" ? "usd" : "count",
    key: [],
    other: false,
    points: values.map((value, i) => ({
      bucket: `2026-11-17T09:${String(i * 5).padStart(2, "0")}:00.000000Z`,
      value,
    })),
  };
}

function response(list: MetricsSeries[], partial = false): MetricsResponse {
  return {
    series: list,
    granularity: "five_minutes",
    truncated: false,
    source_tier: "rollup",
    as_of: "2026-11-17T09:30:00.000000Z",
    partial_last_bucket: false,
    saturated: false,
    meta: { as_of: "2026-11-17T09:30:00.000000Z", partial, consistency: "strong" },
  };
}

// Answers each metrics query by the measures it asks for.
function answer(body: (measures: string[]) => Response) {
  fetchMock.mockImplementation(async (_url, init) => {
    const request = JSON.parse(String(init?.body)) as { measures: { measure: string }[] };
    return body(request.measures.map((m) => m.measure));
  });
}

function renderHome() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <HomeOverview />
    </QueryClientProvider>,
  );
}

const section = (name: string) => screen.getByRole("region", { name });

describe("the home tiles and charts", () => {
  test("each tile shows its headline from its own query", async () => {
    answer((measures) => {
      const [first] = measures;
      if (first === "traces") return json(response([series("traces", "sum", [2, 3])]));
      if (first === "total_cost")
        return json(
          response([series("total_cost", "sum", [0.5, 1]), series("count", "sum", [1, 1])]),
        );
      if (first === "total_tokens")
        return json(
          response([series("total_tokens", "sum", [1000, 500]), series("count", "sum", [1, 1])]),
        );
      if (first === "count")
        return json(
          response([series("count", "sum", [8, 2]), series("error_count", "sum", [1, 0])]),
        );
      return json(
        response([
          series("duration", "p50", [1500, null]),
          series("duration", "p95", [9000, null]),
        ]),
      );
    });
    renderHome();
    expect(await within(section("Traces")).findByText("5")).toBeTruthy();
    expect(await within(section("Cost")).findByText("$1.50")).toBeTruthy();
    expect(await within(section("Tokens")).findByText("1,500")).toBeTruthy();
    expect(await within(section("Error rate")).findByText("10%")).toBeTruthy();
    expect(within(section("Error rate")).getByText("1 of 10 spans")).toBeTruthy();
    expect(await within(section("Latency")).findByText("2 ms p50")).toBeTruthy();
    // The charts share the tiles' answers: five queries, no more.
    expect(fetchMock).toHaveBeenCalledTimes(5);
    const latency = section("Latency percentiles");
    expect(within(latency).getByText("p50")).toBeTruthy();
    expect(within(latency).getByText("p95")).toBeTruthy();
    // Every chart has a table view.
    const toggle = within(latency).getByRole("button", { name: "View as table" });
    await act(async () => toggle.click());
    expect(within(latency).getByRole("table")).toBeTruthy();
  });

  test("one failing query leaves the other tiles standing, and its retry refetches only it", async () => {
    let failCost = true;
    answer((measures) => {
      if (measures[0] === "total_cost" && failCost) return json({ code: "internal_error" }, 500);
      return json(response(measures.map((m) => series(m, m === "duration" ? "p50" : "sum", [1]))));
    });
    renderHome();
    const cost = section("Cost");
    expect(await within(cost).findByText("This section couldn't load.")).toBeTruthy();
    expect(await within(section("Tokens")).findByText("1")).toBeTruthy();
    failCost = false;
    const calls = fetchMock.mock.calls.length;
    await act(async () => within(cost).getByRole("button", { name: "Retry cost" }).click());
    expect(await within(cost).findByText("$1.00")).toBeTruthy();
    expect(fetchMock.mock.calls.length).toBe(calls + 1);
  });

  test("no data is empty only when the answer is whole", async () => {
    answer((measures) =>
      json(
        response(
          measures.map((m) => series(m, "sum", [0, 0])),
          measures[0] === "traces",
        ),
      ),
    );
    renderHome();
    expect(await within(section("Cost")).findByText("No data in the last 24 hours.")).toBeTruthy();
    expect(within(section("Cost")).getByRole("button", { name: "Show last 30 days" })).toBeTruthy();
    expect(await within(section("Traces")).findByText("No data to show yet.")).toBeTruthy();
  });

  test("a tile holds its place while loading", () => {
    fetchMock.mockImplementation(() => new Promise(() => {}));
    renderHome();
    expect(section("Traces").getAttribute("aria-busy")).toBe("true");
  });
});
