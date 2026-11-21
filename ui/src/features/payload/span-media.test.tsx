import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Media } from "@/api/generated/Media";
import type { SpanMedia as Reference } from "@/api/generated/SpanMedia";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { sessionInfo } from "@/test/session";
import { SpanMedia } from "./span-media";

const fetchMock = vi.fn<typeof fetch>();

const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";
const SPAN = "a000000000000002";
const START = "2026-11-17T14:00:00.000000Z";
const PNG_URL = "http://objects.example:7070/media/png?X-Amz-Signature=1";
const GUIDE = "https://github.com/elm-os/crucible/blob/main/docs/self-hosting/media.md";

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

// Answers the span's references with `found`, and each media read with the media and a download link.
function serve(found: Reference[], read?: (id: string) => Response) {
  fetchMock.mockImplementation(async (input) => {
    const url = new URL(String(input), "http://crucible.test");
    if (url.pathname.endsWith(`/spans/${TRACE}/${SPAN}/media`)) {
      expect(url.searchParams.get("start_time")).toBe(START);
      return json({ data: found });
    }
    const id = url.pathname.split("/").pop() ?? "";
    if (read) return read(id);
    const held = found.find((reference) => reference.media?.id === id)?.media;
    return held
      ? json({ ...held, url: PNG_URL, url_expires_at: "2026-11-17T14:05:00.000000Z" })
      : problem(404, "not_found");
  });
}

function renderMedia() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <SpanMedia project="p1" span={{ traceId: TRACE, spanId: SPAN, startTime: START }} />
    </QueryClientProvider>,
  );
}

describe("the media of a span", () => {
  test("with media off the panel says so and links the guide, without asking", () => {
    signIn(false);
    renderMedia();
    expect(
      screen.getByText("This server has no object storage, so it keeps no media."),
    ).toBeTruthy();
    const guide = screen.getByRole("link", { name: "How to turn media on" });
    expect(guide.getAttribute("href")).toBe(GUIDE);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("without a session the server is asked, and media_not_supported says the same", async () => {
    setAuth({ mode: "keys" });
    fetchMock.mockResolvedValue(problem(501, "media_not_supported"));
    renderMedia();
    expect(
      await screen.findByText("This server has no object storage, so it keeps no media."),
    ).toBeTruthy();
    expect(screen.getByRole("link", { name: "How to turn media on" })).toBeTruthy();
  });

  test("loading: a held list shows a skeleton after 300 ms and is announced", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const held: ((response: Response) => void)[] = [];
    fetchMock.mockImplementation(() => new Promise<Response>((resolve) => held.push(resolve)));
    renderMedia();
    await act(async () => {
      vi.advanceTimersByTime(350);
    });
    expect(screen.getByRole("status").textContent).toBe("Loading the media of this span");
    serve([]);
    await act(async () => held[0]?.(json({ data: [] })));
    await waitFor(() => expect(screen.queryByRole("status")).toBeNull());
  });

  test("error: a failed list offers a retry of only this section", async () => {
    fetchMock.mockResolvedValueOnce(problem(503, "telemetry_store_unavailable"));
    renderMedia();
    const retry = await screen.findByRole("button", { name: "Retry media of this span" });
    expect(screen.getByText("req-media")).toBeTruthy();
    serve([{ field: "input", pointer: "/0", content_type: "image/png", media: media() }]);
    fireEvent.click(retry);
    expect(await screen.findByRole("img", { name: /Image from the payload/ })).toBeTruthy();
  });

  test("empty: a span without media says so", async () => {
    serve([]);
    renderMedia();
    expect(await screen.findByText("This span references no media.")).toBeTruthy();
  });

  test("success: each reference says where it appears and shows its media", async () => {
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
      {
        field: "input",
        pointer: "/0/content/1/image_url/url",
        content_type: "image/png",
        media: media(),
      },
      { field: "input", pointer: "/0/content/2", content_type: "application/pdf" },
      { field: "metadata", pointer: "/report", content_type: "application/pdf", media: pdf },
      { field: "output", content_type: "image/png", media: pending },
    ]);
    renderMedia();
    const image = await screen.findByRole("img", {
      name: "Image from the payload, image/png, 2 KB",
    });
    expect(image.getAttribute("src")).toBe(PNG_URL);
    expect(screen.getByText("In the input at /0/content/1/image_url/url")).toBeTruthy();
    expect(screen.getByText("In the input at /0/content/2")).toBeTruthy();
    expect(screen.getByText("Not stored")).toBeTruthy();
    expect(screen.getByText("In the metadata at /report")).toBeTruthy();
    const link = await screen.findByRole("link", { name: "Download application/pdf, 2 KB" });
    expect(link.getAttribute("href")).toBe(PNG_URL);
    expect(screen.getByText("Uploaded for the span's output")).toBeTruthy();
    expect(screen.getByText("Upload not finished")).toBeTruthy();
    // The list, then one read per uploaded media: neither the missing nor the unfinished one is read.
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  test("error: an image that fails to load offers a retry that reads a fresh link", async () => {
    serve([{ field: "input", pointer: "", content_type: "image/png", media: media() }]);
    renderMedia();
    expect(await screen.findByText("In the input")).toBeTruthy();
    const image = await screen.findByRole("img", { name: /Image from the payload/ });
    fireEvent.error(image);
    expect(await screen.findByText("This media couldn't be shown.")).toBeTruthy();
    const reads = fetchMock.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Retry media" }));
    expect(await screen.findByRole("img", { name: /Image from the payload/ })).toBeTruthy();
    expect(fetchMock.mock.calls.length).toBe(reads + 1);
  });
});
