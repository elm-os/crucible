import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Span } from "@/api/generated/Span";
import type { SpanPage } from "@/api/generated/SpanPage";
import type { TraceDetail as TraceDetailBody } from "@/api/generated/TraceDetail";
import type { TraceSummary } from "@/api/generated/TraceSummary";
import { createQueryClient } from "@/components/providers";
import { TraceDetail } from "./trace-detail";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const agent = createAgent();
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
    io_shape: null,
    input_truncated: null,
    output_truncated: null,
    saturated: false,
    ...fields,
  };
}

const usage = (input: number, output: number, cost: number): Partial<Span> => ({
  input_tokens: input,
  output_tokens: output,
  total_tokens: input + output,
  total_cost_usd: cost,
});

const SPANS = [
  span({ total_cost_usd: 0.0042 }),
  span({
    span_id: "00f067aa0ba902b8",
    parent_span_id: "00f067aa0ba902b7",
    name: "search tool",
    observation_type: "tool",
    start_time: "2026-11-17T14:00:00.012500Z",
    duration_us: 300,
    status_code: "error",
    level: "error",
  }),
  span({
    span_id: "00f067aa0ba902b9",
    parent_span_id: "00f067aa0ba902b7",
    name: "",
    observation_type: "generation",
    model: "gpt-x",
    start_time: "2026-11-17T14:00:00.900000Z",
    duration_us: 40_000,
    status_code: "unset",
    input_tokens: 1_200,
    output_tokens: 300,
    total_tokens: 1_500,
  }),
];

// What the server says of SPANS as a whole.
function summary(fields: Partial<TraceSummary> = {}): TraceSummary {
  return {
    name: "agent run",
    trace_name: "",
    head_kind: "root",
    start_time: "2026-11-17T14:00:00.000000Z",
    duration_us: 1_250_000,
    status_code: "ok",
    level: "default",
    span_count: SPANS.length,
    error_count: 1,
    total_cost_usd: 0.0042,
    input_tokens: 1_200,
    output_tokens: 300,
    total_tokens: 1_500,
    user_id: "",
    session_id: "",
    environment: "default",
    release: "",
    saturated: false,
    ...fields,
  };
}

function detail(fields: Partial<TraceDetailBody> = {}): TraceDetailBody {
  return {
    trace_id: TRACE,
    span_count: SPANS.length,
    paged: false,
    summary: summary(),
    spans: SPANS,
    facts: { tags: [], bookmarked: false, public: null, scores: [], correction: null },
    meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
    saturated: false,
    ...fields,
  };
}

const lagging = () =>
  detail({
    spans: [],
    span_count: 0,
    summary: null,
    meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "lagging" },
  });

function rows(): string[][] {
  return within(screen.getByRole("treegrid", { name: "Spans" }))
    .getAllByRole("row")
    .slice(1)
    .map((row) =>
      [...row.querySelectorAll('[role="rowheader"], [role="gridcell"]')].map(
        (cell) => cell.textContent ?? "",
      ),
    );
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

  test("a trace id that isn't 32 hex characters is an invalid link, and nothing is fetched", () => {
    window.history.replaceState(null, "", "/trace/?project=p-1&trace=not-a-trace");
    renderPage();
    expect(screen.getByText("This trace link isn't valid.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("the spans form a tree with type, name, error marker and duration", async () => {
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    await screen.findByRole("treegrid", { name: "Spans" });
    expect(rows()).toEqual([
      ["Agent agent run", "", "1.3 s"],
      ["Tool search tool Error", "", "<1 ms"],
      ["Generation Unnamed span", "", "40 ms"],
    ]);
    const bars = within(screen.getByRole("treegrid")).getAllByRole("gridcell");
    expect(bars[0]?.getAttribute("aria-label")).toBe("Starts at 0 ms, lasts 1.3 s");
    const url = new URL(String(fetchMock.mock.calls[0]?.[0]), "http://localhost");
    expect(url.pathname).toBe(`/api/v1/projects/p-1/traces/${TRACE}`);
    expect(url.search).toBe("");
  });

  test("the summary totals the trace, and status reads as icon and label", async () => {
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    const errors = await screen.findByText("1 error");
    expect(errors.getAttribute("data-status")).toBe("error");
    expect(errors.querySelector("svg")).toBeTruthy();
    expect(screen.getByText("$0.0042")).toBeTruthy();
    expect(screen.getByText("1,500")).toBeTruthy();
  });

  test("the header's cost and tokens are the server's, which counts an agent span that repeats its calls once", async () => {
    // An invoke_agent root that reports the run's total usage, and the two chat calls that make it up: the
    // span rows add up to twice the run.
    const chat = (spanId: string, fields: Partial<Span>) =>
      span({
        span_id: spanId,
        parent_span_id: "00f067aa0ba902b7",
        name: "chat gpt-4o-mini",
        observation_type: "generation",
        ...fields,
      });
    const spans = [
      span({ name: "invoke_agent support", ...usage(3_300, 505, 0.000798) }),
      chat("00f067aa0ba902b8", usage(1_800, 280, 0.000438)),
      chat("00f067aa0ba902b9", usage(1_500, 225, 0.00036)),
    ];
    const run = summary({
      name: "invoke_agent support",
      error_count: 0,
      total_cost_usd: 0.000798,
      input_tokens: 3_300,
      output_tokens: 505,
      total_tokens: 3_805,
    });
    fetchMock.mockResolvedValue(json(detail({ spans, summary: run })));
    renderPage();
    expect(await screen.findByText("$0.0008")).toBeTruthy();
    expect(screen.getByText("3,805")).toBeTruthy();
    expect(screen.queryByText("$0.0016")).toBeNull();
    expect(screen.queryByText("7,610")).toBeNull();
  });

  test("the header is titled with the trace name, names the root span, and shows who and where", async () => {
    const named = summary({
      name: "run_agent",
      trace_name: "research assistant",
      user_id: "user-42",
      session_id: "sess-migrate-1",
      environment: "staging",
      release: "2.7.0",
    });
    fetchMock.mockResolvedValue(json(detail({ summary: named })));
    renderPage();
    const title = await screen.findByText("research assistant");
    expect(title.tagName).toBe("P");
    expect(document.querySelector("[data-head-span]")?.textContent).toBe("Root span run_agent");
    const facts = within(screen.getAllByRole("definition")[0]?.closest("dl") as HTMLElement);
    expect(facts.getByRole("link", { name: "user-42" }).getAttribute("href")).toMatch(
      /^\/user\/?\?project=p-1&user=user-42$/,
    );
    expect(facts.getByRole("link", { name: "sess-migrate-1" }).getAttribute("href")).toMatch(
      /^\/session\/?\?project=p-1&session=sess-migrate-1$/,
    );
    expect(facts.getByText("staging")).toBeTruthy();
    expect(facts.getByText("2.7.0")).toBeTruthy();
  });

  test("without a trace name the header is titled with the root span's name alone", async () => {
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    await screen.findByText("1 error");
    const header = screen.getByText("1 error").closest("div")?.parentElement as HTMLElement;
    expect(within(header).getByText("agent run")).toBeTruthy();
    expect(document.querySelector("[data-head-span]")).toBeNull();
    expect(screen.queryByText("User")).toBeNull();
    expect(screen.queryByText("Release")).toBeNull();
  });

  test("choosing a span writes it to the URL and fills the span panel", async () => {
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    const panel = (await screen.findByText("Select a span.")).closest("section");
    if (!panel) throw new Error("the span panel is not a section");
    expect(panel.getAttribute("aria-busy")).toBeNull();
    const row = screen.getByRole("row", { name: /Unnamed span/ });
    await act(async () => {
      row.focus();
      fireEvent.keyDown(row, { key: "Enter" });
    });
    expect(new URLSearchParams(window.location.search).get("span")).toBe("00f067aa0ba902b9");
    expect(row.getAttribute("aria-selected")).toBe("true");
    expect(within(panel).getByText("gpt-x")).toBeTruthy();
    expect(within(panel).getByText("1,200 in, 300 out, 1,500 total")).toBeTruthy();
    // The overview is open; the payload tabs ask for nothing until one is chosen.
    expect(within(panel).getByRole("tab", { name: "Overview" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect(within(panel).getByRole("tab", { name: "Input" })).toBeTruthy();
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes("/payload"))).toBe(false);
    expect(within(panel).getByRole("tab", { name: "Media" })).toBeTruthy();
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes("/media"))).toBe(false);

    // An agent opens the Media tab as a person would, and the span's media are asked for then.
    fetchMock.mockImplementation(async (input) =>
      String(input).includes(`/spans/${TRACE}/`) ? json({ data: [] }) : json(detail()),
    );
    const opened = await agent.act({
      steps: [{ ref: "Span details", action: "setValue", args: ["media"] }],
    });
    expect(opened.ok, opened.error?.message).toBe(true);
    expect(new URLSearchParams(window.location.search).get("tab")).toBe("media");
    expect(await within(panel).findByText("This span references no media.")).toBeTruthy();
  });

  test("a paged trace says its totals cover the whole trace, and expanding fetches children", async () => {
    const parent = span({ child_count: 2 });
    const child = span({
      span_id: "00f067aa0ba902c1",
      parent_span_id: "00f067aa0ba902b7",
      name: "child one",
      child_count: 4,
    });
    const page: SpanPage = {
      data: [
        span({
          span_id: "00f067aa0ba902d1",
          parent_span_id: "00f067aa0ba902c1",
          name: "grandchild",
        }),
      ],
      next_cursor: null,
      has_more: false,
      meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
    };
    fetchMock.mockImplementation(async (input) =>
      String(input).includes("/spans?")
        ? json(page)
        : json(
            detail({
              paged: true,
              span_count: 12_000,
              summary: summary({ span_count: 12_000, total_tokens: 96_000 }),
              spans: [parent, child],
            }),
          ),
    );
    renderPage();
    expect(
      await screen.findByText(
        "This trace is large, so the timeline loads its spans as you expand them. Duration, cost and tokens cover the whole trace.",
      ),
    ).toBeTruthy();
    expect(screen.getByText("96,000")).toBeTruthy();
    expect(screen.getByText("12,000")).toBeTruthy();
    const row = screen.getByRole("row", { name: /child one/ });
    await act(async () => {
      row.focus();
      fireEvent.keyDown(row, { key: "ArrowRight" });
    });
    expect(await screen.findByRole("row", { name: /grandchild/ })).toBeTruthy();
    const childCalls = fetchMock.mock.calls
      .map(([input]) => new URL(String(input), "http://localhost"))
      .filter((url) => url.pathname.endsWith("/spans"));
    expect(childCalls.map((url) => url.searchParams.get("parent_span_id"))).toContain(
      "00f067aa0ba902c1",
    );
  });

  test("an empty answer that may be incomplete offers to check again, distinct from not found", async () => {
    fetchMock.mockResolvedValueOnce(json(lagging()));
    fetchMock.mockResolvedValueOnce(json(detail()));
    renderPage();
    const empty = await screen.findByText("This trace has no spans to show yet.");
    expect(empty.closest("[data-empty]")?.getAttribute("data-empty")).toBe("incomplete");
    expect(screen.queryByText("This isn't in this project.")).toBeNull();
    await act(async () => screen.getByRole("button", { name: "Check again" }).click());
    expect(await screen.findByRole("row", { name: /agent run/ })).toBeTruthy();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  test("an agent finds the page's controls by name, can check again and choose a span", async () => {
    fetchMock.mockResolvedValueOnce(json(lagging()));
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    await screen.findByRole("button", { name: "Check again" });
    const components = () =>
      agent.snapshot().filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components()) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    expect(components().map((entry) => entry.label)).toContain("Check again");

    const checked = await agent.act({ steps: [{ ref: "Check again", action: "press" }] });
    expect(checked.ok, checked.error?.message).toBe(true);
    await screen.findByRole("treegrid", { name: "Spans" });
    for (const entry of components()) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    expect(components().map((entry) => entry.label)).toContain("Spans");

    const chosen = await agent.act({
      steps: [{ ref: "Spans", action: "setValue", args: ["search tool"] }],
    });
    expect(chosen.ok, chosen.error?.message).toBe(true);
    expect(new URLSearchParams(window.location.search).get("span")).toBe("00f067aa0ba902b8");
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

  test("a store outage is one section error with a retry, and the panel stands aside", async () => {
    fetchMock.mockResolvedValue(
      json(
        { code: "telemetry_store_unavailable", status: 503, request_id: "r3" },
        503,
        "application/problem+json",
      ),
    );
    renderPage();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    expect(screen.getAllByRole("button", { name: /^Retry/ }).map((b) => b.textContent)).toEqual([
      "Retry spans",
    ]);
    expect(screen.queryByRole("region", { name: "Span" })).toBeNull();
  });
});

describe("the agent graph view", () => {
  test("an agent finds the graph's nodes, and choosing one filters the timeline", async () => {
    window.history.replaceState(null, "", `/trace/?project=p-1&trace=${TRACE}&view=graph`);
    fetchMock.mockResolvedValue(json(detail()));
    renderPage();
    const graph = await screen.findByRole("listbox", { name: "Agent graph" });
    // Flow order: left to right, then top to bottom.
    expect(
      within(graph)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual([
      "Agent agent run 1 span ",
      "Model gpt-x 1 span ",
      "Tool search tool 1 span 1 error",
    ]);
    const tool = within(graph).getByRole("option", { name: /search tool/ });
    const description = document.getElementById(tool.getAttribute("aria-describedby") ?? "");
    expect(description?.textContent).toBe("Called by agent run 1 time.");

    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    expect(components.map((entry) => entry.label)).toEqual(
      expect.arrayContaining(["View", "Agent graph"]),
    );
    const chosen = await agent.act({
      steps: [{ ref: "Agent graph", action: "setValue", args: ["Tool search tool"] }],
    });
    expect(chosen.ok, chosen.error?.message).toBe(true);
    expect(tool.getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText("The timeline shows the 1 span of tool search tool.")).toBeTruthy();

    await act(async () => screen.getByRole("button", { name: "Show in timeline" }).click());
    expect(new URLSearchParams(window.location.search).get("view")).toBeNull();
    expect(rows().map(([name]) => name)).toEqual(["Agent agent run", "Tool search tool Error"]);
    await act(async () => screen.getByRole("button", { name: "Show all spans" }).click());
    expect(rows()).toHaveLength(3);
  });
});
