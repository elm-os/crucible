import { QueryClientProvider, useQuery } from "@tanstack/react-query";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { getTrace } from "@/api/client";
import type { Score } from "@/api/generated/Score";
import type { TraceDetail } from "@/api/generated/TraceDetail";
import type { TraceFacts } from "@/api/generated/TraceFacts";
import { createQueryClient } from "@/components/providers";
import { parseScoreValue, shownFacts, TraceFactsSection } from "./trace-facts";

const fetchMock = vi.fn<typeof fetch>();
const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": status < 300 ? "application/json" : "application/problem+json" },
  });

function score(fields: Partial<Score> = {}): Score {
  return {
    name: "helpful",
    score_id: null,
    data_type: "boolean",
    value: true,
    source: "feedback",
    comment: "",
    metadata: null,
    fact: {
      fact_id: "f-1",
      version: 10,
      created_at: "2026-11-17T09:30:00.000000Z",
      author_kind: "api_key",
      author_id: "k",
    },
    ...fields,
  };
}

function facts(fields: Partial<TraceFacts> = {}): TraceFacts {
  return { tags: [], bookmarked: false, public: null, scores: [], correction: null, ...fields };
}

function detail(traceFacts: TraceFacts): TraceDetail {
  return {
    trace_id: TRACE,
    span_count: 1,
    paged: false,
    spans: [
      {
        span_id: "a000000000000001",
        parent_span_id: null,
        name: "root",
        observation_type: "agent",
        start_time: "2026-11-17T09:00:00.000000Z",
        end_time: "2026-11-17T09:00:01.000000Z",
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
    ],
    facts: traceFacts,
    meta: { as_of: "2026-11-17T09:30:00.000000Z", partial: false, consistency: "strong" },
    saturated: false,
  };
}

function Harness() {
  const query = useQuery({
    queryKey: ["p-1", "trace", TRACE],
    queryFn: ({ signal }) => getTrace("p-1", TRACE, signal),
  });
  return <TraceFactsSection project="p-1" trace={TRACE} query={query} />;
}

function renderFacts() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <Harness />
    </QueryClientProvider>,
  );
}

const written = { fact_id: "f-9", key: "k", version: 99, created_at: "x", target_time: "y" };

describe("the facts of a trace", () => {
  test("acknowledged edits stay on screen until a read shows them", () => {
    const server = facts({ tags: ["a", "b"], scores: [score()] });
    const acked = {
      tags: new Map<string, "add" | "remove">([
        ["c", "add"],
        ["a", "remove"],
      ]),
      flags: new Map([["bookmarked" as const, true]]),
      scores: [score({ value: false, fact: { ...score().fact, version: 11 } })],
    };
    const shown = shownFacts(server, acked);
    expect(shown.tags).toEqual(["b", "c"]);
    expect(shown.bookmarked).toBe(true);
    expect(shown.scores.map((s) => s.value)).toEqual([false]);
    // A newer server score replaces the acknowledged one.
    const newer = shownFacts(
      facts({ scores: [score({ fact: { ...score().fact, version: 12 } })] }),
      acked,
    );
    expect(newer.scores.map((s) => s.value)).toEqual([true]);
  });

  test("typed values become numbers, booleans or text", () => {
    expect(parseScoreValue(" 0.5 ")).toBe(0.5);
    expect(parseScoreValue("true")).toBe(true);
    expect(parseScoreValue("good")).toBe("good");
    expect(parseScoreValue("  ")).toBeNull();
  });

  test("empty facts say so, and a new tag is written with an idempotency key", async () => {
    fetchMock.mockImplementation(async (url, init) => {
      if (init?.method === "POST") return json(written, 201);
      return json(detail(facts()));
    });
    renderFacts();
    expect(await screen.findByText("No tags yet.")).toBeTruthy();
    expect(screen.getByText("No scores or feedback yet.")).toBeTruthy();
    const input = screen.getByRole("textbox", { name: "New tag" });
    fireEvent.input(input, { target: { value: "urgent" } });
    await act(async () => screen.getByRole("button", { name: "Add tag" }).click());
    expect(await screen.findByRole("button", { name: "Remove tag urgent" })).toBeTruthy();
    const post = fetchMock.mock.calls.find(([, init]) => init?.method === "POST");
    if (!post) throw new Error("no POST was sent");
    const [url, init] = post;
    expect(String(url)).toBe("/api/v1/projects/p-1/tags");
    expect(JSON.parse(String(init?.body))).toEqual({
      trace_id: TRACE,
      tag: "urgent",
      op: "add",
    });
    const headers = (init?.headers ?? {}) as Record<string, string>;
    expect(headers["Idempotency-Key"]).toBeTruthy();
  });

  test("an empty tag is refused before any request, and a failed write says so", async () => {
    fetchMock.mockImplementation(async (_url, init) => {
      if (init?.method === "POST")
        return json({ code: "ingest_unavailable", request_id: "r-7" }, 503);
      return json(detail(facts({ tags: ["seen"], bookmarked: true })));
    });
    renderFacts();
    await screen.findByText("seen");
    await act(async () => screen.getByRole("button", { name: "Add tag" }).click());
    expect(screen.getAllByText("Enter a tag.").length).toBeGreaterThan(0);
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
    const bookmark = screen.getByRole("button", { name: "Bookmark" });
    expect(bookmark.getAttribute("aria-pressed")).toBe("true");
    await act(async () => bookmark.click());
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText(/Couldn't save the flag/)).toBeTruthy();
    expect(within(alert).getByText("r-7")).toBeTruthy();
    expect(within(alert).getByText(/give your administrator this code/)).toBeTruthy();
    expect(within(alert).queryByText(/Reference/)).toBeNull();
  });

  test("scores show with their comments", async () => {
    fetchMock.mockResolvedValue(
      json(detail(facts({ scores: [score({ comment: "thanks", value: false })] }))),
    );
    renderFacts();
    const row = await screen.findByText("helpful");
    expect(within(row.closest("li") as HTMLElement).getByText("No")).toBeTruthy();
    expect(screen.getByText("thanks")).toBeTruthy();
  });

  test("an agent finds every control by name and can add a tag", async () => {
    fetchMock.mockImplementation(async (_url, init) => {
      if (init?.method === "POST") return json(written, 201);
      return json(detail(facts({ tags: ["seen"] })));
    });
    renderFacts();
    await screen.findByText("seen");
    const agent = createAgent();
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    for (const label of ["Bookmark", "Public", "New tag", "Add tag", "Score name", "Add score"]) {
      expect(labels, `${label} is missing from the snapshot`).toContain(label);
    }
    const result = await agent.act({
      steps: [
        { ref: "New tag", action: "setValue", args: ["agent-tag"] },
        { ref: "Add tag", action: "press" },
      ],
    });
    expect(result.ok, result.error?.message).toBe(true);
    expect(await screen.findByRole("button", { name: "Remove tag agent-tag" })).toBeTruthy();
  });
});
