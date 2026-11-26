import {
  BOUNDS_PATH,
  dataBounds,
  expect,
  expectAccessible,
  json,
  METRICS_PATH,
  measuresOf,
  metrics,
  PROJECT,
  problem,
  SERVER_TEXT,
  test,
} from "./fixtures";

const TILES = ["Traces", "Error rate", "Latency", "Cost", "Tokens"];
const CHARTS = ["Traces over time", "Spans and errors", "Latency percentiles"];

// Every tile and chart asks for its own measures; this answers each by what it asks for.
function answer(measures: string[]) {
  const values: Record<string, (number | null)[]> = {
    traces: [3, 4],
    count: [9, 11],
    error_count: [1, 1],
    total_cost: [0.25, 0.5],
    total_tokens: [1_000, 2_000],
  };
  return json(
    metrics(
      measures.map((measure, i) =>
        measure === "duration"
          ? {
              measure,
              aggregation: i === 0 ? "p50" : "p95",
              values: [1_500, i === 0 ? 2_500 : 9_000],
            }
          : { measure, values: values[measure] ?? [0, 0] },
      ),
    ),
  );
}

const region = (name: string) => ({ name, exact: true });

// A project with traces and MCP operations, unless a test answers its own data bounds.
test.beforeEach(({ api }) => {
  api.on(BOUNDS_PATH, json(dataBounds({ spans: true, server: true })));
});

test.describe("home: tiles and charts", () => {
  test("loading: every tile and chart holds its place and is announced", async ({ page, api }) => {
    const release = api.hold(METRICS_PATH, json(metrics([])));
    await page.goto(`/home/?project=${PROJECT}`);
    for (const name of [...TILES, ...CHARTS]) {
      await expect(page.getByRole("region", region(name))).toHaveAttribute("aria-busy", "true");
    }
    await expect(
      page.getByRole("status").filter({ hasText: "Loading traces" }).first(),
    ).toBeAttached();
    await expectAccessible(page, "home loading");
    release();
  });

  test("success: tiles show their headlines and charts their series, with a table view", async ({
    page,
    api,
  }) => {
    api.on(METRICS_PATH, (route) => route.fulfill(answer(measuresOf(route))));
    await page.goto(`/home/?project=${PROJECT}`);
    await expect(page.getByRole("region", region("Traces"))).toContainText("7");
    await expect(page.getByRole("region", region("Error rate"))).toContainText("10%");
    await expect(page.getByRole("region", region("Cost"))).toContainText("$0.75");
    await expect(page.getByRole("region", region("Tokens"))).toContainText("3,000");
    await expect(page.getByRole("region", region("Latency"))).toContainText("3 ms p50");
    const latency = page.getByRole("region", region("Latency percentiles"));
    await expect(latency.getByRole("listitem")).toHaveText(["p50", "p95"]);
    // The bucket slider steps the crosshair from the keyboard.
    await latency.getByRole("slider").focus();
    await page.keyboard.press("Home");
    await expect(latency.locator("[data-crosshair]")).toBeAttached();
    await latency.getByRole("button", { name: "View as table" }).click();
    await expect(latency.getByRole("table")).toContainText("9 ms");
    await expectAccessible(page, "home success");
  });

  test("error: one failing tile shows the mapped copy and its retry refetches only it", async ({
    page,
    api,
  }) => {
    let failing = true;
    api.on(METRICS_PATH, (route) => {
      const measures = measuresOf(route);
      if (measures[0] === "total_cost" && failing) {
        return route.fulfill(problem(500, "internal_error", "0193a5b2-cost"));
      }
      return route.fulfill(answer(measures));
    });
    await page.goto(`/home/?project=${PROJECT}`);
    const cost = page.getByRole("region", region("Cost"));
    await expect(cost).toContainText("This section couldn't load.");
    await expect(cost).toContainText("0193a5b2-cost");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(page.getByRole("region", region("Tokens"))).toContainText("3,000");
    await expectAccessible(page, "home with a failing tile");
    failing = false;
    const before = api.count(METRICS_PATH);
    await cost.getByRole("button", { name: "Retry cost" }).click();
    await expect(cost).toContainText("$0.75");
    expect(api.count(METRICS_PATH)).toBe(before + 1);
  });

  test("empty: no data offers the 30-day range; an incomplete answer offers to check again", async ({
    page,
    api,
  }) => {
    api.on(METRICS_PATH, (route) => {
      const measures = measuresOf(route);
      const body = metrics(measures.map((measure) => ({ measure, values: [0, 0] })));
      if (measures[0] === "traces") body.meta.consistency = "lagging";
      return route.fulfill(json(body));
    });
    await page.goto(`/home/?project=${PROJECT}`);
    const cost = page.getByRole("region", region("Cost"));
    await expect(cost).toContainText("No data in the last 24 hours.");
    await expect(page.getByRole("region", region("Traces"))).toContainText("No data to show yet.");
    await expectAccessible(page, "home empty");
    await cost.getByRole("button", { name: "Show last 30 days" }).click();
    await expect(cost).toContainText("No data in the last 30 days.");
  });

  test("static: the set-up steps render whatever the API answers", async ({ page, api }) => {
    api.on(METRICS_PATH, problem(503, "telemetry_store_unavailable"));
    await page.goto(`/home/?project=${PROJECT}`);
    const setup = page.getByRole("region", region("Set up"));
    await expect(setup.getByRole("listitem")).toHaveCount(4);
    await expect(setup).toContainText("Send your first trace");
  });
});
