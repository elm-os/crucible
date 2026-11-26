import type { TraceDetail } from "../src/api/generated/TraceDetail";
import {
  AGENT_SPANS,
  AS_OF,
  CHILDREN_PATH,
  expect,
  expectAccessible,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  SIGN_IN_CHECK,
  span,
  spanPage,
  TRACE,
  TRACE_PATH,
  test,
  traceDetail,
  traceSummary,
  traceUrl,
} from "./fixtures";

const ROOT_ROWS = [
  "support agent",
  "plan reply",
  "search_docs",
  "search_docs",
  "review agent",
  "draft review",
  "policy check",
  "send_reply",
];

test.describe("trace detail: summary", () => {
  test("loading: a skeleton holds the header after 300 ms", async ({ page, api }) => {
    const release = api.hold(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    await expect(page.locator("[data-loading=summary]")).toBeVisible();
    await expectAccessible(page, "summary loading");
    release();
    await expect(page.getByText("support agent", { exact: true }).first()).toBeVisible();
  });

  test("success: name, status, start, duration, cost, tokens and span count", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const facts = page.locator("dl").first();
    await expect(page.getByText("1 error")).toBeVisible();
    await expect(facts).toContainText("Duration4.2 s");
    await expect(facts).toContainText("Cost$0.004");
    await expect(facts).toContainText("Tokens4,100");
    await expect(facts).toContainText("Spans8");
    await expect(facts).toContainText("Started");
    await expect(page.getByText(TRACE)).toBeVisible();
    await expectAccessible(page, "summary success");
  });

  test("success: the trace name titles the page, with the root span, user, session, environment and release", async ({
    page,
    api,
  }) => {
    const summary = traceSummary(AGENT_SPANS);
    api.on(
      TRACE_PATH,
      json(
        traceDetail({
          summary: summary && {
            ...summary,
            trace_name: "ticket 4411",
            user_id: "user-42",
            session_id: "sess-migrate-1",
            release: "2.7.0",
          },
        }),
      ),
    );
    await page.goto(traceUrl());
    await expect(page.getByText("ticket 4411", { exact: true })).toBeVisible();
    await expect(page.locator("[data-head-span]")).toHaveText("Root span support agent");
    const facts = page.locator("dl").first();
    await expect(facts.getByRole("link", { name: "user-42" })).toHaveAttribute(
      "href",
      `/user/?project=${PROJECT}&user=user-42`,
    );
    await expect(facts.getByRole("link", { name: "sess-migrate-1" })).toHaveAttribute(
      "href",
      `/session/?project=${PROJECT}&session=sess-migrate-1`,
    );
    await expect(facts).toContainText("Environmentproduction");
    await expect(facts).toContainText("Release2.7.0");
    await expectAccessible(page, "summary with trace name");
  });

  test("success: a partial response says spans may be missing, a lagging one that the newest may", async ({
    page,
    api,
  }) => {
    api.on(
      TRACE_PATH,
      json(
        traceDetail({
          meta: {
            as_of: AS_OF,
            partial: true,
            partial_reason: "store_unavailable",
            consistency: "strong",
          },
        }),
      ),
    );
    await page.goto(traceUrl());
    await expect(
      page.getByText("Some spans may be missing. Part of the data couldn't be read."),
    ).toBeVisible();
    await expect(page.getByText("store_unavailable")).toHaveCount(0);

    api.on(
      TRACE_PATH,
      json(traceDetail({ meta: { as_of: AS_OF, partial: false, consistency: "lagging" } })),
    );
    await page.reload();
    await expect(page.getByText("The newest spans may not be shown yet.")).toBeVisible();
  });

  test("error: the header keeps the trace id and the spans section explains", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, problem(503, "telemetry_store_unavailable"));
    await page.goto(traceUrl());
    await expect(page.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("heading", { name: "Trace", level: 1 })).toBeVisible();
    await expect(page.getByText(TRACE)).toBeVisible();
    await expect(page.getByText("Duration", { exact: true })).toHaveCount(0);
    await expectAccessible(page, "summary error");
  });
});

test.describe("trace detail: spans", () => {
  test("loading: row skeletons after 300 ms, announced to screen readers", async ({
    page,
    api,
  }) => {
    const release = api.hold(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const section = page.getByRole("region", { name: "Spans" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    await expect(section.getByRole("status")).toHaveText("Loading spans");
    await expectAccessible(page, "spans loading");
    release();
    await expect(page.getByRole("treegrid", { name: "Spans" })).toBeVisible();
    await expect(section).not.toHaveAttribute("aria-busy", "true");
  });

  test("success: the span tree in start order, with durations and the error marker", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const grid = page.getByRole("treegrid", { name: "Spans" });
    await expect(grid).toBeVisible();
    const headers = grid.getByRole("rowheader");
    await expect(headers).toHaveCount(ROOT_ROWS.length);
    const names = await headers.allInnerTexts();
    ROOT_ROWS.forEach((name, index) => expect(names[index]).toContain(name));
    await expect(grid.getByRole("row", { name: /send_reply/ })).toContainText("Error");
    await expect(grid.getByRole("row", { name: /search_docs/ }).nth(1)).toContainText("<1 ms");
    await expect(grid.getByRole("row", { name: /review agent/ })).toHaveAttribute(
      "aria-expanded",
      "true",
    );
    await expectAccessible(page, "spans success");
    await page.screenshot({ path: test.info().outputPath("trace-desktop.png"), fullPage: true });
  });

  test("error: a store outage is an in-pane error whose retry refetches only the trace", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, problem(503, "telemetry_store_unavailable", "req-503"));
    await page.goto(traceUrl());
    const section = page.getByRole("region", { name: "Spans" });
    await expect(section.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(section.getByText("req-503")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    // The client retries a 5xx once by itself before showing the error.
    expect(api.count(TRACE_PATH)).toBe(2);
    await expectAccessible(page, "spans error");

    api.on(TRACE_PATH, json(traceDetail()));
    await section.getByRole("button", { name: "Retry spans" }).click();
    await expect(page.getByRole("treegrid", { name: "Spans" })).toBeVisible();
    expect(api.count(TRACE_PATH)).toBe(3);
    expect(api.count(CHILDREN_PATH)).toBe(0);
  });

  test("error: a trace that isn't in the project uses the not-found copy, without a retry", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, problem(404, "not_found", "req-404"));
    await page.goto(traceUrl());
    await expect(page.getByText("This isn't in this project.")).toBeVisible();
    await expect(page.getByRole("button", { name: "Retry spans" })).toHaveCount(0);
    expect(api.count(TRACE_PATH)).toBe(1);
    await expectAccessible(page, "spans not found");
  });

  test("empty: a trace with no spans yet offers to check again", async ({ page, api }) => {
    api.on(
      TRACE_PATH,
      json(
        traceDetail({
          spans: [],
          span_count: 0,
          meta: { as_of: AS_OF, partial: false, consistency: "lagging" },
        }),
      ),
    );
    await page.goto(traceUrl());
    await expect(page.getByText("This trace has no spans to show yet.")).toBeVisible();
    await expect(page.getByText("This isn't in this project.")).toHaveCount(0);
    await expectAccessible(page, "spans empty");

    api.on(TRACE_PATH, json(traceDetail()));
    await page.getByRole("button", { name: "Check again" }).click();
    await expect(page.getByRole("treegrid", { name: "Spans" })).toBeVisible();
  });

  test("keyboard: one tab stop; arrows move, Left and Right collapse and expand, Enter selects", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const grid = page.getByRole("treegrid", { name: "Spans" });
    await expect(grid.locator('[role="row"][tabindex="0"]')).toHaveCount(1);

    const root = grid.getByRole("row", { name: /support agent/ });
    await root.focus();
    await page.keyboard.press("ArrowDown");
    await expect(grid.getByRole("row", { name: /plan reply/ })).toBeFocused();

    const reviewer = grid.getByRole("row", { name: /review agent/ });
    await reviewer.focus();
    await page.keyboard.press("ArrowLeft");
    await expect(reviewer).toHaveAttribute("aria-expanded", "false");
    await expect(grid.getByRole("row", { name: /draft review/ })).toHaveCount(0);
    await page.keyboard.press("ArrowRight");
    await expect(reviewer).toHaveAttribute("aria-expanded", "true");
    await page.keyboard.press("ArrowDown");
    const draft = grid.getByRole("row", { name: /draft review/ });
    await expect(draft).toBeFocused();

    await page.keyboard.press("Enter");
    await expect(draft).toHaveAttribute("aria-selected", "true");
    await expect(page).toHaveURL(/span=a000000000000006/);
    const panel = page.getByRole("region", { name: "Span", exact: true });
    await expect(panel).toContainText("draft review");
    await expect(panel).toContainText("gpt-4.1-mini");
    await expect(panel).toContainText("2,400 in, 320 out, 2,720 total");
  });
});

test.describe("trace detail: span panel", () => {
  test("loading: a skeleton while the trace loads", async ({ page, api }) => {
    const release = api.hold(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    await expect(page.getByRole("region", { name: "Span", exact: true })).toHaveAttribute(
      "aria-busy",
      "true",
    );
    release();
    await expect(page.getByText("Select a span.")).toBeVisible();
  });

  test("empty: with no span selected, it asks for one", async ({ page, api }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    await expect(page.getByRole("region", { name: "Span", exact: true })).toContainText(
      "Select a span.",
    );
    await expectAccessible(page, "span panel empty");
  });

  test("success: a deep link selects its span, brings it into view and fills the panel", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl({ span: "A000000000000008" }));
    const row = page.getByRole("row", { name: /send_reply/ });
    await expect(row).toHaveAttribute("aria-selected", "true");
    await expect(row).toBeInViewport();
    const panel = page.getByRole("region", { name: "Span", exact: true });
    await expect(panel).toContainText("send_reply");
    await expect(panel).toContainText("Error");
    await expect(panel).toContainText("a000000000000001");
    await expect(panel.getByRole("tab", { name: "Overview" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expectAccessible(page, "span panel success");
  });

  test("empty: a span that isn't in the trace, and an invalid span link, each say so", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl({ span: "ffffffffffffffff" }));
    await expect(page.getByText("This span isn't in this trace.")).toBeVisible();
    await page.goto(traceUrl({ span: "not-a-span" }));
    await expect(page.getByText("This span link isn't valid.")).toBeVisible();
  });

  test("error: the panel stands aside while the trace has failed", async ({ page, api }) => {
    api.on(TRACE_PATH, problem(404, "not_found"));
    await page.goto(traceUrl({ span: "a000000000000002" }));
    await expect(page.getByText("This isn't in this project.")).toBeVisible();
    await expect(page.getByRole("region", { name: "Span", exact: true })).toHaveCount(0);
  });
});

test.describe("trace detail: child spans of a paged trace", () => {
  const root = span("b000000000000001", null, 0, 9_000_000, {
    name: "batch agent",
    observation_type: "agent",
    child_count: 1,
  });
  const worker = span("b000000000000002", "b000000000000001", 10_000, 3_000_000, {
    name: "worker one",
    observation_type: "agent",
    child_count: 3,
  });
  const fetches = [0, 1, 2].map((index) =>
    span(`c00000000000000${index}`, "b000000000000002", 20_000 + index * 100_000, 90_000, {
      name: `fetch page ${index}`,
      observation_type: "tool",
    }),
  );
  const paged = traceDetail({ paged: true, span_count: 12_000, spans: [root, worker] });

  test("loading then success: expanding a span fetches its first page of children", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(paged));
    const release = api.hold(CHILDREN_PATH, json(spanPage(fetches)));
    await page.goto(traceUrl());
    await expect(
      page.getByText("This trace is large, so the timeline loads its spans as you expand them."),
    ).toBeVisible();
    const row = page.getByRole("row", { name: /worker one/ });
    await row.focus();
    await page.keyboard.press("ArrowRight");
    const loading = page.getByRole("row", { name: "Loading child spans" });
    await expect(loading).toBeVisible();
    await expect(loading).toHaveAttribute("aria-busy", "true");
    await expectAccessible(page, "child spans loading");
    release();
    await expect(page.getByRole("row", { name: /fetch page 2/ })).toBeVisible();
    await expect(loading).toHaveCount(0);
    expect(api.calls).toContain(
      `/api/v1/projects/${PROJECT}/traces/${TRACE}/spans?parent_span_id=b000000000000002`,
    );
    await expectAccessible(page, "child spans success");
  });

  test("more: the next page loads after its cursor when its row is chosen", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(paged));
    api.on(CHILDREN_PATH, (route, url) =>
      route.fulfill(
        url.searchParams.get("cursor") === "page-2"
          ? json(spanPage(fetches.slice(2)))
          : json(spanPage(fetches.slice(0, 2), "page-2")),
      ),
    );
    await page.goto(traceUrl());
    await page.getByRole("row", { name: /worker one/ }).focus();
    await page.keyboard.press("ArrowRight");
    const more = page.getByRole("row", { name: "Show more child spans (1 not shown)" });
    await expect(more).toBeVisible();
    await more.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("row", { name: /fetch page 2/ })).toBeVisible();
    await expect(page.getByRole("row", { name: /Show more child spans/ })).toHaveCount(0);
    expect(api.calls.some((call) => call.includes("cursor=page-2"))).toBe(true);
    expect(page.url()).not.toContain("/more");
    // The row that loaded them is gone, so focus moves to the first span it loaded.
    await expect(page.getByRole("row", { name: /fetch page 2/ })).toBeFocused();
  });

  test("error: a failed page leaves a retry row that refetches only that page", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(paged));
    api.on(CHILDREN_PATH, problem(503, "telemetry_store_unavailable"));
    await page.goto(traceUrl());
    await page.getByRole("row", { name: /worker one/ }).focus();
    await page.keyboard.press("ArrowRight");
    const retry = page.getByRole("row", { name: /Couldn't load child spans/ });
    await expect(retry).toBeVisible({ timeout: 10_000 });
    await expectAccessible(page, "child spans error");

    api.on(CHILDREN_PATH, json(spanPage(fetches)));
    await retry.click();
    await expect(page.getByRole("row", { name: /fetch page 1/ })).toBeVisible();
    expect(api.count(TRACE_PATH)).toBe(1);
  });

  test("empty: a span whose count ran ahead shows nothing once its page comes back empty", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(paged));
    api.on(CHILDREN_PATH, json(spanPage([])));
    await page.goto(traceUrl());
    const row = page.getByRole("row", { name: /worker one/ });
    await row.focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByRole("row", { name: /child spans/ })).toHaveCount(0);
    await expect(page.getByRole("treegrid").getByRole("rowheader")).toHaveCount(2);
  });

  test("a deep link to a span that isn't loaded yet says how to find it", async ({ page, api }) => {
    api.on(TRACE_PATH, json(paged));
    await page.goto(traceUrl({ span: "c000000000000009" }));
    await expect(page.getByText("This span isn't loaded yet.")).toBeVisible();
  });
});

test.describe("trace detail: links", () => {
  test("without a trace in the URL, it points back to the list and asks for nothing", async ({
    page,
    api,
  }) => {
    await page.goto(`/trace/?project=${PROJECT}`);
    await expect(page.getByText("No trace selected.")).toBeVisible();
    await expect(page.getByRole("link", { name: "Open the traces list" })).toHaveAttribute(
      "href",
      new RegExp(`^/traces/?\\?project=${PROJECT}$`),
    );
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
    await expectAccessible(page, "no trace");
  });

  test("an invalid trace link says so and asks for nothing", async ({ page, api }) => {
    await page.goto(`/trace/?project=${PROJECT}&trace=4bf92f35`);
    await expect(page.getByText("This trace link isn't valid.")).toBeVisible();
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
  });
});

// Scripts the page loaded, by body, so a test can tell whether the layout library was among them:
// its ranker's name appears in no other script.
function collectScripts(page: import("@playwright/test").Page): Promise<string>[] {
  const bodies: Promise<string>[] = [];
  page.on("response", (response) => {
    if (response.request().resourceType() === "script") {
      bodies.push(response.text().catch(() => ""));
    }
  });
  return bodies;
}
const loadsDagre = async (bodies: Promise<string>[]) =>
  (await Promise.all(bodies)).some((body) => body.includes("network-simplex"));

test.describe("trace detail: agent graph", () => {
  test("loading: the layout library loads only for the graph, behind a skeleton", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    const scripts = collectScripts(page);
    await page.goto(traceUrl());
    await expect(page.getByRole("treegrid", { name: "Spans" })).toBeVisible();
    await page.waitForLoadState("networkidle");
    expect(await loadsDagre(scripts)).toBe(false);

    const { promise: gate, resolve: release } = Promise.withResolvers<void>();
    await page.route("**/_next/static/chunks/**", async (route) => {
      await gate;
      await route.continue();
    });
    await page.getByRole("radio", { name: "Graph" }).click();
    const section = page.getByRole("region", { name: "Agent graph" });
    await expect(section.getByRole("status")).toHaveText("Loading agent graph");
    await expectAccessible(page, "agent graph loading");
    release();
    await expect(page.getByRole("listbox", { name: "Agent graph" })).toBeVisible();
    await expect(page).toHaveURL(/view=graph/);
    expect(await loadsDagre(scripts)).toBe(true);
  });

  test("success: each agent, tool and model is a node that says what it calls", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl({ view: "graph" }));
    const graph = page.getByRole("listbox", { name: "Agent graph" });
    await expect(graph.getByRole("option")).toHaveCount(6);
    await expect(graph.getByRole("option", { name: /search_docs/ })).toHaveAccessibleDescription(
      "Called by support agent 2 times.",
    );
    await expect(graph.getByRole("option", { name: /support agent/ })).toHaveAccessibleDescription(
      /^Calls .*search_docs 2 times.*review agent 1 time/,
    );
    await expect(graph.getByRole("option", { name: /send_reply/ })).toContainText("1 error");
    await expectAccessible(page, "agent graph success");
    await page.screenshot({ path: test.info().outputPath("trace-graph.png"), fullPage: true });
  });

  test("keyboard: one tab stop that reaches every node; Enter chooses and filters the timeline", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl({ view: "graph" }));
    const graph = page.getByRole("listbox", { name: "Agent graph" });
    const nodes = graph.getByRole("option");
    await expect(nodes).toHaveCount(6);
    await expect(graph.locator('[role="option"][tabindex="0"]')).toHaveCount(1);

    await page.getByRole("radio", { name: "Graph" }).focus();
    await page.keyboard.press("Tab");
    const reached = new Set<string>();
    for (let step = 0; step < 6; step += 1) {
      reached.add(
        await page.evaluate(() =>
          document.activeElement?.getAttribute("role") === "option"
            ? (document.activeElement.textContent ?? "")
            : "not a node",
        ),
      );
      await page.keyboard.press("ArrowDown");
    }
    expect([...reached].toSorted()).toEqual((await nodes.allTextContents()).toSorted());

    const tool = graph.getByRole("option", { name: /search_docs/ });
    await tool.focus();
    await page.keyboard.press("Enter");
    await expect(tool).toHaveAttribute("aria-selected", "true");
    await expect(
      page.getByText("The timeline shows the 2 spans of tool search_docs."),
    ).toBeVisible();
    await page.getByRole("button", { name: "Show in timeline" }).click();

    const grid = page.getByRole("treegrid", { name: "Spans" });
    await expect(grid.getByRole("rowheader")).toHaveCount(3);
    await expect(page.getByText(/Showing the 2 spans of tool search_docs/)).toBeVisible();
    await expectAccessible(page, "timeline filtered by a graph node");
    await page.getByRole("button", { name: "Show all spans" }).click();
    await expect(grid.getByRole("rowheader")).toHaveCount(8);
  });

  test("error: when the layout library can't load, the section offers a retry that works", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    await expect(page.getByRole("treegrid", { name: "Spans" })).toBeVisible();
    await page.waitForLoadState("networkidle");
    await page.route("**/_next/static/chunks/**", (route) => route.abort());
    await page.getByRole("radio", { name: "Graph" }).click();
    const section = page.getByRole("region", { name: "Agent graph" });
    await expect(section.getByText("This section couldn't load.")).toBeVisible();
    await expectAccessible(page, "agent graph error");

    await page.unroute("**/_next/static/chunks/**");
    await section.getByRole("button", { name: "Retry agent graph" }).click();
    await expect(page.getByRole("listbox", { name: "Agent graph" })).toBeVisible();
  });

  test("empty: a trace without agents, tools or model calls says so and offers the timeline", async ({
    page,
    api,
  }) => {
    api.on(
      TRACE_PATH,
      json(
        traceDetail({
          spans: [
            span("d000000000000001", null, 0, 1_000, { name: "GET /health" }),
            span("d000000000000002", "d000000000000001", 10, 500, { name: "db.query" }),
          ],
        }),
      ),
    );
    await page.goto(traceUrl({ view: "graph" }));
    await expect(page.getByText("This trace has no agents, tools or model calls.")).toBeVisible();
    await expectAccessible(page, "agent graph empty");
    await page.getByRole("button", { name: "Show the timeline" }).click();
    await expect(page.getByRole("treegrid", { name: "Spans" })).toBeVisible();
  });

  test("a paged trace says the graph covers the loaded spans only", async ({ page, api }) => {
    const root = span("e000000000000001", null, 0, 9_000, {
      name: "batch agent",
      observation_type: "agent",
      child_count: 1,
    });
    const tool = span("e000000000000002", "e000000000000001", 10, 500, {
      name: "fetch",
      observation_type: "tool",
      child_count: 0,
    });
    api.on(TRACE_PATH, json(traceDetail({ paged: true, span_count: 9_000, spans: [root, tool] })));
    await page.goto(traceUrl({ view: "graph" }));
    await expect(page.getByText("The graph covers the 2 spans loaded so far.")).toBeVisible();
  });
});

const PAYLOAD_PATH = new RegExp(`/projects/${PROJECT}/spans/${TRACE}/[0-9a-f]{16}/payload$`);

// Answers payload requests over `body` as the server does: 206 for a range with the field's length, the
// whole field when it is empty.
function payload(body: string) {
  const bytes = Buffer.from(body, "utf8");
  return async (route: import("@playwright/test").Route) => {
    const match = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range ?? "");
    if (!match || bytes.length === 0) {
      await route.fulfill({ status: 200, contentType: "text/plain; charset=utf-8", body: bytes });
      return;
    }
    const first = Number(match[1]);
    const last = Math.min(match[2] ? Number(match[2]) : bytes.length - 1, bytes.length - 1);
    await route.fulfill({
      status: 206,
      contentType: "text/plain; charset=utf-8",
      headers: { "content-range": `bytes ${first}-${last}/${bytes.length}` },
      body: bytes.subarray(first, last + 1),
    });
  };
}

test.describe("trace detail: payload", () => {
  const inputTab = traceUrl({ span: "a000000000000002", tab: "input" });

  test("loading: the open tab asks for the first kibibyte and is busy until it comes", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    const release = api.hold(PAYLOAD_PATH, { status: 200, body: '{"q": 1}' });
    await page.goto(inputTab);
    const panel = page.getByRole("region", { name: "Span", exact: true });
    await expect(panel.getByRole("tab", { name: "Input" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(panel.getByRole("status")).toHaveText("Loading the input");
    await expectAccessible(page, "payload loading");
    release();
    await expect(panel.getByRole("tree", { name: "Input" })).toBeVisible();
  });

  test("success: a JSON field is a tree whose branches open in place", async ({ page, api }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    api.on(PAYLOAD_PATH, payload('{"model": "gpt-4.1", "params": {"temperature": 0.2}}'));
    await page.goto(inputTab);
    const tree = page.getByRole("tree", { name: "Input" });
    await expect(tree.getByRole("treeitem")).toHaveCount(2);
    const params = tree.getByRole("treeitem", { name: /params/ });
    await params.focus();
    await page.keyboard.press("ArrowRight");
    await expect(tree.getByRole("treeitem", { name: /temperature/ })).toBeVisible();
    await expectAccessible(page, "payload success");
  });

  test("success: GenAI messages read as a conversation, with the JSON a choice away", async ({
    page,
    api,
  }) => {
    const spans = AGENT_SPANS.map((s) =>
      s.span_id === "a000000000000002" ? { ...s, io_shape: "genai_messages" as const } : s,
    );
    api.on(TRACE_PATH, json(traceDetail({ spans })));
    api.on(
      PAYLOAD_PATH,
      payload(
        JSON.stringify([
          { role: "user", parts: [{ type: "text", content: "Where is my refund?" }] },
          {
            role: "assistant",
            parts: [{ type: "tool_call", name: "search_docs", arguments: { q: "refund" } }],
          },
        ]),
      ),
    );
    await page.goto(inputTab);
    const messages = page.getByRole("list", { name: "Input messages" });
    await expect(messages).toContainText("Where is my refund?");
    await expect(messages).toContainText("Calls tool search_docs");
    await expectAccessible(page, "payload messages");
    await page.getByRole("radio", { name: "JSON" }).click();
    await expect(page.getByRole("tree", { name: "Input" })).toBeVisible();
  });

  test("a large field shows its preview, then loads by byte ranges with progress when asked", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    const body = JSON.stringify({ transcript: "word ".repeat(80_000) });
    api.on(PAYLOAD_PATH, payload(body));
    await page.goto(inputTab);
    await expect(page.getByText(/showing the first 1 KB/)).toBeVisible();
    expect(api.count(PAYLOAD_PATH)).toBe(1);
    await page.getByRole("button", { name: /^Load / }).click();
    await expect(page.getByRole("tree", { name: "Input" })).toBeVisible();
    await expect(page.getByRole("treeitem", { name: /transcript/ })).toBeVisible();
    expect(api.count(PAYLOAD_PATH)).toBe(2);
  });

  test("error: a failed read offers a retry that reads the field again", async ({ page, api }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    api.on(PAYLOAD_PATH, problem(503, "telemetry_store_unavailable", "req-payload"));
    await page.goto(inputTab);
    const panel = page.getByRole("region", { name: "Span", exact: true });
    await expect(panel.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(panel.getByText("req-payload")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "payload error");
    api.on(PAYLOAD_PATH, payload('{"ok": true}'));
    await panel.getByRole("button", { name: "Retry input" }).click();
    await expect(panel.getByRole("tree", { name: "Input" })).toBeVisible();
  });

  test("empty: an empty field, and a payload that is not stored, each say so", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    api.on(PAYLOAD_PATH, payload(""));
    await page.goto(inputTab);
    await expect(page.getByText("No input was recorded.")).toBeVisible();
    await expectAccessible(page, "payload empty");
    api.on(PAYLOAD_PATH, problem(404, "not_found"));
    await page.getByRole("tab", { name: "Output" }).click();
    await expect(page.getByText("This span's output isn't stored.")).toBeVisible();
  });
});

const FACT_PATH = new RegExp(`/projects/${PROJECT}/(tags|flags|feedback)$`);
const written = { fact_id: "f-1", key: "k", version: 1, created_at: AS_OF, target_time: AS_OF };
const facts = (fields: Partial<TraceDetail["facts"]> = {}) => ({
  tags: [],
  bookmarked: false,
  public: null,
  scores: [],
  correction: null,
  ...fields,
});

test.describe("trace detail: facts", () => {
  test("loading: the facts frame holds its place while the trace loads", async ({ page, api }) => {
    const release = api.hold(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const frame = page.getByRole("region", { name: "Tags, flags and feedback" });
    await expect(frame).toHaveAttribute("aria-busy", "true");
    release();
    await expect(frame).not.toHaveAttribute("aria-busy", "true");
  });

  test("success: tags, flags and scores show; a new tag is written and stays", async ({
    page,
    api,
  }) => {
    const score = {
      name: "helpful",
      score_id: null,
      data_type: "boolean" as const,
      value: true,
      source: "feedback" as const,
      comment: "thanks",
      metadata: null,
      fact: {
        fact_id: "f-0",
        version: 1,
        created_at: AS_OF,
        author_kind: "api_key" as const,
        author_id: "k",
      },
    };
    api.on(
      TRACE_PATH,
      json(traceDetail({ facts: facts({ tags: ["seen"], bookmarked: true, scores: [score] }) })),
    );
    api.on(FACT_PATH, json(written, 201));
    await page.goto(traceUrl());
    const frame = page.getByRole("region", { name: "Tags, flags and feedback" });
    await expect(frame.getByText("seen")).toBeVisible();
    await expect(frame.getByRole("button", { name: "Bookmark" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(frame.locator("[data-score=helpful]")).toContainText("thanks");
    await frame.getByRole("textbox", { name: "New tag" }).fill("urgent");
    await frame.getByRole("button", { name: "Add tag" }).click();
    await expect(frame.getByRole("button", { name: "Remove tag urgent" })).toBeVisible();
    expect(api.count(/\/tags$/)).toBe(1);
    await expectAccessible(page, "trace facts");
  });

  test("empty: a trace without facts says so and still offers the forms", async ({ page, api }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const frame = page.getByRole("region", { name: "Tags, flags and feedback" });
    await expect(frame).toContainText("No tags yet.");
    await expect(frame).toContainText("No scores or feedback yet.");
    await expect(frame.getByRole("button", { name: "Add score" })).toBeVisible();
  });

  test("error: a refused write says so with its reference, never the server text", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    api.on(FACT_PATH, problem(503, "ingest_unavailable", "0193a5b2-fact"));
    await page.goto(traceUrl());
    const frame = page.getByRole("region", { name: "Tags, flags and feedback" });
    await frame.getByRole("button", { name: "Bookmark" }).click();
    await expect(frame.getByRole("alert")).toContainText("Couldn't save the flag.");
    await expect(frame.getByRole("alert")).toContainText("0193a5b2-fact");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(frame.getByRole("button", { name: "Bookmark" })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
  });
});
