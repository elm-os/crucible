import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { IoShape } from "@/api/generated/IoShape";
import type { PayloadField } from "@/api/generated/PayloadField";
import { createQueryClient } from "@/components/providers";
import { AUTO_LOAD_BYTES, PayloadViewer, PREVIEW_BYTES } from "./payload-viewer";

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => vi.stubGlobal("fetch", fetchMock));

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

const SPAN = {
  traceId: "4bf92f3577b34da6a3ce929d0e0e4736",
  spanId: "a000000000000002",
  startTime: "2026-11-17T14:00:00.020000Z",
};

function renderViewer(
  ioShape: IoShape | null = null,
  tab: { field?: PayloadField; noun?: string; plural?: boolean; truncated?: boolean | null } = {},
) {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <PayloadViewer
        project="p1"
        span={SPAN}
        field={tab.field ?? "input"}
        noun={tab.noun ?? "input"}
        plural={tab.plural}
        truncated={tab.truncated}
        ioShape={ioShape}
      />
    </QueryClientProvider>,
  );
}

const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ code, status, request_id: "req-1" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

// Answers range requests over `body` as the server does: 206 with the field's length, the whole body for
// an empty field, 416 past the end.
function serve(body: string, fail?: (first: number) => Response | null) {
  const bytes = new TextEncoder().encode(body);
  fetchMock.mockImplementation(async (_url, init) => {
    const range = new Headers(init?.headers).get("range") ?? "";
    const match = /^bytes=(\d+)-(\d*)$/.exec(range);
    if (!match) return new Response(bytes, { status: 200 });
    const first = Number(match[1]);
    const failure = fail?.(first) ?? null;
    if (failure) return failure;
    if (bytes.length === 0) return new Response(bytes, { status: 200 });
    if (first >= bytes.length) {
      return new Response(null, {
        status: 416,
        headers: { "content-range": `bytes */${bytes.length}` },
      });
    }
    const last = Math.min(match[2] ? Number(match[2]) : bytes.length - 1, bytes.length - 1);
    return new Response(bytes.slice(first, last + 1), {
      status: 206,
      headers: { "content-range": `bytes ${first}-${last}/${bytes.length}` },
    });
  });
}

function requestedRanges(): string[] {
  return fetchMock.mock.calls.map(([, init]) => new Headers(init?.headers).get("range") ?? "");
}

describe("the payload viewer", () => {
  test("loading: it asks for the first kibibyte only and says it is busy", () => {
    fetchMock.mockReturnValue(new Promise(() => {}));
    const { container } = renderViewer();
    expect(container.querySelector("[aria-busy=true]")).not.toBeNull();
    expect(requestedRanges()).toEqual([`bytes=0-${PREVIEW_BYTES - 1}`]);
    const url = String(fetchMock.mock.calls[0]?.[0]);
    expect(url).toContain(`/api/v1/projects/p1/spans/${SPAN.traceId}/${SPAN.spanId}/payload?`);
    expect(url).toContain("field=input");
    expect(url).toContain(`start_time=${encodeURIComponent(SPAN.startTime)}`);
  });

  test("empty: an empty field says nothing was recorded", async () => {
    serve("");
    renderViewer();
    expect(await screen.findByText("No input was recorded.")).toBeTruthy();
  });

  test("empty: a plural field's copy agrees with it", async () => {
    serve("");
    const attributes = { field: "attributes", noun: "attributes", plural: true } as const;
    const { unmount } = renderViewer(null, attributes);
    expect(await screen.findByText("No attributes were recorded.")).toBeTruthy();
    unmount();
    renderViewer(null, { field: "events", noun: "events", plural: true });
    expect(await screen.findByText("No events were recorded.")).toBeTruthy();
  });

  test("empty: a payload that is not stored says why it may be gone", async () => {
    fetchMock.mockImplementation(async () => problem(404, "not_found"));
    const { unmount } = renderViewer();
    expect(await screen.findByText("This span's input isn't stored.")).toBeTruthy();
    unmount();
    renderViewer(null, { field: "events", noun: "events", plural: true });
    expect(await screen.findByText("This span's events aren't stored.")).toBeTruthy();
  });

  test("error: a failed read offers a retry that reads again", async () => {
    fetchMock.mockResolvedValue(problem(503, "telemetry_store_unavailable"));
    renderViewer();
    expect(await screen.findByText("This section couldn't load.")).toBeTruthy();
    serve('{"ok": true}');
    fireEvent.click(screen.getByRole("button", { name: "Retry input" }));
    expect(await screen.findByRole("tree", { name: "Input" })).toBeTruthy();
  });

  test("success: a small JSON field opens as a tree whose branches load their children", async () => {
    serve('{"model": "gpt-4.1", "params": {"temperature": 0.2}}');
    renderViewer();
    const tree = await screen.findByRole("tree", { name: "Input" });
    expect(tree.textContent).toContain('model:"gpt-4.1"');
    expect(tree.textContent).toContain("params:{1 key}");
    expect(requestedRanges()).toHaveLength(1);
  });

  test("success: GenAI messages show as a conversation first, with the JSON one choice away", async () => {
    serve(
      JSON.stringify([
        { role: "user", parts: [{ type: "text", content: "What is the forecast?" }] },
        { role: "assistant", parts: [{ type: "tool_call", name: "get_forecast", arguments: {} }] },
      ]),
    );
    renderViewer("genai_messages");
    const list = await screen.findByRole("list", { name: "Input messages" });
    expect(list.textContent).toContain("What is the forecast?");
    expect(list.textContent).toContain("get_forecast");
    fireEvent.click(screen.getByRole("radio", { name: "JSON" }));
    expect(await screen.findByRole("tree", { name: "Input" })).toBeTruthy();
  });

  test("success: without a message shape the same list is a tree", async () => {
    serve(JSON.stringify([{ role: "user", content: "hi" }]));
    renderViewer(null);
    expect(await screen.findByRole("tree", { name: "Input" })).toBeTruthy();
    expect(screen.queryByRole("radio", { name: "Messages" })).toBeNull();
  });

  test("success: up to 256 KiB the rest loads by itself; text shows as text", async () => {
    const body = "word ".repeat(1_000);
    serve(body);
    renderViewer(null, { truncated: false });
    expect(await screen.findByText("Text")).toBeTruthy();
    expect(screen.queryByText(/cut at the size limit/)).toBeNull();
    expect(requestedRanges()).toEqual([
      `bytes=0-${PREVIEW_BYTES - 1}`,
      `bytes=${PREVIEW_BYTES}-${body.length - 1}`,
    ]);
  });

  test("success: only a field the span says was cut carries the truncation notice", async () => {
    serve('{"messages": [{"role": "user", "content": "invoice INV-');
    renderViewer(null, { truncated: true });
    expect(
      await screen.findByText("The input was cut at the size limit, so it shows as text."),
    ).toBeTruthy();
    expect(screen.queryByText("Text")).toBeNull();
  });

  test("success: short text the span did not cut, or whose cut is not known, is labelled text", async () => {
    serve("invoice INV-7");
    renderViewer(null, { truncated: null });
    expect(await screen.findByText("Text")).toBeTruthy();
    expect(screen.getByText("invoice INV-7")).toBeTruthy();
    expect(screen.queryByText(/cut at the size limit/)).toBeNull();
  });

  test("a larger field shows its preview and loads by ranges only when asked", async () => {
    const body = JSON.stringify({ text: "x".repeat(AUTO_LOAD_BYTES * 2) });
    serve(body);
    renderViewer();
    const load = await screen.findByRole("button", { name: /^Load / });
    expect(screen.getByText(/showing the first 1 KB/)).toBeTruthy();
    expect(requestedRanges()).toHaveLength(1);
    fireEvent.click(load);
    expect(await screen.findByRole("tree", { name: "Input" }, { timeout: 5_000 })).toBeTruthy();
    expect(requestedRanges()[1]).toBe(`bytes=${PREVIEW_BYTES}-${body.length - 1}`);
  });

  test("error: when the rest fails, the preview stays and the retry reads the rest again", async () => {
    const body = "y".repeat(5_000);
    let failing = true;
    serve(body, (first) =>
      first > 0 && failing ? problem(503, "telemetry_store_unavailable") : null,
    );
    renderViewer();
    expect(
      await screen.findByText("Couldn't load the full input. The preview above is intact."),
    ).toBeTruthy();
    expect(screen.getByText(/^y+…$/)).toBeTruthy();
    failing = false;
    fireEvent.click(screen.getByRole("button", { name: "Retry full input" }));
    await waitFor(() => expect(screen.getByText("Text")).toBeTruthy());
  });
});
