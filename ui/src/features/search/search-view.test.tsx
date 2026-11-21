import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { SearchHit } from "@/api/generated/SearchHit";
import type { SearchPage } from "@/api/generated/SearchPage";
import type { SearchRequest } from "@/api/generated/SearchRequest";
import { createQueryClient } from "@/components/providers";
import { SearchView } from "./search-view";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/search/?project=p1");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function renderView() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <SearchView />
    </QueryClientProvider>,
  );
}

const json = (body: unknown, status = 200, type = "application/json") =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": type } });

function hit(spanId: string, name: string, fragments: [string, boolean][]): SearchHit {
  return {
    trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
    span: {
      span_id: spanId,
      parent_span_id: null,
      name,
      observation_type: "generation",
      start_time: "2026-11-17T14:00:00.000000Z",
      end_time: "2026-11-17T14:00:01.000000Z",
      duration_us: 1_000_000,
      status_code: "ok",
      level: "default",
      model: "",
      total_cost_usd: 0,
      input_tokens: 0,
      output_tokens: 0,
      total_tokens: 0,
      child_count: null,
      io_shape: "raw",
      saturated: false,
    },
    matches: [
      {
        field: "input",
        fragments: fragments.map(([text, highlighted]) => ({ text, highlighted })),
        cut_before: true,
        cut_after: false,
      },
    ],
  };
}

function page(data: SearchHit[], next: string | null = null): SearchPage {
  return {
    data,
    next_cursor: next,
    has_more: next !== null,
    meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
  };
}

function bodies(): SearchRequest[] {
  return fetchMock.mock.calls.map(([, init]) => JSON.parse(String(init?.body)) as SearchRequest);
}

function search(text: string) {
  fireEvent.input(screen.getByRole("searchbox", { name: "Find" }), { target: { value: text } });
  fireEvent.click(screen.getByRole("button", { name: "Search" }));
}

describe("search", () => {
  test("idle: without a query nothing is asked and the page says what to do", () => {
    renderView();
    expect(screen.getByText("Type what to find.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("loading: the results section is busy while the first page is asked for", () => {
    fetchMock.mockReturnValue(new Promise(() => {}));
    renderView();
    search("refund");
    expect(screen.getByRole("region", { name: "Results" }).getAttribute("aria-busy")).toBe("true");
    const [request] = bodies();
    expect(request).toMatchObject({ query: "refund", mode: "all", fields: ["input", "output"] });
    expect(window.location.search).toContain("q=refund");
  });

  test("success: hits link to their span and mark what matched; more pages load on request", async () => {
    fetchMock
      .mockResolvedValueOnce(
        json(
          page(
            [
              hit("a000000000000001", "plan reply", [
                ["a ", false],
                ["Refund", true],
              ]),
            ],
            "c2",
          ),
        ),
      )
      .mockResolvedValueOnce(json(page([hit("a000000000000002", "send", [["refund", true]])])));
    renderView();
    search("refund");
    const link = await screen.findByRole("link", { name: "plan reply" });
    expect(link.getAttribute("href")).toMatch(
      /^\/trace\/?\?project=p1&trace=4bf92f3577b34da6a3ce929d0e0e4736&span=a000000000000001$/,
    );
    const marked = document.querySelectorAll("mark");
    expect([...marked].map((m) => m.textContent)).toEqual(["Refund"]);
    expect(screen.getByRole("list", { name: "Matching spans" }).textContent).toContain("…a Refund");
    fireEvent.click(screen.getByRole("button", { name: "Load more results" }));
    expect(await screen.findByRole("link", { name: "send" })).toBeTruthy();
    expect(bodies()[1]?.cursor).toBe("c2");
    expect(screen.queryByRole("button", { name: "Load more results" })).toBeNull();
  });

  test("success: a phrase page without hits reads on by itself", async () => {
    fetchMock
      .mockResolvedValueOnce(json(page([], "c2")))
      .mockResolvedValueOnce(json(page([hit("a000000000000003", "late", [["rate limit", true]])])));
    window.history.replaceState(null, "", "/search/?project=p1&q=rate%20limit&mode=phrase");
    renderView();
    expect(await screen.findByRole("link", { name: "late" })).toBeTruthy();
    expect(bodies().map((b) => b.mode)).toEqual(["phrase", "phrase"]);
  });

  test("empty: no hits says so and offers a wider range", async () => {
    fetchMock.mockResolvedValue(json(page([])));
    renderView();
    search("nothing-like-this");
    expect(
      await screen.findByText('No spans match "nothing-like-this" in the last 24 hours.'),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Search the last 30 days" }));
    expect(window.location.search).toContain("range=30d");
  });

  test("error: a failed search shows the mapped copy, never the server's text", async () => {
    fetchMock.mockResolvedValue(
      new Response(
        JSON.stringify({ code: "query_timeout", title: "raw server detail", status: 504 }),
        {
          status: 504,
          headers: { "content-type": "application/problem+json" },
        },
      ),
    );
    renderView();
    search("refund");
    expect(
      await screen.findByText("This query was too heavy for the selected range."),
    ).toBeTruthy();
    expect(screen.queryByText(/raw server detail/)).toBeNull();
  });

  test("the match and the fields are part of the request", async () => {
    fetchMock.mockResolvedValue(json(page([])));
    window.history.replaceState(
      null,
      "",
      "/search/?project=p1&q=tenant%3Dacme&mode=any&fields=metadata",
    );
    renderView();
    await screen.findByText(/No spans match/);
    expect(bodies()[0]).toMatchObject({ query: "tenant=acme", mode: "any", fields: ["metadata"] });
  });

  test("filters: the filter bar's conditions travel with the search and live in the URL", async () => {
    fetchMock.mockImplementation(async () => json(page([])));
    window.history.replaceState(
      null,
      "",
      "/search/?project=p1&q=refund&environment=prod&level=error&name=checkout",
    );
    renderView();
    expect(await screen.findByText(/No spans match "refund" and these filters/)).toBeTruthy();
    expect(bodies()[0]).toMatchObject({
      query: "refund",
      filters: { environment: ["prod"], level: ["error"], name: "checkout" },
    });
    const chips = screen.getByRole("list", { name: "Active filters" });
    expect(chips.textContent).toContain("Environment: prod");
    expect(chips.textContent).toContain("Name contains: checkout");
    const results = screen.getByRole("region", { name: "Results" });
    fireEvent.click(within(results).getByRole("button", { name: "Clear filters" }));
    await screen.findByText(/No spans match "refund" in the last 24 hours/);
    expect(bodies().at(-1)?.filters).toBeUndefined();
    expect(window.location.search).not.toContain("environment=");
  });

  test("filters: a search without filters sends none", async () => {
    fetchMock.mockResolvedValue(json(page([])));
    renderView();
    search("refund");
    await screen.findByText(/No spans match/);
    expect(bodies()[0]).not.toHaveProperty("filters");
    expect(screen.getByRole("form", { name: "Filter results" })).toBeTruthy();
  });
});
