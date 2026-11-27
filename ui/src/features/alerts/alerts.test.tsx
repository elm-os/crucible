import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { AlertRule } from "@/api/generated/AlertRule";
import type { Channel } from "@/api/generated/Channel";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { PROJECT_A, PROJECT_B, sessionInfo, sessionProject } from "@/test/session";
import { ChannelsSettings } from "./channels";
import { AlertRules } from "./rules";

const navigation = vi.hoisted(() => ({ search: "" }));

vi.mock("next/navigation", async () => ({
  ...(await import("@/test/navigation")),
  useSearchParams: () => new URLSearchParams(navigation.search),
}));

// Steps on controls marked `data-agent-confirm` run only once a person approves.
let approve = false;
const agent = createAgent({ allow: (step) => !step.confirm || approve });
const fetchMock = vi.fn<typeof fetch>();

type Handler = (init: RequestInit | undefined) => Response;

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
  (status: number, code: string, errors?: { pointer: string; code: string }[]) => () =>
    new Response(JSON.stringify({ code, status, request_id: "req-9", errors }), {
      status,
      headers: { "content-type": "application/problem+json" },
    });

const ADMIN = ["read", "write_facts", "edit_content", "channels", "manage"];
const BASE = `/api/v1/projects/${PROJECT_A}`;

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  navigation.search = `project=${PROJECT_A}`;
  const session = sessionInfo({ csrf_token: "csrf-5" });
  setAuth({
    mode: "session",
    session: {
      ...session,
      projects: [sessionProject({ permissions: ADMIN }), session.projects[1]],
    },
  });
});

afterEach(() => {
  approve = false;
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function renderPage(page: ReactNode) {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(<QueryClientProvider client={client}>{page}</QueryClientProvider>);
}

function labels(): string[] {
  const components = agent
    .snapshot()
    .filter((entry) => !entry.offPage && entry.component !== "more");
  for (const entry of components) {
    expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
  }
  return components.map((entry) => entry.label ?? "");
}

// Each row repeats the channel's name, so a control is addressed by its ref, found by its exact label.
function refOf(label: string): string {
  const ref = agent.snapshot().find((entry) => entry.label === label)?.ref;
  if (ref === undefined) throw new Error(`no control named ${label}`);
  return ref;
}

const rule = (fields: Partial<AlertRule> = {}): AlertRule => ({
  id: "0193a1b2-0000-7000-8000-00000000a001",
  name: "Checkout errors",
  enabled: true,
  source: "spans",
  measure: "error_count",
  aggregation: "sum",
  unit: "count",
  dimensions: ["environment"],
  comparison: "gt",
  threshold: 5,
  window_secs: 300,
  interval_secs: 60,
  for_secs: 0,
  no_data: "keep",
  no_data_for_secs: 300,
  renotify_secs: 14_400,
  channels: [],
  status: "firing",
  series: [
    {
      key: [{ dimension: "environment", value: "prod" }],
      status: "firing",
      since: "2026-11-18T10:00:00.000000Z",
      value: 12,
    },
  ],
  last_evaluated_at: "2026-11-18T10:05:00.000000Z",
  eval_failures: 0,
  skipped_ticks: 0,
  created_at: "2026-11-18T09:00:00.000000Z",
  updated_at: "2026-11-18T09:00:00.000000Z",
  saturated: false,
  version: 1,
  ...fields,
});

const channel = (fields: Partial<Channel> = {}): Channel => ({
  id: "0193a1b2-0000-7000-8000-00000000c001",
  name: "On call",
  kind: "webhook",
  enabled: true,
  failures: 0,
  url: "http://10.0.0.8:9000/hook",
  to: [],
  version: 1,
  ...fields,
});

describe("alert rules", () => {
  test("an agent creates a rule: every control has a name and the body is in stored units", async () => {
    serve({
      [`GET ${BASE}/alerts`]: json({ data: [] }),
      [`GET ${BASE}/channels`]: json({ data: [channel()] }),
      [`POST ${BASE}/alerts`]: json(rule(), 201),
    });
    renderPage(<AlertRules />);
    expect(await screen.findByText("No alert rules yet.")).toBeTruthy();

    const opened = await agent.act({ steps: [{ ref: "New rule", action: "press" }] });
    expect(opened.ok, opened.error?.message).toBe(true);
    await screen.findByRole("dialog", { name: "New alert rule for Checkout" });
    await screen.findByText(/On call \(Webhook/);
    const found = labels();
    for (const label of [
      "Name",
      "Measure",
      "Group by",
      "Then by",
      "Fire when the value",
      "Threshold",
      "Over the last (minutes)",
      "Check every",
      "For at least (minutes)",
      "When there is no data",
      "After no data for (minutes)",
      "While it keeps firing, notify again every",
      "Preview",
      "Create rule",
    ]) {
      expect(found, `${label} is missing from the snapshot`).toContain(label);
    }

    const created = await agent.act({
      steps: [
        { ref: "Name", action: "setValue", args: ["Slow checkout"] },
        { ref: "Measure", action: "setValue", args: ["Duration"] },
        { ref: "Threshold, in milliseconds", action: "setValue", args: ["250"] },
        { ref: "Create rule", action: "press" },
      ],
    });
    expect(created.ok, created.error?.message).toBe(true);
    await waitFor(() => expect(requests(`POST ${BASE}/alerts`)).toHaveLength(1));
    const [post] = requests(`POST ${BASE}/alerts`);
    expect(JSON.parse(String(post?.body))).toEqual({
      name: "Slow checkout",
      measure: "duration",
      aggregation: "p50",
      dimensions: [],
      comparison: "gt",
      threshold: 250_000,
      window_secs: 300,
      interval_secs: 60,
      for_secs: 0,
      no_data: "keep",
      no_data_for_secs: 300,
      renotify_secs: 14_400,
      channels: [],
    });
    expect(new Headers(post?.headers).get("x-crucible-csrf")).toBe("csrf-5");
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "New alert rule for Checkout" })).toBeNull(),
    );
  });

  test("an MCP rule reads one side under a capture source, its rate typed in percent", async () => {
    const created = rule({
      source: "mcp_ops",
      side: "client",
      capture_source: "proxy",
      measure: "mcp.error_rate",
      aggregation: "rate",
      unit: "ppm",
    });
    serve({
      [`GET ${BASE}/alerts`]: json({ data: [] }),
      [`GET ${BASE}/channels`]: json({ data: [] }),
      [`POST ${BASE}/alerts`]: json(created, 201),
    });
    renderPage(<AlertRules />);
    await screen.findByText("No alert rules yet.");
    const result = await agent.act({
      steps: [
        { ref: "New rule", action: "press" },
        { ref: "Name", action: "setValue", args: ["Tool errors"] },
        { ref: "Watch", action: "setValue", args: ["MCP calls"] },
        { ref: "Side", action: "setValue", args: ["Client side"] },
        { ref: "Capture source", action: "setValue", args: ["Proxy"] },
        { ref: "Group by", action: "setValue", args: ["Server"] },
        { ref: "Threshold, in percent", action: "setValue", args: ["5"] },
        { ref: "Create rule", action: "press" },
      ],
    });
    expect(result.ok, result.error?.message).toBe(true);
    await waitFor(() => expect(requests(`POST ${BASE}/alerts`)).toHaveLength(1));
    const [post] = requests(`POST ${BASE}/alerts`);
    expect(JSON.parse(String(post?.body))).toEqual({
      name: "Tool errors",
      source: "mcp_ops",
      side: "client",
      capture_source: "proxy",
      measure: "mcp.error_rate",
      aggregation: "rate",
      dimensions: ["server_name"],
      comparison: "gt",
      threshold: 50_000,
      window_secs: 300,
      interval_secs: 60,
      for_secs: 0,
      no_data: "keep",
      no_data_for_secs: 300,
      renotify_secs: 14_400,
      channels: [],
    });
  });

  test("the server's field errors mark their fields, in the page's words", async () => {
    serve({
      [`GET ${BASE}/alerts`]: json({ data: [] }),
      [`GET ${BASE}/channels`]: json({ data: [] }),
      [`POST ${BASE}/alerts`]: problem(422, "validation_failed", [
        { pointer: "/window_secs", code: "invalid" },
      ]),
    });
    renderPage(<AlertRules />);
    await screen.findByText("No alert rules yet.");
    const result = await agent.act({
      steps: [
        { ref: "New rule", action: "press" },
        { ref: "Name", action: "setValue", args: ["Errors"] },
        { ref: "Threshold", action: "setValue", args: ["5"] },
        { ref: "Create rule", action: "press" },
      ],
    });
    expect(result.ok, result.error?.message).toBe(true);
    expect(await screen.findByText("Use a whole number of minutes from 1 to 1440.")).toBeTruthy();
    expect((await screen.findByRole("alert")).textContent).toContain("Some fields need attention.");
    expect(screen.getByText(/No channels yet, so this rule records its state/)).toBeTruthy();
  });

  test("a viewer sees rules and their state, with nothing to change", async () => {
    navigation.search = `project=${PROJECT_B}`;
    serve({
      [`GET /api/v1/projects/${PROJECT_B}/alerts`]: json({
        data: [
          rule(),
          rule({ id: "r-2", name: "Quiet", enabled: false, status: "ok", series: [] }),
        ],
      }),
    });
    renderPage(<AlertRules />);
    const row = await screen.findByRole("row", { name: /Checkout errors/ });
    expect(row.textContent).toContain("Firing");
    expect(row.textContent).toContain("Errors above 5 over 5 min");
    expect(screen.getByRole("row", { name: /Quiet/ }).textContent).toContain("Turned off");
    expect(screen.getByRole("link", { name: "Checkout errors" }).getAttribute("href")).toMatch(
      /^\/alert\/?\?project=.*&id=0193a1b2-0000-7000-8000-00000000a001$/,
    );
    expect(labels()).not.toContain("New rule");
  });

  test("without a control plane nothing is fetched", () => {
    setAuth({ mode: "keys" });
    renderPage(<AlertRules />);
    expect(screen.getByText("Alerts need a control plane.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe("channels", () => {
  test("a test-send shows its outcome in the page's words, never the receiver's", async () => {
    serve({
      [`GET ${BASE}/channels`]: json({ data: [channel()] }),
      [`POST ${BASE}/channels/${channel().id}/test`]: json({
        ok: false,
        status_class: "5xx",
        latency_ms: 31,
        error_code: "receiver_failed",
      }),
    });
    renderPage(<ChannelsSettings />);
    await screen.findByRole("heading", { name: "Channels for Checkout" });
    await waitFor(() => expect(document.querySelector('[data-channel="On call"]')).not.toBeNull());
    const found = labels();
    for (const label of [
      "New channel",
      "Send test to On call",
      "Turn off On call",
      "Rotate secret of On call",
      "Delete this channel?",
    ]) {
      expect(found, `${label} is missing from the snapshot`).toContain(label);
    }
    const sent = await agent.act({
      steps: [{ ref: refOf("Send test to On call"), action: "press" }],
    });
    expect(sent.ok, sent.error?.message).toBe(true);
    expect((await screen.findByRole("alert")).textContent).toContain(
      "The receiver ran into an error of its own.",
    );
  });

  test("rotating the secret needs a person, and the new secret is hidden from agents", async () => {
    const secret = `whsec_${"a".repeat(64)}`;
    serve({
      [`GET ${BASE}/channels`]: json({ data: [channel()] }),
      [`PATCH ${BASE}/channels/${channel().id}`]: json(
        channel({ version: 2, signing_secret: secret }),
      ),
    });
    renderPage(<ChannelsSettings />);
    await screen.findByRole("heading", { name: "Channels for Checkout" });
    await waitFor(() => expect(document.querySelector('[data-channel="On call"]')).not.toBeNull());
    const steps = [{ ref: refOf("Rotate secret of On call"), action: "press" }];
    const refused = await agent.act({ steps });
    expect(refused.error?.message).toContain("said no");
    expect(requests(`PATCH ${BASE}/channels/${channel().id}`)).toHaveLength(0);

    approve = true;
    const rotated = await agent.act({ steps });
    expect(rotated.ok, rotated.error?.message).toBe(true);
    expect(await screen.findByText(secret)).toBeTruthy();
    const [patch] = requests(`PATCH ${BASE}/channels/${channel().id}`);
    expect(JSON.parse(String(patch?.body))).toEqual({ rotate_secret: true });
    expect(new Headers(patch?.headers).get("if-match")).toBe('"1"');
    expect(JSON.stringify(agent.snapshot())).not.toContain(secret);
  });

  test("a refused address says which setting allows it", async () => {
    serve({
      [`GET ${BASE}/channels`]: json({ data: [] }),
      [`POST ${BASE}/channels`]: problem(422, "ssrf_blocked", [
        { pointer: "/url", code: "private_address" },
      ]),
    });
    renderPage(<ChannelsSettings />);
    await screen.findByText("No channels yet.");
    const result = await agent.act({
      steps: [
        { ref: "New channel", action: "press" },
        { ref: "Name", action: "setValue", args: ["Internal"] },
        { ref: "URL", action: "setValue", args: ["https://10.0.0.8/hook"] },
        { ref: "Create channel", action: "press" },
      ],
    });
    expect(result.ok, result.error?.message).toBe(true);
    expect(
      await screen.findByText(/egress\.allow_cidrs, or to set egress\.mode = open/),
    ).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toContain("Crucible won't call that address.");
  });

  test("a viewer gets an explanation, and nothing is fetched", () => {
    navigation.search = `project=${PROJECT_B}`;
    renderPage(<ChannelsSettings />);
    expect(screen.getByText("Only project members and admins see channels.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("members see the channels, without their targets or any control", async () => {
    const session = sessionInfo();
    setAuth({
      mode: "session",
      session: {
        ...session,
        projects: [sessionProject({ role: "member", permissions: ["read", "edit_content"] })],
      },
    });
    serve({
      [`GET ${BASE}/channels`]: json({ data: [channel({ url: undefined })] }),
    });
    renderPage(<ChannelsSettings />);
    await screen.findByRole("heading", { name: "Channels for Checkout" });
    await waitFor(() => expect(document.querySelector('[data-channel="On call"]')).not.toBeNull());
    expect(labels().filter((label) => /^(New|Send|Turn|Rotate|Delete)/.test(label))).toEqual([]);
    expect(screen.queryByText(/10\.0\.0\.8/)).toBeNull();
  });
});
