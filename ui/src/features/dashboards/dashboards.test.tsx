import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { Dashboard } from "@/api/generated/Dashboard";
import type { MetricsSchema } from "@/api/generated/MetricsSchema";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { PROJECT_A, sessionInfo, sessionProject } from "@/test/session";
import { DashboardEditor } from "./dashboard-editor";
import { DashboardList } from "./dashboard-list";
import { DashboardView } from "./dashboard-view";

const navigation = vi.hoisted(() => ({ search: "" }));

vi.mock("next/navigation", async () => ({
  ...(await import("@/test/navigation")),
  useSearchParams: () => new URLSearchParams(navigation.search),
}));

const agent = createAgent({ allow: (step) => !step.confirm });
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

const ID = "0193a1b2-0000-7000-8000-0000000d0001";
const BASE = `/api/v1/projects/${PROJECT_A}`;

const SCHEMA: MetricsSchema = {
  sources: [
    {
      source: "spans",
      measures: [
        {
          id: "count",
          label: "Spans",
          unit: "count",
          aggregations: ["sum"],
          additive: true,
          tier: "rollup",
        },
        {
          id: "duration",
          label: "Duration",
          unit: "us",
          aggregations: ["p50", "p95"],
          additive: false,
          tier: "rollup",
        },
      ],
      dimensions: [{ id: "model", type: "string", high_cardinality: false }],
      filter_fields: [],
      granularities: ["minute", "hour", "day"],
      limits: {
        max_buckets: 500,
        max_series: 20,
        max_points: 10_000,
        max_measures: 4,
        max_dimensions: 2,
      },
    },
  ],
};

const DASHBOARD: Dashboard = {
  id: ID,
  name: "Checkout health",
  description: "",
  widgets: [
    {
      id: "w-spans",
      query: { source: "spans", measures: [{ measure: "count", aggregation: "sum" }] },
      display: { kind: "stat", title: "Spans" },
      layout: { x: 0, y: 0, w: 3, h: 2 },
    },
  ],
  version: 1,
  created_at: "2026-11-18T09:00:00.000000Z",
  updated_at: "2026-11-18T09:00:00.000000Z",
};

const MCP_SCHEMA: MetricsSchema = {
  sources: [
    ...SCHEMA.sources,
    {
      ...SCHEMA.sources[0],
      source: "mcp_ops",
      measures: [
        {
          id: "mcp.calls",
          label: "MCP calls",
          unit: "count",
          aggregations: ["sum"],
          additive: true,
          tier: "rollup",
        },
      ],
      dimensions: [{ id: "server_name", type: "string", high_cardinality: false }],
    },
  ],
};

const EMPTY_METRICS = {
  series: [],
  granularity: "hour",
  truncated: false,
  source_tier: "rollup",
  as_of: "2026-11-18T10:00:00.000000Z",
  partial_last_bucket: false,
  saturated: false,
  meta: { as_of: "2026-11-18T10:00:00.000000Z", partial: false, consistency: "strong" },
};

const MCP_LABELS = ["Side", "Capture source", "Include caller faults"];

const bodyOf = (init: RequestInit | undefined): unknown => JSON.parse(String(init?.body));
const lastQuery = () => bodyOf(requests(`POST ${BASE}/metrics/query`).at(-1));

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("matchMedia", () => ({
    matches: true,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
  const session = sessionInfo({ csrf_token: "csrf-7" });
  setAuth({
    mode: "session",
    session: {
      ...session,
      projects: [sessionProject({ permissions: ["read", "edit_content"] }), session.projects[1]],
    },
  });
});

afterEach(() => {
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

describe("dashboards for agents", () => {
  test("an agent creates a dashboard from the list: every control has a name", async () => {
    navigation.search = `project=${PROJECT_A}`;
    serve({
      [`GET ${BASE}/dashboards`]: json({
        data: [
          {
            id: "overview",
            builtin_key: "overview",
            name: "Agent overview",
            description: "",
            widget_count: 8,
            version: 1,
            updated_at: null,
          },
        ],
      }),
      [`POST ${BASE}/dashboards`]: json({ ...DASHBOARD, widgets: [] }, 201),
    });
    renderPage(<DashboardList />);
    expect(await screen.findByText("No dashboards yet.")).toBeTruthy();
    const found = labels();
    for (const label of ["New dashboard", "Duplicate Agent overview"]) {
      expect(found, `${label} is missing from the snapshot`).toContain(label);
    }
    const opened = await agent.act({ steps: [{ ref: "New dashboard", action: "press" }] });
    expect(opened.ok, opened.error?.message).toBe(true);
    await screen.findByRole("dialog", { name: "New dashboard" });
    const created = await agent.act({
      steps: [
        { ref: "Name", action: "setValue", args: ["Checkout health"] },
        { ref: "Create", action: "press" },
      ],
    });
    expect(created.ok, created.error?.message).toBe(true);
    await waitFor(() => expect(requests(`POST ${BASE}/dashboards`)).toHaveLength(1));
    const [post] = requests(`POST ${BASE}/dashboards`);
    expect(JSON.parse(String(post?.body))).toEqual({ name: "Checkout health" });
    expect(new Headers(post?.headers).get("x-crucible-csrf")).toBe("csrf-7");
  });

  test("an agent adds a widget, moves another through the layout, and saves with the version", async () => {
    navigation.search = `project=${PROJECT_A}&id=${ID}`;
    serve({
      [`GET ${BASE}/dashboards/${ID}`]: json(DASHBOARD),
      [`GET ${BASE}/metrics/schema`]: json(SCHEMA),
      [`POST ${BASE}/metrics/query`]: json({
        series: [],
        granularity: "hour",
        truncated: false,
        source_tier: "rollup",
        as_of: "2026-11-18T10:00:00.000000Z",
        partial_last_bucket: false,
        saturated: false,
        meta: { as_of: "2026-11-18T10:00:00.000000Z", partial: false, consistency: "strong" },
      }),
      [`PATCH ${BASE}/dashboards/${ID}`]: json({ ...DASHBOARD, version: 2 }),
    });
    renderPage(<DashboardEditor />);
    expect(await screen.findByText("Stat of Spans")).toBeTruthy();
    const found = labels();
    for (const label of [
      "Name",
      "Description",
      "Add widget",
      "Save",
      "Discard changes",
      "Configure Spans",
      "Delete Spans",
      "Layout of Checkout health",
    ]) {
      expect(found, `${label} is missing from the snapshot`).toContain(label);
    }

    const moved = await agent.act({
      steps: [{ ref: "Layout of Checkout health", action: "moveWidget", args: ["w-spans", 0, 3] }],
    });
    expect(moved.ok, moved.error?.message).toBe(true);
    const opened = await agent.act({ steps: [{ ref: "Add widget", action: "press" }] });
    expect(opened.ok, opened.error?.message).toBe(true);
    await screen.findByRole("dialog", { name: "Add a widget" });
    await screen.findByText("Group by");
    const sheet = labels();
    for (const label of ["Title", "Measure", "Group by", "Buckets", "Apply", "Cancel"]) {
      expect(sheet, `${label} is missing from the sheet`).toContain(label);
    }
    const added = await agent.act({
      steps: [
        { ref: "Title", action: "setValue", args: ["Latency"] },
        { ref: "Measure", action: "setValue", args: ["Duration p95"] },
        { ref: "Apply", action: "press" },
      ],
    });
    expect(added.ok, added.error?.message).toBe(true);
    await screen.findByText("Time series of Duration p95");
    const saved = await agent.act({ steps: [{ ref: "Save", action: "press" }] });
    expect(saved.ok, saved.error?.message).toBe(true);
    await waitFor(() => expect(requests(`PATCH ${BASE}/dashboards/${ID}`)).toHaveLength(1));
    const [patch] = requests(`PATCH ${BASE}/dashboards/${ID}`);
    expect(new Headers(patch?.headers).get("if-match")).toBe('"1"');
    const body = JSON.parse(String(patch?.body)) as {
      widgets: { id?: string; query?: unknown; layout: unknown }[];
    };
    expect(body.widgets).toHaveLength(2);
    expect(body.widgets.find((w) => w.id === "w-spans")?.layout).toEqual({
      x: 0,
      y: 3,
      w: 3,
      h: 2,
    });
    const latency = body.widgets.find((w) => w.id === undefined);
    expect(latency?.query).toMatchObject({
      measures: [{ measure: "duration", aggregation: "p95" }],
    });
  });
});

describe("an MCP widget in the editor", () => {
  test("reads the side and capture source chosen in the sheet, which shows them only for MCP calls", async () => {
    navigation.search = `project=${PROJECT_A}&id=${ID}`;
    serve({
      [`GET ${BASE}/dashboards/${ID}`]: json(DASHBOARD),
      [`GET ${BASE}/metrics/schema`]: json(MCP_SCHEMA),
      [`POST ${BASE}/metrics/query`]: json(EMPTY_METRICS),
      [`PATCH ${BASE}/dashboards/${ID}`]: json({ ...DASHBOARD, version: 2 }),
    });
    renderPage(<DashboardEditor />);
    expect(await screen.findByText("Stat of Spans")).toBeTruthy();
    const opened = await agent.act({ steps: [{ ref: "Add widget", action: "press" }] });
    expect(opened.ok, opened.error?.message).toBe(true);
    await screen.findByRole("dialog", { name: "Add a widget" });
    await screen.findByText("Group by");
    for (const label of MCP_LABELS) {
      expect(labels(), `${label} shows for agent spans`).not.toContain(label);
    }

    const chosen = await agent.act({
      steps: [
        { ref: "Title", action: "setValue", args: ["Tool errors"] },
        { ref: "Data", action: "setValue", args: ["MCP operations"] },
      ],
    });
    expect(chosen.ok, chosen.error?.message).toBe(true);
    await screen.findByText("Capture source");
    const sheet = labels();
    for (const label of MCP_LABELS) {
      expect(sheet, `${label} is missing from the sheet`).toContain(label);
    }
    const scoped = await agent.act({
      steps: [
        { ref: "Side", action: "setValue", args: ["Client side"] },
        { ref: "Capture source", action: "setValue", args: ["Proxy"] },
        { ref: "Include caller faults", action: "setChecked", args: [true] },
      ],
    });
    expect(scoped.ok, scoped.error?.message).toBe(true);
    await waitFor(() =>
      expect(lastQuery()).toMatchObject({
        source: "mcp_ops",
        side: "client",
        capture_source: "proxy",
        include_caller_faults: true,
      }),
    );

    // Agent spans take none of the three, and the sheet keeps the choices for MCP calls meanwhile.
    const spans = await agent.act({
      steps: [
        { ref: "Data", action: "setValue", args: ["Agent spans"] },
        { ref: "Measure", action: "setValue", args: ["Duration p95"] },
      ],
    });
    expect(spans.ok, spans.error?.message).toBe(true);
    await waitFor(() => expect(screen.queryByText("Capture source")).toBeNull());
    await waitFor(() =>
      expect(lastQuery()).toMatchObject({
        source: "spans",
        measures: [{ measure: "duration", aggregation: "p95" }],
      }),
    );
    for (const member of ["side", "capture_source", "include_caller_faults"]) {
      expect(lastQuery()).not.toHaveProperty(member);
    }

    const saved = await agent.act({
      steps: [
        { ref: "Data", action: "setValue", args: ["MCP operations"] },
        { ref: "Apply", action: "press" },
        { ref: "Save", action: "press" },
      ],
    });
    expect(saved.ok, saved.error?.message).toBe(true);
    await waitFor(() => expect(requests(`PATCH ${BASE}/dashboards/${ID}`)).toHaveLength(1));
    const [patch] = requests(`PATCH ${BASE}/dashboards/${ID}`);
    const body = bodyOf(patch) as { widgets: { id?: string; query?: unknown }[] };
    expect(body.widgets.find((w) => w.id === undefined)?.query).toEqual({
      source: "mcp_ops",
      measures: [{ measure: "mcp.calls", aggregation: "sum" }],
      side: "client",
      capture_source: "proxy",
      include_caller_faults: true,
    });
    expect(body.widgets.find((w) => w.id === "w-spans")?.query).toEqual(
      DASHBOARD.widgets[0]?.query,
    );
  });
});

describe("a widget's filters and chart form in the editor", () => {
  test("an agent filters the widget's data and draws it as areas", async () => {
    navigation.search = `project=${PROJECT_A}&id=${ID}`;
    const spans = SCHEMA.sources[0] as MetricsSchema["sources"][number];
    const filtered: MetricsSchema = {
      sources: [
        {
          ...spans,
          filter_fields: [
            { field: "environment", operator: "in" },
            { field: "level", operator: "in" },
          ],
        },
      ],
    };
    serve({
      [`GET ${BASE}/dashboards/${ID}`]: json(DASHBOARD),
      [`GET ${BASE}/metrics/schema`]: json(filtered),
      [`POST ${BASE}/metrics/query`]: json(EMPTY_METRICS),
      [`PATCH ${BASE}/dashboards/${ID}`]: json({ ...DASHBOARD, version: 2 }),
    });
    renderPage(<DashboardEditor />);
    expect(await screen.findByText("Stat of Spans")).toBeTruthy();
    const opened = await agent.act({ steps: [{ ref: "Add widget", action: "press" }] });
    expect(opened.ok, opened.error?.message).toBe(true);
    await screen.findByRole("dialog", { name: "Add a widget" });
    await screen.findByText("Filter by");
    expect(labels()).toEqual(expect.arrayContaining(["Filter by", "Match", "Add filter"]));

    const built = await agent.act({
      steps: [
        { ref: "Title", action: "setValue", args: ["Errors outside staging"] },
        { ref: "Filter by", action: "setValue", args: ["Level"] },
        { ref: "Value", action: "setValue", args: ["error"] },
        { ref: "Add filter", action: "press" },
        { ref: "Draw as", action: "setValue", args: ["Areas"] },
      ],
    });
    expect(built.ok, built.error?.message).toBe(true);
    await screen.findByRole("button", { name: "Remove filter Level is error" });
    await waitFor(() =>
      expect(lastQuery()).toMatchObject({ filter: { in: { field: "level", values: ["error"] } } }),
    );

    // A second condition joins the first under `and`, and removing a chip drops its condition.
    const more = await agent.act({
      steps: [
        { ref: "Filter by", action: "setValue", args: ["Level"] },
        { ref: "Match", action: "setValue", args: ["is not"] },
        { ref: "Value", action: "setValue", args: ["debug"] },
        { ref: "Add filter", action: "press" },
      ],
    });
    expect(more.ok, more.error?.message).toBe(true);
    await waitFor(() =>
      expect(lastQuery()).toMatchObject({
        filter: {
          and: [
            { in: { field: "level", values: ["error"] } },
            { not: { in: { field: "level", values: ["debug"] } } },
          ],
        },
      }),
    );

    const saved = await agent.act({
      steps: [
        { ref: "Apply", action: "press" },
        { ref: "Save", action: "press" },
      ],
    });
    expect(saved.ok, saved.error?.message).toBe(true);
    await waitFor(() => expect(requests(`PATCH ${BASE}/dashboards/${ID}`)).toHaveLength(1));
    const [patch] = requests(`PATCH ${BASE}/dashboards/${ID}`);
    const body = bodyOf(patch) as {
      widgets: { id?: string; query?: { filter?: unknown }; display: { chart?: string } }[];
    };
    const added = body.widgets.find((w) => w.id === undefined);
    expect(added?.display.chart).toBe("area");
    expect(added?.query?.filter).toEqual({
      and: [
        { in: { field: "level", values: ["error"] } },
        { not: { in: { field: "level", values: ["debug"] } } },
      ],
    });
  });

  test("a filter the sheet can't show is kept as it is, and can be removed", async () => {
    navigation.search = `project=${PROJECT_A}&id=${ID}`;
    const spans = SCHEMA.sources[0] as MetricsSchema["sources"][number];
    const either = {
      or: [
        { in: { field: "level", values: ["error"] } },
        { in: { field: "environment", values: ["prod"] } },
      ],
    };
    const widget = {
      ...(DASHBOARD.widgets[0] as Dashboard["widgets"][number]),
      query: {
        source: "spans",
        measures: [{ measure: "count", aggregation: "sum" }],
        filter: either,
      },
    };
    serve({
      [`GET ${BASE}/dashboards/${ID}`]: json({ ...DASHBOARD, widgets: [widget] }),
      [`GET ${BASE}/metrics/schema`]: json({
        sources: [{ ...spans, filter_fields: [{ field: "level", operator: "in" }] }],
      }),
      [`POST ${BASE}/metrics/query`]: json(EMPTY_METRICS),
    });
    renderPage(<DashboardEditor />);
    expect(await screen.findByText("Stat of Spans")).toBeTruthy();
    const opened = await agent.act({ steps: [{ ref: "Configure Spans", action: "press" }] });
    expect(opened.ok, opened.error?.message).toBe(true);
    expect(
      await screen.findByText(/combines conditions in a way the sheet can't show/),
    ).toBeTruthy();
    await waitFor(() => expect(lastQuery()).toMatchObject({ filter: either }));
    const removed = await agent.act({ steps: [{ ref: "Remove the filter", action: "press" }] });
    expect(removed.ok, removed.error?.message).toBe(true);
    await waitFor(() => expect(lastQuery()).not.toHaveProperty("filter"));
    expect(screen.getByText("Filter by")).toBeTruthy();
  });
});

describe("the MCP starter", () => {
  test("reads the server side, and its error rate reads as a percentage", async () => {
    const mcpSchema: MetricsSchema = {
      sources: [
        ...SCHEMA.sources,
        {
          ...SCHEMA.sources[0],
          source: "mcp_ops",
          measures: [
            {
              id: "mcp.error_rate",
              label: "MCP error rate",
              unit: "ppm",
              aggregations: ["rate"],
              additive: false,
              tier: "rollup",
            },
          ],
          dimensions: [{ id: "server_name", type: "string", high_cardinality: false }],
        },
      ],
    };
    const starter: Dashboard = {
      ...DASHBOARD,
      id: "mcp",
      name: "MCP servers",
      builtin_key: "mcp",
      widgets: [
        {
          id: "error-rate",
          query: {
            source: "mcp_ops",
            measures: [{ measure: "mcp.error_rate", aggregation: "rate" }],
            side: "server",
          },
          display: { kind: "stat", title: "Error rate" },
          layout: { x: 0, y: 0, w: 4, h: 2 },
        },
      ],
    };
    navigation.search = `project=${PROJECT_A}&id=mcp`;
    serve({
      [`GET ${BASE}/dashboards/mcp`]: json(starter),
      [`GET ${BASE}/metrics/schema`]: json(mcpSchema),
      [`POST ${BASE}/metrics/query`]: json({
        series: [
          {
            measure: "mcp.error_rate",
            aggregation: "rate",
            unit: "ppm",
            key: [],
            other: false,
            points: [{ bucket: "2026-11-18T09:00:00.000000Z", value: 50_000 }],
          },
        ],
        granularity: "hour",
        truncated: false,
        source_tier: "rollup",
        as_of: "2026-11-18T10:00:00.000000Z",
        partial_last_bucket: false,
        saturated: false,
        meta: { as_of: "2026-11-18T10:00:00.000000Z", partial: false, consistency: "strong" },
      }),
    });
    renderPage(<DashboardView />);
    expect(await screen.findByText("5%")).toBeTruthy();
    const [post] = requests(`POST ${BASE}/metrics/query`);
    expect(JSON.parse(String(post?.body))).toMatchObject({
      source: "mcp_ops",
      side: "server",
      measures: [{ measure: "mcp.error_rate", aggregation: "rate" }],
    });
  });
});
