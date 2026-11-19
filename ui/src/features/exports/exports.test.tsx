import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Export } from "@/api/generated/Export";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { PROJECT_A, sessionInfo, sessionProject } from "@/test/session";
import { ExportsSettings } from "./exports";

vi.mock("next/navigation", () => import("@/test/navigation"));

const agent = createAgent();
const fetchMock = vi.fn<typeof fetch>();
const LIST = `/api/v1/projects/${PROJECT_A}/exports`;

type Handler = (init: RequestInit | undefined) => Response | Promise<Response>;

function serve(routes: Record<string, Handler>) {
  fetchMock.mockImplementation(async (input, init) => {
    const route = `${init?.method ?? "GET"} ${String(input)}`;
    const handler = routes[route];
    if (!handler) throw new Error(`no route for ${route}`);
    return handler(init);
  });
}

function requests(route: string): RequestInit[] {
  return fetchMock.mock.calls
    .filter(([input, init]) => `${init?.method ?? "GET"} ${String(input)}` === route)
    .map(([, init]) => init ?? {});
}

const json =
  (body: unknown, status = 200) =>
  () =>
    new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const problem =
  (
    status: number,
    code: string,
    extra: Record<string, unknown> = {},
    headers: Record<string, string> = {},
  ) =>
  () =>
    new Response(JSON.stringify({ code, status, request_id: "req-7", ...extra }), {
      status,
      headers: { "content-type": "application/problem+json", ...headers },
    });

const item = (fields: Partial<Export>): Export => ({
  id: "0193a1b2-7c3d-7e4f-8a5b-000000000001",
  format: "crucible-jsonl",
  state: "ready",
  from: "2026-11-16T09:00:00.000000Z",
  to: "2026-11-17T09:00:00.000000Z",
  trace_count: 0,
  gzip: false,
  keep_truncated: false,
  created_at: "2026-11-17T09:00:05.000000Z",
  as_of: "2026-11-17T09:00:05.000000Z",
  spans: 1200,
  skipped: 0,
  bytes: 2_500_000,
  checksum: "ab".repeat(32),
  expires_at: "2026-11-24T09:01:00.000000Z",
  download_path: `/api/v1/projects/${PROJECT_A}/exports/0193a1b2-7c3d-7e4f-8a5b-000000000001/download`,
  saturated: false,
  ...fields,
});

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", `/settings/exports/?project=${PROJECT_A}`);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function renderPage() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <ExportsSettings />
    </QueryClientProvider>,
  );
}

function signIn(permissions = ["read", "export"]) {
  const session = sessionInfo({ csrf_token: "csrf-5" });
  setAuth({
    mode: "session",
    session: { ...session, projects: [sessionProject({ permissions }), session.projects[1]] },
  });
}

describe("exports", () => {
  test("loading shows a skeleton after a short wait, then the exports with their state", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    signIn();
    let release: ((response: Response) => void) | undefined;
    serve({
      [`GET ${LIST}`]: () =>
        new Promise<Response>((resolve) => {
          release = resolve;
        }),
    });
    const { container } = renderPage();
    // No skeleton flashes for a fast answer: it stays invisible for the first 300 ms.
    const skeleton = () => container.querySelector(".skeleton")?.closest("[aria-hidden]");
    expect(skeleton()?.classList.contains("invisible")).toBe(true);
    await act(async () => {
      vi.advanceTimersByTime(400);
    });
    expect(skeleton()?.classList.contains("invisible")).toBe(false);
    expect(screen.getByText("Loading Exports")).toBeTruthy();
    const failed = item({
      id: "e2",
      state: "failed",
      error_code: "export_too_large",
      spans: 0,
      bytes: 0,
      download_path: undefined,
      checksum: undefined,
      expires_at: undefined,
    });
    const running = item({
      id: "e3",
      state: "running",
      spans: 300,
      download_path: undefined,
      checksum: undefined,
      expires_at: undefined,
      format: "otlp-json",
      gzip: true,
    });
    await act(async () => {
      release?.(json({ data: [item({}), failed, running] })());
    });
    const rows = await screen.findAllByRole("row");
    expect(rows).toHaveLength(4);
    expect(within(rows[1]).getByText("Ready")).toBeTruthy();
    const download = within(rows[1]).getByRole("link", { name: /Download/ });
    expect(download.getAttribute("href")).toBe(item({}).download_path);
    expect(download.hasAttribute("download")).toBe(true);
    expect(within(rows[2]).getByText("Failed")).toBeTruthy();
    expect(within(rows[2]).getByText(/didn't finish within 30 minutes/)).toBeTruthy();
    expect(within(rows[3]).getByText("Writing, 300 spans so far")).toBeTruthy();
    expect(within(rows[3]).getByText("OTLP JSON, gzip")).toBeTruthy();
  });

  test("an empty list invites the first export, and a failed list says so with a retry", async () => {
    signIn();
    serve({ [`GET ${LIST}`]: json({ data: [] }) });
    const first = renderPage();
    expect(await screen.findByText("No exports yet.")).toBeTruthy();
    first.unmount();

    fetchMock.mockReset();
    serve({ [`GET ${LIST}`]: problem(503, "control_plane_unavailable") });
    const { container } = renderPage();
    const retry = await screen.findByRole("button", { name: "Retry Exports" });
    const card = container.querySelector("[data-error]");
    expect(card).not.toBeNull();
    expect(card?.textContent).not.toContain("control_plane_unavailable");
    serve({ [`GET ${LIST}`]: json({ data: [item({})] }) });
    fireEvent.click(retry);
    expect(await screen.findByText("Ready")).toBeTruthy();
  });

  test("starting an export sends the range, the format and one idempotency key, then lists it", async () => {
    signIn();
    let listed: Export[] = [];
    serve({
      [`GET ${LIST}`]: () => json({ data: listed })(),
      [`POST ${LIST}`]: (init) => {
        listed = [item({ state: "queued", spans: 0, download_path: undefined })];
        return json(JSON.parse(String(init?.body)), 202)();
      },
    });
    renderPage();
    await screen.findByText("No exports yet.");
    fireEvent.click(screen.getByRole("button", { name: "Export spans" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByRole("radio", { name: "OTLP JSON" }));
    fireEvent.change(within(dialog).getByLabelText("User"), { target: { value: " user-42 " } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start export" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(await screen.findByText("Queued")).toBeTruthy();
    const [sent] = requests(`POST ${LIST}`);
    const body = JSON.parse(String(sent.body));
    expect(body).toMatchObject({ format: "otlp-json", user_id: "user-42", gzip: false });
    expect(Date.parse(body.to) - Date.parse(body.from)).toBe(24 * 3_600_000);
    const headers = sent.headers as Record<string, string>;
    expect(headers["Idempotency-Key"]).toMatch(/^[0-9a-f]{32}$/);
    expect(headers["X-Crucible-CSRF"]).toBe("csrf-5");
  });

  test("the hourly limit says when to try again, in the dialog", async () => {
    signIn();
    serve({
      [`GET ${LIST}`]: json({ data: [item({})] }),
      [`POST ${LIST}`]: problem(
        429,
        "rate_limited",
        { retry_after_s: 1500 },
        { "retry-after": "1500" },
      ),
    });
    renderPage();
    await screen.findByText("Ready");
    fireEvent.click(screen.getByRole("button", { name: "Export spans" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Start export" }));
    const alert = await within(dialog).findByRole("alert");
    expect(alert.textContent).toContain("A project can start 5 exports an hour.");
    expect(alert.textContent).toContain("Try again in 25 minutes.");
  });

  test("a viewer sees the exports without the control to start one", async () => {
    signIn(["read"]);
    serve({ [`GET ${LIST}`]: json({ data: [] }) });
    renderPage();
    expect(await screen.findByText(/Members and admins of Checkout can export/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Export spans" })).toBeNull();
    const labels = agent.snapshot().map((entry) => entry.label);
    expect(labels).not.toContain("Export spans");
  });

  test("without a control plane, or for a project outside the session, nothing is fetched", () => {
    setAuth({ mode: "keys" });
    const keys = renderPage();
    expect(screen.getByText("Exports need a control plane.")).toBeTruthy();
    keys.unmount();
    signIn();
    window.history.replaceState(
      null,
      "",
      "/settings/exports/?project=0193a1b2-0000-7000-8000-00000000ffff",
    );
    renderPage();
    expect(screen.getByText("This project isn't available to you.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
