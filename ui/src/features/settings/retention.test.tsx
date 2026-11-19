import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { DeletionRequest } from "@/api/generated/DeletionRequest";
import type { Retention } from "@/api/generated/Retention";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { PROJECT_A, sessionInfo, sessionProject } from "@/test/session";
import { parseTraceIds } from "./deletion-requests";
import { RetentionSettings } from "./retention";

vi.mock("next/navigation", () => import("@/test/navigation"));

// Steps on controls marked `data-agent-confirm` run only once a person approves.
let approve = false;
const agent = createAgent({ allow: (step) => !step.confirm || approve });
const fetchMock = vi.fn<typeof fetch>();

type Handler = (init: RequestInit | undefined) => Response;

const RETENTION = `/api/v1/projects/${PROJECT_A}/retention`;
const REQUESTS = `/api/v1/projects/${PROJECT_A}/deletion-requests`;
const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";

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
const problem = (status: number, code: string) => () =>
  new Response(JSON.stringify({ code, status, request_id: "req-9" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

const retention = (fields: Partial<Retention> = {}): Retention => ({
  retention_days: null,
  payload_retention_days: null,
  mcp_content_retention_days: null,
  hidden_before: { retention: null, payload: null, mcp_content: null },
  version: 4,
  ...fields,
});

const request = (fields: Partial<DeletionRequest> = {}): DeletionRequest => ({
  id: "0193a1b2-0000-7000-8000-00000000d001",
  kind: "trace",
  trace_count: 2,
  trace_ids: [TRACE],
  state: "completed",
  verified: true,
  requested_at: "2026-11-17T14:00:00.000000Z",
  requested_by: "user:0193a1b2-0000-7000-8000-000000000001",
  updated_at: "2026-11-17T15:00:00.000000Z",
  traces_resolved: 2,
  places: [],
  ...fields,
});

const FULL = ["read", "write_facts", "edit_content", "manage", "delete_traces", "delete_by_user"];

function signIn(permissions: string[] = FULL) {
  const session = sessionInfo({ csrf_token: "csrf-5" });
  setAuth({
    mode: "session",
    session: { ...session, projects: [sessionProject({ permissions })] },
  });
}

function renderPage() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <RetentionSettings />
    </QueryClientProvider>,
  );
}

const press = (label: string) => agent.act({ steps: [{ ref: label, action: "press" }] });
const fill = (label: string, value: string) =>
  agent.act({ steps: [{ ref: label, action: "setValue", args: [value] }] });

// Presses a control inside the dialog with this title.
function pressIn(title: string, label: string) {
  const dialog = agent.snapshot().find((entry) => entry.label === title)?.ref;
  if (dialog === undefined) throw new Error(`no dialog named ${title}`);
  return agent.act({ steps: [{ ref: dialog, action: "press", args: [label] }] });
}

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  approve = false;
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

describe("retention", () => {
  test("success: the stored settings, and a longer setting saves with If-Match and says so", async () => {
    signIn();
    serve({
      [`GET ${RETENTION}`]: json(retention({ retention_days: 30 })),
      [`PUT ${RETENTION}`]: json(retention({ retention_days: 90, version: 5 })),
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
    });
    renderPage();
    const days = await screen.findByLabelText("Keep data for");
    expect(days).toHaveProperty("value", "30");
    expect(screen.getByRole("button", { name: "Save retention" })).toHaveProperty("disabled", true);

    await act(() => fill("Keep data for", "90 days"));
    await act(() => press("Save retention"));
    await screen.findByText("Saved. Reads follow the new settings now.");
    const [put] = requests(`PUT ${RETENTION}`);
    expect(new Headers(put?.headers).get("if-match")).toBe('"4"');
    expect(new Headers(put?.headers).get("x-crucible-csrf")).toBe("csrf-5");
    expect(JSON.parse(String(put?.body))).toEqual({
      retention_days: 90,
      payload_retention_days: null,
      mcp_content_retention_days: null,
    });
  });

  test("shortening asks first, and nothing is sent until a person approves", async () => {
    signIn();
    serve({
      [`GET ${RETENTION}`]: json(retention()),
      [`PUT ${RETENTION}`]: json(retention({ retention_days: 14, version: 5 })),
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
    });
    renderPage();
    await screen.findByLabelText("Keep data for");
    await act(() => fill("Keep data for", "14"));
    await act(() => press("Save retention"));
    await screen.findByRole("alertdialog", { name: "Shorten retention?" });
    expect(requests(`PUT ${RETENTION}`)).toHaveLength(0);

    const refused = await pressIn("Shorten retention?", "Shorten retention");
    expect(refused.error?.message).toContain("said no");
    expect(requests(`PUT ${RETENTION}`)).toHaveLength(0);
    approve = true;
    await act(() => pressIn("Shorten retention?", "Shorten retention"));
    await waitFor(() => expect(requests(`PUT ${RETENTION}`)).toHaveLength(1));
    await screen.findByText("Saved. Reads follow the new settings now.");
  });

  test("a value that can't be saved says why before anything is sent", async () => {
    signIn();
    serve({
      [`GET ${RETENTION}`]: json(retention({ retention_days: 30 })),
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
    });
    renderPage();
    await screen.findByLabelText("Keep data for");
    await act(() => fill("Keep inputs and outputs for", "45"));
    await screen.findByText(/can't be longer than the data is kept/);
    await act(() => fill("Keep data for", "soon"));
    await screen.findByText("Enter a whole number of days from 1 to 36,500, or leave it empty.");
    await act(() => press("Save retention"));
    expect(requests(`PUT ${RETENTION}`)).toHaveLength(0);
  });

  test("a version that moved on reloads the stored settings and says to enter the change again", async () => {
    signIn();
    let stored = retention({ retention_days: 30 });
    serve({
      [`GET ${RETENTION}`]: () => json(stored)(),
      [`PUT ${RETENTION}`]: () => {
        stored = retention({ retention_days: 60, version: 9 });
        return problem(412, "precondition_failed")();
      },
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
    });
    renderPage();
    await screen.findByLabelText("Keep data for");
    await act(() => fill("Keep data for", "90"));
    await act(() => press("Save retention"));
    await waitFor(() =>
      expect(screen.getByLabelText("Keep data for")).toHaveProperty("value", "60"),
    );
  });

  test("error: the section says so with a retry, and the deletion requests still load", async () => {
    signIn();
    let fail = true;
    serve({
      [`GET ${RETENTION}`]: () =>
        fail ? problem(503, "control_plane_unavailable")() : json(retention())(),
      [`GET ${REQUESTS}`]: json({ data: [request()], next_cursor: null }),
    });
    renderPage();
    await screen.findByText("2 traces");
    const retry = await screen.findByRole("button", { name: /Retry/ });
    expect(screen.queryByLabelText("Keep data for")).toBeNull();
    fail = false;
    await act(async () => retry.click());
    await screen.findByLabelText("Keep data for");
  });

  test("a role that can't manage reads the settings and gets no save control", async () => {
    signIn(["read"]);
    serve({
      [`GET ${RETENTION}`]: json(retention({ retention_days: 30 })),
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
    });
    renderPage();
    const days = await screen.findByLabelText("Keep data for");
    expect(days).toHaveProperty("readOnly", true);
    expect(screen.queryByRole("button", { name: "Save retention" })).toBeNull();
    await screen.findByText(/Only project admins change retention/);
    expect(screen.queryByRole("button", { name: "Delete data" })).toBeNull();
    await screen.findByText(/Members and admins of Checkout can delete traces/);
  });

  test("without a control plane the page says which setting turns it on and requests nothing", () => {
    setAuth({ mode: "keys" });
    renderPage();
    expect(screen.getByText("Retention needs a control plane.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe("deletion requests", () => {
  test("success: each request with its status as icon and label, and its progress", async () => {
    signIn();
    serve({
      [`GET ${RETENTION}`]: json(retention()),
      [`GET ${REQUESTS}`]: json({
        data: [
          request({
            id: "d-2",
            kind: "user",
            user_id: "ada",
            trace_count: undefined,
            trace_ids: undefined,
            state: "applied",
            verified: false,
            places: [
              {
                place: "span_core",
                status: "deleted",
                deletes: 3,
                deletes_done: 3,
                deletes_verified: 1,
              },
            ],
          }),
          request(),
          request({
            id: "d-3",
            kind: "session",
            session_id: "s-9",
            state: "failed",
            error: "raw text",
          }),
        ],
        next_cursor: null,
      }),
    });
    renderPage();
    await screen.findByText("User ada");
    expect(screen.getByText("Deleted")).toBeTruthy();
    expect(screen.getByText("3 of 3 deletes done, 1 removed from storage")).toBeTruthy();
    expect(screen.getByText("Removed")).toBeTruthy();
    expect(screen.getByText("Session s-9")).toBeTruthy();
    expect(screen.getByText("Failed")).toBeTruthy();
    // What the server wrote about the failure stays on the server.
    expect(screen.queryByText(/raw text/)).toBeNull();
  });

  test("empty: says what Delete data is for", async () => {
    signIn();
    serve({
      [`GET ${RETENTION}`]: json(retention()),
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
    });
    renderPage();
    await screen.findByText("No deletion requests yet.");
  });

  test("delete data: checks the ids, needs approval, sends an idempotency key and lists the request", async () => {
    signIn();
    let list: DeletionRequest[] = [];
    serve({
      [`GET ${RETENTION}`]: json(retention()),
      [`GET ${REQUESTS}`]: () => json({ data: list, next_cursor: null })(),
      [`POST ${REQUESTS}`]: () => {
        list = [request({ state: "pending", verified: false, trace_count: 1 })];
        return json(list[0], 201)();
      },
    });
    renderPage();
    await screen.findByText("No deletion requests yet.");
    await act(() => press("Delete data"));
    await screen.findByRole("dialog", { name: "Delete data from Checkout" });

    await act(() => fill("Trace ids", "not-an-id"));
    approve = true;
    await act(() => pressIn("Delete data from Checkout", "Delete data"));
    await screen.findByText(/isn't a trace id/);
    expect(requests(`POST ${REQUESTS}`)).toHaveLength(0);

    await act(() =>
      fill(
        "Trace ids",
        ` ${TRACE.toUpperCase()},
${TRACE} `,
      ),
    );
    await act(() => pressIn("Delete data from Checkout", "Delete data"));
    await screen.findByText("1 trace");
    const [post] = requests(`POST ${REQUESTS}`);
    expect(JSON.parse(String(post?.body))).toEqual({ kind: "trace", trace_ids: [TRACE] });
    expect(new Headers(post?.headers).get("idempotency-key")).toMatch(/^[0-9a-f-]{36}$/);
    expect(screen.getByText("Hidden")).toBeTruthy();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  test("delete data: a failure stays in the dialog with what to do", async () => {
    signIn(["read", "delete_traces"]);
    serve({
      [`GET ${RETENTION}`]: json(retention()),
      [`GET ${REQUESTS}`]: json({ data: [], next_cursor: null }),
      [`POST ${REQUESTS}`]: problem(503, "control_plane_unavailable"),
    });
    renderPage();
    await screen.findByText("No deletion requests yet.");
    await act(() => press("Delete data"));
    await screen.findByRole("dialog", { name: "Delete data from Checkout" });
    // A member deletes traces only: no choice of user or session.
    expect(screen.queryByRole("radiogroup", { name: "What to delete" })).toBeNull();
    approve = true;
    await act(() => fill("Trace ids", TRACE));
    await act(() => pressIn("Delete data from Checkout", "Delete data"));
    await screen.findByText("Crucible can't reach its database.");
    expect(screen.getByRole("dialog", { name: "Delete data from Checkout" })).toBeTruthy();
  });

  test("trace ids are read as people paste them", () => {
    expect(parseTraceIds(` ${TRACE.toUpperCase()}, ${TRACE}\n`)).toEqual({
      ids: [TRACE],
      invalid: null,
    });
    expect(parseTraceIds("abc").invalid).toBe("abc");
    expect(parseTraceIds("  ").ids).toEqual([]);
  });
});
