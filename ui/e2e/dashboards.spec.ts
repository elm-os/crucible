import type { Page, Request, Route } from "@playwright/test";
import type { Dashboard } from "../src/api/generated/Dashboard";
import type { DashboardSummary } from "../src/api/generated/DashboardSummary";
import type { MetricsSchema } from "../src/api/generated/MetricsSchema";
import type { Widget } from "../src/api/generated/Widget";
import {
  type Api,
  AS_OF,
  CSRF,
  capture,
  expect,
  expectAccessible,
  expectNoPageScrollX,
  json,
  PROJECT,
  PROJECT_B,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const ID = "0193a1b2-0000-7000-8000-0000000d0001";
const LIST_PATH = new RegExp(`/projects/${PROJECT}/dashboards$`);
const ONE_PATH = new RegExp(`/projects/${PROJECT}/dashboards/${ID}$`);
const OVERVIEW_PATH = new RegExp(`/projects/${PROJECT}/dashboards/overview$`);
const DUPLICATE_PATH = new RegExp(`/projects/${PROJECT}/dashboards/overview/duplicate$`);
const SCHEMA_PATH = new RegExp(`/projects/${PROJECT}/metrics/schema$`);
const QUERY_PATH = new RegExp(`/projects/${PROJECT}/metrics/query$`);

const LIST_URL = `/dashboards/?project=${PROJECT}`;
const VIEW_URL = `/dashboard/?project=${PROJECT}&id=${ID}`;
const EDIT_URL = `/dashboard/edit/?project=${PROJECT}&id=${ID}`;

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
          id: "error_count",
          label: "Errors",
          unit: "count",
          aggregations: ["sum"],
          additive: true,
          tier: "rollup",
        },
        {
          id: "total_cost",
          label: "Total cost",
          unit: "usd",
          aggregations: ["sum"],
          additive: true,
          tier: "rollup",
        },
        {
          id: "duration",
          label: "Duration",
          unit: "us",
          aggregations: ["p50", "p90", "p95", "p99"],
          additive: false,
          tier: "rollup",
        },
      ],
      dimensions: [
        { id: "model", type: "string", high_cardinality: false },
        { id: "environment", type: "string", high_cardinality: false },
      ],
      filter_fields: [],
      granularities: ["minute", "five_minutes", "fifteen_minutes", "hour", "day", "week"],
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

function widget(id: string, fields: Partial<Widget> & Pick<Widget, "display" | "layout">): Widget {
  return { id, ...fields };
}

const WIDGETS: Widget[] = [
  widget("w-spans", {
    query: { source: "spans", measures: [{ measure: "count", aggregation: "sum" }] },
    display: { kind: "stat", title: "Spans" },
    layout: { x: 0, y: 0, w: 3, h: 2 },
  }),
  widget("w-notes", {
    display: { kind: "markdown", title: "Notes", text: "Watch **checkout** after a release." },
    layout: { x: 3, y: 0, w: 3, h: 2 },
  }),
  widget("w-duration", {
    query: {
      source: "spans",
      measures: [
        { measure: "duration", aggregation: "p50" },
        { measure: "duration", aggregation: "p95" },
      ],
    },
    display: { kind: "timeseries", title: "Duration", chart: "line" },
    layout: { x: 0, y: 2, w: 6, h: 3 },
  }),
  widget("w-cost", {
    query: {
      source: "spans",
      measures: [{ measure: "total_cost", aggregation: "sum" }],
      dimensions: ["model"],
      top_n: 2,
    },
    display: { kind: "bar", title: "Cost by model" },
    layout: { x: 6, y: 0, w: 6, h: 3 },
  }),
  widget("w-table", {
    query: {
      source: "spans",
      measures: [
        { measure: "count", aggregation: "sum" },
        { measure: "error_count", aggregation: "sum" },
      ],
      dimensions: ["environment"],
      top_n: 5,
    },
    display: { kind: "table", title: "By environment" },
    layout: { x: 6, y: 3, w: 6, h: 3 },
  }),
];

function dashboard(fields: Partial<Dashboard> = {}): Dashboard {
  return {
    id: ID,
    name: "Checkout health",
    description: "What the checkout agents did.",
    widgets: WIDGETS,
    version: 1,
    created_at: "2026-11-17T12:00:00.000000Z",
    updated_at: "2026-11-17T12:30:00.000000Z",
    ...fields,
  };
}

const OVERVIEW: Dashboard = {
  id: "overview",
  builtin_key: "overview",
  name: "Agent overview",
  description: "Traces, spans, errors and cost at a glance.",
  widgets: [WIDGETS[0] as Widget],
  version: 1,
  created_at: null,
  updated_at: null,
};

function summary(d: Dashboard): DashboardSummary {
  return {
    id: d.id,
    builtin_key: d.builtin_key,
    name: d.name,
    description: d.description,
    widget_count: d.widgets.length,
    version: d.version,
    updated_at: d.updated_at,
  };
}

const BUCKETS = [
  "2026-11-17T13:00:00.000000Z",
  "2026-11-17T13:30:00.000000Z",
  "2026-11-17T14:00:00.000000Z",
];
const UNIT: Record<string, string> = {
  count: "count",
  error_count: "count",
  total_cost: "usd",
  duration: "us",
};

// The answer of a metrics query, shaped by what it asked: one series per measure and key.
function answer(body: {
  measures: { measure: string; aggregation: string }[];
  dimensions?: string[];
}) {
  const keys = body.dimensions?.length ? ["prod", "staging"] : [null];
  const series = keys.flatMap((key, k) =>
    body.measures.map((m, i) => ({
      measure: m.measure,
      aggregation: m.aggregation,
      unit: UNIT[m.measure] ?? "count",
      key: key === null ? [] : [{ dimension: body.dimensions?.[0] ?? "", value: key }],
      other: false,
      points: BUCKETS.map((bucket, b) => ({
        bucket,
        value: m.measure === "total_cost" ? 0.25 * (b + 1) : (i + 1) * 100_000 + (b + 1) * 10 + k,
      })),
    })),
  );
  return {
    series,
    granularity: "hour",
    truncated: false,
    source_tier: "rollup",
    as_of: AS_OF,
    partial_last_bucket: false,
    saturated: false,
    meta: { as_of: AS_OF, partial: false, consistency: "strong" },
  };
}

function editor(api: Api): void {
  signedIn(api);
  api.on(SCHEMA_PATH, json(SCHEMA));
}

function metricsByQuery(api: Api, onRequest?: (body: Record<string, unknown>) => void): void {
  api.on(QUERY_PATH, (route: Route) => {
    const body = route.request().postDataJSON() as Parameters<typeof answer>[0];
    onRequest?.(body as Record<string, unknown>);
    return route.fulfill(json(answer(body)));
  });
}

const NOTHING = () => {};

const region = (page: Page, name: string) => page.getByRole("region", { name, exact: true });

test.describe("dashboards: list", () => {
  test("loading, then success: the project's dashboards and the starters", async ({
    page,
    api,
  }) => {
    editor(api);
    const release = api.hold(LIST_PATH, json({ data: [summary(dashboard()), summary(OVERVIEW)] }));
    await page.goto(LIST_URL);
    const list = region(page, "Dashboards");
    await expect(list.getByRole("status")).toHaveText("Loading dashboards");
    await expectAccessible(page, "list loading");
    release();
    await expect(list.getByRole("link", { name: "Checkout health" })).toBeVisible();
    await expect(list.getByText("5 widgets")).toBeVisible();
    await expect(list.getByRole("link", { name: "Agent overview" })).toBeVisible();
    await expect(list.getByRole("button", { name: "Duplicate Agent overview" })).toBeVisible();
    await expect(list.getByRole("button", { name: "New dashboard" })).toBeVisible();
    await expectAccessible(page, "list success");
    await expectNoPageScrollX(page);
  });

  test("empty: the first use offers New dashboard beside the starters", async ({ page, api }) => {
    editor(api);
    api.on(LIST_PATH, json({ data: [summary(OVERVIEW)] }));
    await page.goto(LIST_URL);
    await expect(page.getByText("No dashboards yet.")).toBeVisible();
    await expect(page.getByRole("link", { name: "Agent overview" })).toBeVisible();
    await expectAccessible(page, "list empty");
  });

  test("error: the page's own words, the reference, and a retry of the list", async ({
    page,
    api,
  }) => {
    editor(api);
    api.on(LIST_PATH, problem(503, "control_plane_unavailable", "req-list"));
    await page.goto(LIST_URL);
    const list = region(page, "Dashboards");
    await expect(list.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(list.getByText("req-list")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "list error");
    api.on(LIST_PATH, json({ data: [summary(OVERVIEW)] }));
    await list.getByRole("button", { name: "Retry dashboards" }).click();
    await expect(list.getByRole("link", { name: "Agent overview" })).toBeVisible();
  });

  test("without a control plane the starters are read, and nothing is offered to change", async ({
    page,
    api,
  }) => {
    api.on(LIST_PATH, json({ data: [summary(OVERVIEW)] }));
    await page.goto(LIST_URL);
    await expect(page.getByText("Your own dashboards need a control plane.")).toBeVisible();
    await expect(page.getByRole("link", { name: "Agent overview" })).toBeVisible();
    await expect(page.getByRole("button", { name: /New dashboard|Duplicate/ })).toHaveCount(0);
    await expectAccessible(page, "list without a control plane");
  });

  test("new dashboard: named, created with the session's CSRF token, and opened in the editor", async ({
    page,
    api,
  }) => {
    editor(api);
    const posts: Request[] = [];
    api.on(LIST_PATH, (route) => {
      if (route.request().method() !== "POST") return route.fulfill(json({ data: [] }));
      posts.push(route.request());
      return route.fulfill(json(dashboard({ widgets: [] }), 201));
    });
    api.on(ONE_PATH, json(dashboard({ widgets: [] })));
    await page.goto(LIST_URL);
    await page.getByRole("button", { name: "New dashboard" }).click();
    const dialog = page.getByRole("dialog", { name: "New dashboard" });
    await dialog.getByRole("button", { name: "Create" }).click();
    await expect(dialog.getByText("Enter a name, like Agent health.")).toBeVisible();
    await dialog.getByLabel("Name").fill("Checkout health");
    await expectAccessible(page, "new dashboard dialog");
    await dialog.getByRole("button", { name: "Create" }).click();
    await expect(page).toHaveURL(/\/dashboard\/edit\/\?project=.*&id=/);
    expect(posts[0]?.postDataJSON()).toEqual({ name: "Checkout health" });
    expect(posts[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
    await expect(page.getByText("No widgets yet.")).toBeVisible();
  });

  test("duplicate a starter: the copy opens in the editor", async ({ page, api }) => {
    editor(api);
    api.on(LIST_PATH, json({ data: [summary(OVERVIEW)] }));
    const copies = capture(
      api,
      DUPLICATE_PATH,
      json(dashboard({ name: "Agent overview (copy)" }), 201),
    );
    api.on(ONE_PATH, json(dashboard({ name: "Agent overview (copy)" })));
    await page.goto(LIST_URL);
    await page.getByRole("button", { name: "Duplicate Agent overview" }).click();
    await expect(page).toHaveURL(new RegExp(`/dashboard/edit/\\?project=${PROJECT}&id=${ID}`));
    expect(copies).toHaveLength(1);
    await expect(page.getByLabel("Name")).toHaveValue("Agent overview (copy)");
  });
});

test.describe("dashboard: view", () => {
  test("loading, then success: each widget is its own section and reads the range", async ({
    page,
    api,
  }) => {
    editor(api);
    const asked: Record<string, unknown>[] = [];
    metricsByQuery(api, (body) => asked.push(body));
    const release = api.hold(ONE_PATH, json(dashboard()));
    await page.goto(`${VIEW_URL}&range=24h`);
    await expect(page.getByRole("status").first()).toHaveText("Loading the dashboard");
    release();
    await expect(region(page, "Checkout health")).toContainText("What the checkout agents did.");
    await expect(region(page, "Spans").locator("[data-value]")).toHaveText("300.1K");
    await expect(region(page, "Spans")).toContainText("in the last 24 hours");
    await expect(region(page, "Notes").locator("strong")).toHaveText("checkout");
    const duration = region(page, "Duration");
    await expect(duration.locator("svg [data-series]")).toHaveCount(2);
    await expect(duration.getByText("Duration p95")).toBeVisible();
    const cost = region(page, "Cost by model");
    await expect(cost.getByRole("listitem")).toHaveCount(2);
    await expect(cost.getByRole("listitem").first()).toContainText("$1.50");
    await cost.getByRole("button", { name: "View as table: cost by model" }).click();
    await expect(cost.getByRole("row", { name: /prod/ })).toContainText("$1.50");
    const table = region(page, "By environment");
    await expect(table.getByRole("row", { name: /staging/ })).toContainText("300,063");
    expect(asked).toHaveLength(4);
    for (const body of asked) {
      expect(Date.parse(String(body.to)) - Date.parse(String(body.from))).toBe(24 * 3_600_000);
    }
    await expectAccessible(page, "view success");
    await expectNoPageScrollX(page);
  });

  test("one widget down: its error and retry stay inside it; empty and loading widgets hold their place", async ({
    page,
    api,
  }) => {
    editor(api);
    api.on(ONE_PATH, json(dashboard()));
    let release = NOTHING;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    api.on(QUERY_PATH, async (route) => {
      const body = route.request().postDataJSON() as Parameters<typeof answer>[0];
      const first = body.measures[0]?.measure;
      if (first === "error_count" || body.dimensions?.[0] === "environment") {
        return route.fulfill(problem(503, "telemetry_store_unavailable", "req-widget"));
      }
      if (first === "total_cost") {
        return route.fulfill(json({ ...answer(body), series: [] }));
      }
      if (first === "duration") {
        await held;
        return route.fulfill(json(answer(body))).catch(() => {});
      }
      return route.fulfill(json(answer(body)));
    });
    await page.goto(VIEW_URL);
    await expect(region(page, "Duration").getByRole("status")).toHaveText("Loading duration");
    const table = region(page, "By environment");
    await expect(table.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(table.getByText("req-widget")).toBeVisible();
    await expect(table.getByRole("link", { name: "Edit widget" })).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    const cost = region(page, "Cost by model");
    await expect(cost.getByText("No data in the last 24 hours.")).toBeVisible();
    await expect(region(page, "Spans").locator("[data-value]")).toHaveText("300.1K");
    await expectAccessible(page, "view with one widget down");
    release();
    await expect(region(page, "Duration").locator("svg [data-series]")).toHaveCount(2);
    metricsByQuery(api);
    await table.getByRole("button", { name: "Retry by environment" }).click();
    await expect(table.getByRole("row", { name: /prod/ })).toBeVisible();
    await cost.getByRole("button", { name: "Show last 30 days: cost by model" }).click();
    await expect(page).toHaveURL(/range=30d/);
    await expect(cost.getByRole("listitem")).toHaveCount(2);
  });

  test("the time range above reads every widget again", async ({ page, api }) => {
    editor(api);
    api.on(ONE_PATH, json(dashboard()));
    const asked: Record<string, unknown>[] = [];
    metricsByQuery(api, (body) => asked.push(body));
    await page.goto(`${VIEW_URL}&range=24h`);
    await expect(region(page, "Spans").locator("[data-value]")).toHaveText("300.1K");
    const before = asked.length;
    await page.getByRole("radio", { name: "7d" }).click();
    await expect(page).toHaveURL(/range=7d/);
    await expect.poll(() => asked.length).toBe(before + 4);
    for (const body of asked.slice(before)) {
      expect(Date.parse(String(body.to)) - Date.parse(String(body.from))).toBe(7 * 24 * 3_600_000);
    }
    await expect(region(page, "Spans")).toContainText("in the last 7 days");
  });

  test("a widget whose buckets don't fit the range says so, with a way to fix it", async ({
    page,
    api,
  }) => {
    editor(api);
    api.on(ONE_PATH, json(dashboard({ widgets: [WIDGETS[0] as Widget] })));
    api.on(QUERY_PATH, problem(422, "query_too_broad"));
    await page.goto(VIEW_URL);
    const spans = region(page, "Spans");
    await expect(
      spans.getByText("This widget's buckets don't fit the selected range."),
    ).toBeVisible();
    await expect(spans.getByRole("link", { name: "Edit widget" })).toHaveAttribute(
      "href",
      new RegExp(`/dashboard/edit/\\?project=${PROJECT}&id=${ID}&widget=w-spans`),
    );
  });

  test("not found, a built-in dashboard, and a dashboard without widgets", async ({
    page,
    api,
  }) => {
    editor(api);
    api.on(ONE_PATH, problem(404, "not_found"));
    await page.goto(VIEW_URL);
    await expect(page.getByText("This isn't in this project.")).toBeVisible();
    api.on(OVERVIEW_PATH, json(OVERVIEW));
    metricsByQuery(api);
    await page.goto(`/dashboard/?project=${PROJECT}&id=overview`);
    const header = region(page, "Agent overview");
    await expect(
      header.getByText("Built in and read-only. Duplicate it to change it."),
    ).toBeVisible();
    await expect(header.getByRole("button", { name: "Duplicate Agent overview" })).toBeVisible();
    await expect(header.getByRole("link", { name: /^Edit/ })).toHaveCount(0);
    await expect(header.getByRole("button", { name: "Delete" })).toHaveCount(0);
    api.on(ONE_PATH, json(dashboard({ widgets: [] })));
    await page.goto(VIEW_URL);
    await expect(page.getByText("This dashboard has no widgets yet.")).toBeVisible();
    await expectAccessible(page, "view without widgets");
  });

  test("delete asks first, sends the version, and returns to the list", async ({ page, api }) => {
    editor(api);
    const deletes: Request[] = [];
    api.on(ONE_PATH, (route) => {
      if (route.request().method() === "DELETE") {
        deletes.push(route.request());
        return route.fulfill({ status: 204 });
      }
      return route.fulfill(json(dashboard({ version: 4 })));
    });
    metricsByQuery(api);
    api.on(LIST_PATH, json({ data: [] }));
    await page.goto(VIEW_URL);
    await region(page, "Checkout health").getByRole("button", { name: "Delete" }).click();
    const confirm = page.getByRole("alertdialog", { name: "Delete Checkout health?" });
    await expectAccessible(page, "delete confirmation");
    await confirm.getByRole("button", { name: "Cancel" }).click();
    await expect(confirm).toBeHidden();
    expect(deletes).toHaveLength(0);
    await region(page, "Checkout health").getByRole("button", { name: "Delete" }).click();
    await page.getByRole("button", { name: "Delete dashboard" }).click();
    await expect(page).toHaveURL(/\/dashboards\/\?project=/);
    expect(deletes[0]?.headers()["if-match"]).toBe('"4"');
  });
});

test.describe("dashboard: editor", () => {
  test("add a widget from the schema's pickers, preview it, and save it with the version", async ({
    page,
    api,
  }) => {
    editor(api);
    api.on(ONE_PATH, json(dashboard({ widgets: [WIDGETS[0] as Widget] })));
    const previews: Record<string, unknown>[] = [];
    metricsByQuery(api, (body) => previews.push(body));
    const saved = dashboard({ version: 2, widgets: [WIDGETS[0] as Widget] });
    const patches: Request[] = [];
    api.on(ONE_PATH, (route) => {
      if (route.request().method() === "PATCH") {
        patches.push(route.request());
        return route.fulfill(json(saved));
      }
      return route.fulfill(json(dashboard({ widgets: [WIDGETS[0] as Widget] })));
    });
    await page.goto(EDIT_URL);
    await expect(page.getByRole("button", { name: "Move Spans" })).toBeVisible();
    await expectAccessible(page, "editor");
    await page.getByRole("button", { name: "Add widget" }).click();
    const drawer = page.getByRole("dialog", { name: "Add a widget" });
    await expect(drawer.getByText("Give the widget a title.")).toBeVisible();
    await drawer.getByLabel("Title").fill("Latency");
    await drawer.getByRole("combobox", { name: "Measure" }).click();
    await page.getByRole("option", { name: "Duration p95" }).click();
    await drawer.getByRole("button", { name: "Add measure" }).click();
    await expect(
      drawer.getByText("A chart has one axis: choose measures of one unit."),
    ).toBeVisible();
    await drawer.getByRole("combobox", { name: "Measure 2" }).click();
    await page.getByRole("option", { name: "Duration p50" }).click();
    await expect(drawer.locator("svg [data-series]")).toHaveCount(2);
    await expectAccessible(page, "widget form with preview");
    expect(previews.at(-1)).toMatchObject({
      source: "spans",
      measures: [
        { measure: "duration", aggregation: "p95" },
        { measure: "duration", aggregation: "p50" },
      ],
    });
    await drawer.getByRole("button", { name: "Apply" }).click();
    await expect(drawer).toBeHidden();
    await expect(page.getByRole("button", { name: "Move Latency" })).toBeVisible();
    await expect(page.getByText("Unsaved changes.")).toBeVisible();
    // The sheet takes the page's clicks until it has finished closing.
    await expect(page.locator("[inert]")).toHaveCount(0);
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText("Saved.")).toBeVisible();
    const patch = patches[0];
    expect(patch?.headers()["if-match"]).toBe('"1"');
    expect(patch?.headers()["x-crucible-csrf"]).toBe(CSRF);
    const body = patch?.postDataJSON() as { widgets: { id?: string; layout: unknown }[] };
    expect(body.widgets[0]).toMatchObject({ id: "w-spans" });
    expect(body.widgets[1]?.id).toBeUndefined();
    expect(body.widgets[1]?.layout).toEqual({ x: 3, y: 0, w: 6, h: 3 });
  });

  test("move and resize by keyboard; a move onto another widget is refused aloud", async ({
    page,
    api,
  }) => {
    editor(api);
    const two = [
      WIDGETS[0] as Widget,
      widget("w-errors", {
        query: { source: "spans", measures: [{ measure: "error_count", aggregation: "sum" }] },
        display: { kind: "stat", title: "Errors" },
        layout: { x: 4, y: 0, w: 3, h: 2 },
      }),
    ];
    const patches: unknown[] = [];
    api.on(ONE_PATH, (route) => {
      if (route.request().method() === "PATCH") {
        patches.push(route.request().postDataJSON());
        return route.fulfill(json(dashboard({ version: 2, widgets: two })));
      }
      return route.fulfill(json(dashboard({ widgets: two })));
    });
    await page.goto(EDIT_URL);
    const move = page.getByRole("button", { name: "Move Spans" });
    await move.focus();
    // Spans spans columns 0 to 2 and Errors starts at column 4: one step right touches it, a second overlaps.
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    await expect(page.getByText(/Spans can't move there/)).toBeAttached();
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("ArrowRight");
    await page.getByRole("button", { name: "Resize Spans" }).focus();
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowDown");
    await expectAccessible(page, "editor after keyboard moves");
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText("Saved.")).toBeVisible();
    const sent = patches[0] as { widgets: { id: string; layout: unknown }[] };
    expect(sent.widgets.find((w) => w.id === "w-spans")?.layout).toEqual({
      x: 2,
      y: 2,
      w: 4,
      h: 3,
    });
  });

  test("move and resize by pointer; a drop onto another widget snaps back", async ({
    page,
    api,
  }) => {
    editor(api);
    const two = [
      WIDGETS[0] as Widget,
      widget("w-errors", {
        query: { source: "spans", measures: [{ measure: "error_count", aggregation: "sum" }] },
        display: { kind: "stat", title: "Errors" },
        layout: { x: 4, y: 0, w: 3, h: 2 },
      }),
    ];
    const patches: unknown[] = [];
    api.on(ONE_PATH, (route) => {
      if (route.request().method() === "PATCH") {
        patches.push(route.request().postDataJSON());
        return route.fulfill(json(dashboard({ version: 2, widgets: two })));
      }
      return route.fulfill(json(dashboard({ widgets: two })));
    });
    await page.goto(EDIT_URL);
    const box = page.locator('[data-widget="w-spans"]');
    await expect(box).toBeVisible();
    const start = await box.boundingBox();
    if (!start) throw new Error("the box has no place");
    // A box of 3 columns is 3 pitches wide less the 12 px gap.
    const pitch = (start.width + 12) / 3;
    const drag = async (name: string, dx: number, dy: number) => {
      const handle = await page.getByRole("button", { name }).boundingBox();
      if (!handle) throw new Error(`no ${name}`);
      const x = handle.x + handle.width / 2;
      const y = handle.y + handle.height / 2;
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.move(x + dx / 2, y + dy / 2, { steps: 5 });
      await page.mouse.move(x + dx, y + dy, { steps: 5 });
      await page.mouse.up();
    };

    // Onto Errors: refused, and the box is back where it was.
    await drag("Move Spans", 3 * pitch, 0);
    await expect(page.getByText(/Spans can't move there/)).toBeAttached();
    await expect.poll(async () => (await box.boundingBox())?.x).toBeCloseTo(start.x, 0);
    expect((await box.boundingBox())?.width).toBeCloseTo(start.width, 0);
    await expect(page.getByText("No changes.")).toBeVisible();

    // Two rows down, then one cell wider and taller from the corner.
    await drag("Move Spans", 0, 2 * pitch);
    await expect.poll(async () => (await box.boundingBox())?.y).toBeCloseTo(start.y + 2 * pitch, 0);
    await drag("Resize Spans", pitch, pitch);
    await expect
      .poll(async () => (await box.boundingBox())?.width)
      .toBeCloseTo(start.width + pitch, 0);
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText("Saved.")).toBeVisible();
    const sent = patches[0] as { widgets: { id: string; layout: unknown }[] };
    expect(sent.widgets.find((w) => w.id === "w-spans")?.layout).toEqual({
      x: 0,
      y: 2,
      w: 4,
      h: 3,
    });
  });

  test("the sheet says when the measures are loading or failed, and retries them", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(ONE_PATH, json(dashboard({ widgets: [] })));
    const release = api.hold(SCHEMA_PATH, problem(503, "control_plane_unavailable", "req-schema"));
    await page.goto(EDIT_URL);
    await page.getByRole("button", { name: "Add widget" }).click();
    const drawer = page.getByRole("dialog", { name: "Add a widget" });
    await expect(drawer.getByRole("status")).toHaveText("Loading the measures");
    await expect(drawer.getByText("The measures haven't loaded yet.")).toBeVisible();
    release();
    await expect(drawer.getByText("The measures couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(drawer.getByText("req-schema")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "sheet without the measures");
    api.on(SCHEMA_PATH, json(SCHEMA));
    metricsByQuery(api);
    await drawer.getByRole("button", { name: "Retry the measures" }).click();
    await expect(drawer.getByRole("combobox", { name: "Measure" })).toBeVisible();
    await expect(drawer.getByText("The measures haven't loaded yet.")).toHaveCount(0);
  });

  test("a save after someone else's change: load theirs, or save yours over it", async ({
    page,
    api,
  }) => {
    editor(api);
    let version = 1;
    const patches: { version: string | undefined; body: unknown }[] = [];
    api.on(ONE_PATH, (route) => {
      const request = route.request();
      if (request.method() === "PATCH") {
        patches.push({ version: request.headers()["if-match"], body: request.postDataJSON() });
        if (request.headers()["if-match"] !== `"${version}"`) {
          return route.fulfill(problem(412, "precondition_failed"));
        }
        return route.fulfill(json(dashboard({ version: version + 1, name: "Mine" })));
      }
      return route.fulfill(
        json(dashboard({ version, name: version === 1 ? "Checkout health" : "Theirs" })),
      );
    });
    metricsByQuery(api);
    await page.goto(EDIT_URL);
    await page.getByLabel("Name").fill("Mine");
    version = 5;
    await page.getByRole("button", { name: "Save" }).click();
    await expect(
      page.getByText("Someone changed this dashboard since you opened it."),
    ).toBeVisible();
    await expectAccessible(page, "editor conflict");
    await page.getByRole("button", { name: "Save mine over theirs" }).click();
    await expect(page.getByText("Saved.")).toBeVisible();
    expect(patches.map((p) => p.version)).toEqual(['"1"', '"5"']);
    expect(patches[1]?.body).toMatchObject({ name: "Mine" });

    version = 7;
    await page.getByLabel("Name").fill("Mine again");
    await page.getByRole("button", { name: "Save" }).click();
    await expect(
      page.getByText("Someone changed this dashboard since you opened it."),
    ).toBeVisible();
    await page.getByRole("button", { name: "Load their version" }).click();
    await expect(page.getByLabel("Name")).toHaveValue("Theirs");
    await expect(page.getByText("No changes.")).toBeVisible();
  });

  test("delete a widget, then save without it", async ({ page, api }) => {
    editor(api);
    const patches: unknown[] = [];
    api.on(ONE_PATH, (route) => {
      if (route.request().method() === "PATCH") {
        patches.push(route.request().postDataJSON());
        return route.fulfill(json(dashboard({ version: 2, widgets: WIDGETS.slice(1) })));
      }
      return route.fulfill(json(dashboard()));
    });
    await page.goto(EDIT_URL);
    await page.getByRole("button", { name: "Delete Spans" }).click();
    await expect(page.getByRole("button", { name: "Move Spans" })).toHaveCount(0);
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText("Saved.")).toBeVisible();
    const sent = patches[0] as { widgets: { id: string }[] };
    expect(sent.widgets.map((w) => w.id)).not.toContain("w-spans");
    expect(sent.widgets).toHaveLength(4);
  });

  test("a viewer is told why and the editor asks the server for nothing", async ({ page, api }) => {
    signedIn(api, sessionInfo());
    await page.goto(`/dashboard/edit/?project=${PROJECT_B}&id=${ID}`);
    await expect(page.getByText("Your role can't edit dashboards.")).toBeVisible();
    expect(api.calls.filter((call) => call.includes("/dashboards"))).toEqual([]);
    await expectAccessible(page, "editor for a viewer");
  });

  test("a built-in dashboard is not edited in place", async ({ page, api }) => {
    editor(api);
    api.on(OVERVIEW_PATH, json(OVERVIEW));
    await page.goto(`/dashboard/edit/?project=${PROJECT}&id=overview`);
    await expect(page.getByText("Built-in dashboards are read-only.")).toBeVisible();
    await expect(page.getByRole("button", { name: "Duplicate Agent overview" })).toBeVisible();
  });
});
