import { act, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { TraceListItem } from "@/api/generated/TraceListItem";
import type { TraceListPage } from "@/api/generated/TraceListPage";
import { WHOLE, json, renderWithClient, requests } from "@/test/render";
import { TracesSection } from "./traces-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/traces/?project=p-1&environment=prod");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function item(id: string): TraceListItem {
  return {
    facts: { tags: [], bookmarked: false, public: null, scores: [], correction: null },
    trace_id: id.repeat(32).slice(0, 32),
    name: `trace ${id}`,
    trace_name: "",
    head_kind: "root",
    start_time: "2026-11-17T14:00:00.000000Z",
    duration_us: 1_000,
    status_code: "ok",
    level: "default",
    span_count: 1,
    error_count: 0,
    total_cost_usd: 0,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    user_id: "",
    session_id: "",
    environment: "prod",
    release: "",
    tags: [],
    saturated: false,
  };
}

function listPage(data: TraceListItem[], head: string): TraceListPage {
  return { data, next_cursor: null, has_more: false, head_cursor: head, meta: WHOLE };
}

// The list answers with head cursor `h-<n>` on its n-th read; live tail answers `newCount`.
function serve(newCount: () => number, empty = false) {
  let reads = 0;
  fetchMock.mockImplementation(async (input) => {
    const url = new URL(String(input), "http://localhost");
    if (url.searchParams.has("since")) {
      return json({
        data: [],
        next_cursor: null,
        has_more: newCount() > 1,
        head_cursor: "h-tail",
        new_count: newCount(),
        meta: WHOLE,
      });
    }
    reads += 1;
    return json(listPage(empty ? [] : [item(String(reads))], `h-${reads}`));
  });
}

const listReads = () =>
  requests(fetchMock.mock.calls).filter((url) => !url.searchParams.has("since"));
const tailReads = () =>
  requests(fetchMock.mock.calls).filter((url) => url.searchParams.has("since"));

describe("live tail", () => {
  test("asks with the page's own window, filters and head cursor, and shows the new traces on request", async () => {
    serve(() => 3);
    renderWithClient(<TracesSection />);
    const show = await screen.findByRole("button", { name: "Show 3 new traces" });
    const [list] = listReads();
    const [tail] = tailReads();
    expect(tail?.searchParams.get("since")).toBe("h-1");
    expect(tail?.searchParams.get("from")).toBe(list?.searchParams.get("from"));
    expect(tail?.searchParams.get("to")).toBe(list?.searchParams.get("to"));
    expect(tail?.searchParams.getAll("environment")).toEqual(["prod"]);
    await act(async () => show.click());
    expect(await screen.findByRole("link", { name: "trace 2" })).toBeTruthy();
    expect(listReads()).toHaveLength(2);
  });

  test("an empty list counts the first traces too", async () => {
    serve(() => 2, true);
    renderWithClient(<TracesSection />);
    expect(
      await screen.findByText("No traces match these filters in the last 24 hours."),
    ).toBeTruthy();
    expect(await screen.findByRole("button", { name: "Show 2 new traces" })).toBeTruthy();
  });

  test("nothing new shows nothing", async () => {
    serve(() => 0);
    renderWithClient(<TracesSection />);
    await screen.findByRole("link", { name: "trace 1" });
    await vi.waitFor(() => expect(tailReads().length).toBeGreaterThan(0));
    expect(screen.queryByRole("button", { name: /new trace/ })).toBeNull();
    expect(screen.queryByText(/Couldn't check for new traces/)).toBeNull();
  });

  test("a check that fails says so quietly, and the list stays", async () => {
    fetchMock.mockImplementation(async (input) => {
      const url = new URL(String(input), "http://localhost");
      if (url.searchParams.has("since")) throw new TypeError("Failed to fetch");
      return json(listPage([item("1")], "h-1"));
    });
    renderWithClient(<TracesSection />);
    await screen.findByRole("link", { name: "trace 1" });
    expect(
      await screen.findByText("Couldn't check for new traces. Checking again every 5 seconds."),
    ).toBeTruthy();
    expect(screen.getByRole("link", { name: "trace 1" })).toBeTruthy();
  });

  test("when the page's window ends with nothing new, the list reads the next window", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.setSystemTime(new Date("2026-11-17T14:00:30Z"));
    serve(() => 0);
    renderWithClient(<TracesSection />);
    await screen.findByRole("link", { name: "trace 1" });
    expect(listReads()[0]?.searchParams.get("to")).toBe("2026-11-17T14:01:00.000000Z");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(31_000);
    });
    expect(await screen.findByRole("link", { name: "trace 2" })).toBeTruthy();
    expect(listReads()[1]?.searchParams.get("to")).toBe("2026-11-17T14:02:00.000000Z");
  });

  test("a count reached before the window ends stays on offer", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.setSystemTime(new Date("2026-11-17T14:00:30Z"));
    serve(() => 1);
    renderWithClient(<TracesSection />);
    expect(await screen.findByRole("button", { name: "Show 1 new trace" })).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(31_000);
    });
    expect(listReads()).toHaveLength(1);
    expect(screen.getByRole("button", { name: "Show 1 new trace" })).toBeTruthy();
  });
});
