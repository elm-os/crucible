import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Media } from "@/api/generated/Media";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { sessionInfo } from "@/test/session";
import { MediaUses } from "./media-uses";

const fetchMock = vi.fn<typeof fetch>();

const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";
const SPAN = "a000000000000002";
const PNG_URL = "http://objects.example:7070/media/png?X-Amz-Signature=1";

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  signIn(true);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  setAuth({ mode: "unknown" });
});

function signIn(enabled: boolean) {
  const session = sessionInfo();
  session.features.media = { enabled, max_bytes: enabled ? 67_108_864 : 0 };
  setAuth({ mode: "session", session });
}

function media(fields: Partial<Media> = {}): Media {
  return {
    id: "0193a1b2-0000-7000-8000-00000000me01",
    content_type: "image/png",
    len: 2_048,
    sha256: "ab".repeat(32),
    state: "uploaded",
    disposition: "inline",
    created_at: "2026-11-17T14:00:00.000000Z",
    uploaded_at: "2026-11-17T14:00:01.000000Z",
    saturated: false,
    ...fields,
  };
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ code, status, request_id: "req-media" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

// Answers the list with `uses` and each media read with the media and a download link.
function serve(uses: { field: string; media: Media }[], read?: (id: string) => Response) {
  fetchMock.mockImplementation(async (input) => {
    const url = new URL(String(input), "http://crucible.test");
    if (url.pathname.endsWith("/media")) {
      expect(url.searchParams.get("trace_id")).toBe(TRACE);
      expect(url.searchParams.get("span_id")).toBe(SPAN);
      return json({ data: uses.map((use) => ({ span_id: SPAN, ...use })) });
    }
    const id = url.pathname.split("/").pop() ?? "";
    const found = uses.find((use) => use.media.id === id)?.media;
    if (read) return read(id);
    return found
      ? json({ ...found, url: PNG_URL, url_expires_at: "2026-11-17T14:05:00.000000Z" })
      : problem(404, "not_found");
  });
}

function renderUses(field = "input") {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <MediaUses project="p1" traceId={TRACE} spanId={SPAN} field={field} noun="input" />
    </QueryClientProvider>,
  );
}

describe("media in a payload", () => {
  test("without a session, or with media off, nothing is asked for or shown", () => {
    setAuth({ mode: "keys" });
    const keys = renderUses();
    expect(keys.container.innerHTML).toBe("");
    keys.unmount();
    signIn(false);
    const off = renderUses();
    expect(off.container.innerHTML).toBe("");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("loading: a held list shows a skeleton after 300 ms and is announced", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const held: ((response: Response) => void)[] = [];
    fetchMock.mockImplementation(() => new Promise<Response>((resolve) => held.push(resolve)));
    renderUses();
    await act(async () => {
      vi.advanceTimersByTime(350);
    });
    expect(screen.getByRole("status").textContent).toBe("Loading the media of the input");
    serve([]);
    await act(async () => held[0]?.(json({ data: [] })));
    await waitFor(() => expect(screen.queryByRole("status")).toBeNull());
  });

  test("error: a failed list offers a retry, and no object store shows nothing", async () => {
    fetchMock.mockResolvedValueOnce(problem(503, "control_plane_unavailable"));
    renderUses();
    const retry = await screen.findByRole("button", { name: "Retry media of the input" });
    expect(screen.getByText("req-media")).toBeTruthy();
    serve([{ field: "input", media: media() }]);
    fireEvent.click(retry);
    expect(await screen.findByRole("img", { name: /Image from the payload/ })).toBeTruthy();

    fetchMock.mockReset();
    fetchMock.mockResolvedValue(problem(501, "media_not_supported"));
    const none = renderUses("output");
    await waitFor(() => expect(none.container.innerHTML).toBe(""));
    expect(fetchMock).toHaveBeenCalled();
  });

  test("empty: media of other fields, or none, add nothing to the payload", async () => {
    serve([{ field: "output", media: media() }]);
    const view = renderUses("input");
    await waitFor(() => expect(view.container.innerHTML).toBe(""));
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  test("success: an image is shown inline with its type and size", async () => {
    serve([{ field: "input", media: media() }]);
    renderUses();
    const image = await screen.findByRole("img", {
      name: "Image from the payload, image/png, 2 KB",
    });
    expect(image.getAttribute("src")).toBe(PNG_URL);
    expect(screen.getByRole("region", { name: "Media in the input" })).toBeTruthy();
  });

  test("success: a type the page doesn't show is a download, and an unfinished upload says so", async () => {
    const pdf = media({
      id: "0193a1b2-0000-7000-8000-00000000me02",
      content_type: "application/pdf",
      disposition: "attachment",
    });
    const pending = media({
      id: "0193a1b2-0000-7000-8000-00000000me03",
      state: "pending",
      uploaded_at: undefined,
    });
    serve([
      { field: "input", media: pdf },
      { field: "input", media: pending },
    ]);
    renderUses();
    const link = await screen.findByRole("link", { name: "Download application/pdf, 2 KB" });
    expect(link.getAttribute("href")).toBe(PNG_URL);
    expect(screen.getByText("Upload not finished")).toBeTruthy();
    // The unfinished upload is never read: it has no download.
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  test("error: an image that fails to load offers a retry that reads a fresh link", async () => {
    serve([{ field: "input", media: media() }]);
    renderUses();
    const image = await screen.findByRole("img", { name: /Image from the payload/ });
    fireEvent.error(image);
    expect(await screen.findByText("This media couldn't be shown.")).toBeTruthy();
    const reads = fetchMock.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Retry media" }));
    expect(await screen.findByRole("img", { name: /Image from the payload/ })).toBeTruthy();
    expect(fetchMock.mock.calls.length).toBe(reads + 1);
  });
});
