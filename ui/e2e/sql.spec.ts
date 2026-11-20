import type { Page, Route } from "@playwright/test";
import {
  type Api,
  capture,
  expect,
  expectAccessible,
  expectNoPageScrollX,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  SQL_EXAMPLE,
  SQL_EXPORT_PATH,
  SQL_RUN_PATH,
  SQL_RUNS_PATH,
  SQL_SAVED_ONE_PATH,
  SQL_SAVED_PATH,
  SQL_SAVED_RUN_PATH,
  SQL_SCHEMA_PATH,
  savedQuery,
  sessionInfo,
  signedIn,
  sqlProblem,
  sqlResult,
  sqlRun,
  sqlSchema,
  test,
} from "./fixtures";

const SQL_URL = `/sql/?project=${PROJECT}`;
const LOGIN_PATH = /\/api\/v1\/auth\/login$/;

// Every test starts from a workspace that answers: a schema, no saved queries and no runs.
function healthy(api: Api): void {
  api.on(SQL_SCHEMA_PATH, json(sqlSchema()));
  api.on(SQL_SAVED_PATH, json({ data: [] }));
  api.on(SQL_RUNS_PATH, json({ data: [] }));
}

const section = (page: Page, name: string) => page.getByRole("region", { name, exact: true });
const editor = (page: Page) => page.getByLabel("Query text");
const runButton = (page: Page) => page.getByRole("button", { name: "Run", exact: true });

async function open(page: Page): Promise<void> {
  await page.goto(SQL_URL);
  await expect(editor(page)).toHaveValue(SQL_EXAMPLE);
}

function body(route: Route): Record<string, unknown> {
  return route.request().postDataJSON() as Record<string, unknown>;
}

test.describe("sql: schema browser", () => {
  test("loading: skeleton lines after 300 ms, announced to screen readers", async ({
    page,
    api,
  }) => {
    healthy(api);
    const release = api.hold(SQL_SCHEMA_PATH, json(sqlSchema()));
    await page.goto(SQL_URL);
    const schema = section(page, "Schema");
    await expect(schema).toHaveAttribute("aria-busy", "true");
    await expect(schema.getByRole("status")).toHaveText("Loading schema");
    expect(api.count(SQL_SAVED_PATH) + api.count(SQL_RUNS_PATH)).toBe(0);
    await expectAccessible(page, "sql schema loading");
    release();
    await expect(
      schema.getByRole("button", { name: "crucible_sql.spans", exact: true }),
    ).toBeVisible();
  });

  test("success: relations with description, columns, types and time column; the first example starts the editor", async ({
    page,
    api,
  }) => {
    healthy(api);
    await open(page);
    const schema = section(page, "Schema");
    await expect(schema.getByText("One row per span of this project.")).toBeVisible();
    await expect(schema.getByText("A query must filter it with the time window.")).toBeVisible();
    const columns = schema.getByRole("table", { name: "Columns of crucible_sql.spans" });
    await expect(columns.locator("tbody tr")).toHaveText([
      /start_time\s*DateTime64\(6\)/,
      /name\s*String/,
      /total_tokens\s*UInt64/,
    ]);
    await expect(section(page, "Query")).toContainText("are bound to the time range in the header");
    await expectAccessible(page, "sql schema success");

    await schema.getByRole("button", { name: "crucible_sql.span_metrics", exact: true }).click();
    await expect(schema.getByText("Span counts and token totals per minute.")).toBeVisible();
    await schema
      .getByRole("button", { name: "Insert example for crucible_sql.span_metrics" })
      .click();
    await expect(editor(page)).toHaveValue(/FROM crucible_sql\.span_metrics/);
    await expect(editor(page)).toBeFocused();
    await expect(section(page, "Query").getByRole("status")).toContainText(
      "Inserted the example for crucible_sql.span_metrics.",
    );
  });

  test("error: an in-section card whose retry refetches only the schema", async ({ page, api }) => {
    healthy(api);
    api.on(SQL_SCHEMA_PATH, problem(503, "telemetry_store_unavailable", "req-schema"));
    await page.goto(SQL_URL);
    const schema = section(page, "Schema");
    await expect(schema.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(schema.getByText("req-schema")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    expect(api.count(SQL_SCHEMA_PATH)).toBe(2);
    await expect(section(page, "History").getByText("No runs yet.")).toBeVisible();
    await expectAccessible(page, "sql schema error");

    api.on(SQL_SCHEMA_PATH, json(sqlSchema()));
    await schema.getByRole("button", { name: "Retry schema" }).click();
    await expect(
      schema.getByRole("button", { name: "crucible_sql.spans", exact: true }),
    ).toBeVisible();
    expect(api.count(SQL_SCHEMA_PATH)).toBe(3);
    expect(api.count(SQL_RUNS_PATH)).toBe(1);
  });

  test("empty: a workspace without relations says so", async ({ page, api }) => {
    healthy(api);
    api.on(SQL_SCHEMA_PATH, json(sqlSchema({ relations: [] })));
    await page.goto(SQL_URL);
    await expect(section(page, "Schema").getByText("No relations to query.")).toBeVisible();
    await expect(editor(page)).toHaveValue("");
    await expect(runButton(page)).toBeDisabled();
    await expectAccessible(page, "sql schema empty");
  });
});

test.describe("sql: workspace unavailable", () => {
  test("isolation_probe_failed: a page state with operator guidance, Run off, nothing else requested", async ({
    page,
    api,
  }) => {
    api.on(
      SQL_SCHEMA_PATH,
      json(
        sqlSchema({
          status: {
            enabled: false,
            reason: "isolation_probe_failed",
            failed_checks: ["cross_project_read"],
          },
        }),
      ),
    );
    await page.goto(SQL_URL);
    const state = section(page, "The SQL workspace is switched off.");
    await expect(state).toContainText("Its isolation self-check failed");
    await expect(state).toContainText("An operator should read the health page");
    await expect(state).toContainText("cross_project_read");
    await expect(state).toHaveAttribute("data-unavailable", "isolation_probe_failed");
    await expect(runButton(page)).toBeDisabled();
    await expect(page.getByRole("button", { name: "Export CSV" })).toBeDisabled();
    await expect(section(page, "Saved queries")).toHaveCount(0);
    expect(api.count(SQL_SAVED_PATH) + api.count(SQL_RUNS_PATH)).toBe(0);
    await expectAccessible(page, "sql workspace unavailable");

    healthy(api);
    await state.getByRole("button", { name: "Check again" }).click();
    await expect(state).toHaveCount(0);
    await expect(runButton(page)).toBeEnabled();
    await expect(section(page, "History").getByText("No runs yet.")).toBeVisible();
  });

  test("not_configured and probe_pending: a 503 from the schema names the reason", async ({
    page,
    api,
  }) => {
    api.on(
      SQL_SCHEMA_PATH,
      sqlProblem(503, "sql_workspace_unavailable", {
        reason: "not_configured",
        detail: "server sentence",
      }),
    );
    await page.goto(SQL_URL);
    const state = section(page, "The SQL workspace is not set up on this server.");
    await expect(state).toContainText("An operator configures both and runs crucible migrate.", {
      timeout: 10_000,
    });
    await expect(runButton(page)).toBeDisabled();

    api.on(
      SQL_SCHEMA_PATH,
      sqlProblem(503, "sql_workspace_unavailable", { reason: "probe_pending" }),
    );
    await state.getByRole("button", { name: "Check again" }).click();
    await expect(section(page, "The SQL workspace is starting.")).toContainText(
      "Check again in a moment.",
      { timeout: 10_000 },
    );
  });
});

test.describe("sql: editor and result", () => {
  test("loading: Run is off while a run is in flight, with progress in the result", async ({
    page,
    api,
  }) => {
    healthy(api);
    const release = api.hold(SQL_RUN_PATH, json(sqlResult()));
    await open(page);
    const result = section(page, "Result");
    await expect(result.getByText("No result yet.")).toBeVisible();
    await runButton(page).click();
    await expect(runButton(page)).toBeDisabled();
    await expect(page.getByRole("button", { name: "Cancel", exact: true })).toBeEnabled();
    await expect(result).toHaveAttribute("aria-busy", "true");
    await expect(result.getByRole("progressbar", { name: "Running the query" })).toBeVisible();
    await expectAccessible(page, "sql run in flight");
    release();
    await expect(result.getByRole("table")).toBeVisible();
    await expect(runButton(page)).toBeEnabled();
  });

  test("success: a typed grid with right-aligned numbers, a distinct null, compact JSON and read statistics", async ({
    page,
    api,
  }) => {
    healthy(api);
    let sent: Record<string, unknown> = {};
    api.on(SQL_RUN_PATH, async (route) => {
      sent = body(route);
      await route.fulfill(json(sqlResult()));
    });
    await open(page);
    await runButton(page).click();
    const table = section(page, "Result").getByRole("table");
    await expect(table.getByRole("columnheader")).toHaveText([
      /name\s*String/,
      /spans\s*UInt64/,
      /model\s*Nullable\(String\)/,
      /tags\s*Array\(String\)/,
    ]);
    const first = table.locator("tbody tr").first().getByRole("cell");
    await expect(first).toHaveText([
      "support agent",
      "18446744073709551615",
      "gpt-4.1",
      '["prod","eu"]',
    ]);
    await expect(first.nth(1)).toHaveCSS("text-align", "end");
    await expect(first.nth(0)).toHaveCSS("text-align", "start");
    await expect(table.getByRole("columnheader").nth(1)).toHaveCSS("text-align", "end");
    const second = table.locator("tbody tr").nth(1).getByRole("cell");
    await expect(second.nth(2).locator("[data-null]")).toHaveText("null");
    await expect(second.nth(2).locator("[data-null]")).toHaveCSS("font-style", "italic");
    await expect(section(page, "Result").locator("[data-stats]")).toHaveText(
      "2 rows returned. Read 120,000 rows (3.0 MiB) in 85 ms.",
    );
    await expect(section(page, "Result").locator("[data-notice]")).toHaveCount(0);

    expect(sent.sql).toBe(SQL_EXAMPLE);
    expect(sent.params).toEqual({});
    expect(String(sent.to)).toMatch(/^\d{4}-\d\d-\d\dT\d\d:\d\d:00\.000000Z$/);
    expect(Date.parse(String(sent.to)) - Date.parse(String(sent.from))).toBe(24 * 60 * 60 * 1000);
    await expect.poll(() => api.count(SQL_RUNS_PATH)).toBe(2);
    await expectAccessible(page, "sql result grid");
  });

  test("success: a truncated result says so, counts cut values and pages 100 rows at a time", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(
      SQL_RUN_PATH,
      json(
        sqlResult({
          columns: [{ name: "n", type: "UInt32" }],
          rows: Array.from({ length: 250 }, (_, index) => [index + 1]),
          truncated: true,
          cut_cells: 2,
        }),
      ),
    );
    await open(page);
    await runButton(page).click();
    const result = section(page, "Result");
    await expect(result.locator('[data-notice="truncated"]')).toHaveText(
      "Showing the first 250 rows. Narrow the query or export it to get every row.",
    );
    await expect(result.locator('[data-notice="cut"]')).toContainText(
      "2 values were too long and were cut. A cut value ends with …[cut].",
    );
    await expect(result.locator("tbody tr")).toHaveCount(100);
    await expect(result.locator("[data-page-range]")).toHaveText("Rows 1 to 100 of 250");
    await expectAccessible(page, "sql result truncated");

    const pages = result.getByRole("navigation", { name: "Result pages" });
    await pages.getByText("Next").click();
    await expect(result.locator("[data-page-range]")).toHaveText("Rows 101 to 200 of 250");
    await expect(result.locator("tbody tr").first()).toHaveText("101");
    await pages.getByText("3", { exact: true }).click();
    await expect(result.locator("tbody tr")).toHaveCount(50);
  });

  test("empty: a result without rows says the query returned none", async ({ page, api }) => {
    healthy(api);
    api.on(SQL_RUN_PATH, json(sqlResult({ rows: [] })));
    await open(page);
    await runButton(page).click();
    const result = section(page, "Result");
    await expect(result.getByText("The query returned no rows.")).toBeVisible();
    await expect(result.getByRole("table")).toHaveCount(0);
    await expect(result.locator("[data-stats]")).toContainText("0 rows returned.");
    await expectAccessible(page, "sql result empty");
  });

  test("error: sql_invalid shows the fix, what ClickHouse said and the position, and moves the caret there", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(
      SQL_RUN_PATH,
      sqlProblem(
        422,
        "sql_invalid",
        {
          detail: "Correct the query near line 2: ClickHouse could not parse it.",
          clickhouse: "Syntax error: failed at position 31 ('crucible_sql')",
          line: 2,
          column: 6,
        },
        "req-run",
      ),
    );
    await open(page);
    await runButton(page).click();
    const card = section(page, "Result").getByRole("alert");
    await expect(card).toHaveAttribute("data-error", "sql_invalid");
    await expect(card).toContainText("The query has an error.");
    await expect(card.locator("[data-fix]")).toHaveText(
      "Correct the query near line 2: ClickHouse could not parse it.",
    );
    await expect(card).toContainText("ClickHouse said");
    await expect(card.locator("[data-clickhouse]")).toHaveText(
      "Syntax error: failed at position 31 ('crucible_sql')",
    );
    await expect(card.locator("[data-clickhouse]")).toHaveCSS("font-family", /mono/i);
    await expect(card.locator("[data-position]")).toHaveText("Line 2, column 6");
    await expect(card).toContainText("req-run");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    const offset = SQL_EXAMPLE.indexOf("\n") + 6;
    expect(await editor(page).evaluate((field: HTMLTextAreaElement) => field.selectionStart)).toBe(
      offset,
    );
    await expectAccessible(page, "sql run error");

    await editor(page).evaluate((field: HTMLTextAreaElement) => field.setSelectionRange(0, 0));
    await card.getByRole("button", { name: "Go to line 2" }).click();
    await expect(editor(page)).toBeFocused();
    expect(await editor(page).evaluate((field: HTMLTextAreaElement) => field.selectionStart)).toBe(
      offset,
    );
    await expect(runButton(page)).toBeEnabled();
  });

  const REFUSALS = [
    {
      code: "sql_time_window_required",
      status: 422,
      title: "The query needs the time window.",
      detail:
        "Add start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)} to the WHERE clause.",
    },
    {
      code: "sql_forbidden",
      status: 403,
      title: "The workspace doesn't allow this query.",
      detail:
        "Remove the SETTINGS clause: the workspace fixes its settings and limits for every query.",
    },
    {
      code: "query_too_broad",
      status: 422,
      title: "The query would read too much.",
      detail: "Narrow the time window or add a filter on a key column.",
    },
    {
      code: "forbidden",
      status: 403,
      title: "Your role can't do this.",
      detail:
        "Running ad hoc SQL needs the member role on this project. Viewers can run the project's saved queries.",
    },
  ] as const;

  for (const refusal of REFUSALS) {
    test(`error: ${refusal.code} renders its title and the fix sentence`, async ({ page, api }) => {
      healthy(api);
      api.on(SQL_RUN_PATH, sqlProblem(refusal.status, refusal.code, { detail: refusal.detail }));
      await open(page);
      await runButton(page).click();
      const card = section(page, "Result").getByRole("alert");
      await expect(card).toHaveAttribute("data-error", refusal.code);
      await expect(card).toContainText(refusal.title);
      await expect(card.locator("[data-fix]")).toHaveText(refusal.detail);
      await expect(card.locator("[data-clickhouse]")).toHaveCount(0);
      await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
      await expectAccessible(page, `sql ${refusal.code}`);
    });
  }

  test("error: sql_quota_exceeded says how long to wait, with what ClickHouse said", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(
      SQL_RUN_PATH,
      sqlProblem(429, "sql_quota_exceeded", {
        detail: "You started 30 queries in the last minute, which is the limit.",
        clickhouse: "Quota for user has been exceeded",
        retry_after_s: 12,
      }),
    );
    await open(page);
    await runButton(page).click();
    const card = section(page, "Result").getByRole("alert");
    await expect(card).toContainText("This project has reached its SQL limit for now.");
    await expect(card.locator("[data-fix]")).toHaveText(
      "You started 30 queries in the last minute, which is the limit.",
    );
    await expect(card.locator("[data-retry-after]")).toHaveText("You can run it again in 12 s.");
    await expect(card.locator("[data-clickhouse]")).toHaveText("Quota for user has been exceeded");
    expect(api.count(SQL_RUN_PATH)).toBe(1);
    await expectAccessible(page, "sql quota exceeded");
  });

  test("error: a failure the workspace does not explain keeps the shared copy and hides server text", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(SQL_RUN_PATH, problem(500, "internal_error", "req-500"));
    await open(page);
    await runButton(page).click();
    const card = section(page, "Result").getByRole("alert");
    await expect(card).toContainText("Nothing was changed. Try again in a moment.");
    await expect(card).toContainText("req-500");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
  });

  test("error: a run refused because the workspace went away shows the page state", async ({
    page,
    api,
  }) => {
    healthy(api);
    const off = sqlProblem(503, "sql_workspace_unavailable", { reason: "isolation_probe_failed" });
    api.on(SQL_RUN_PATH, off);
    await open(page);
    api.on(SQL_SCHEMA_PATH, off);
    await runButton(page).click();
    await expect(section(page, "Result").getByRole("alert")).toContainText(
      "The SQL workspace is switched off.",
    );
    await expect(section(page, "The SQL workspace is switched off.")).toBeVisible({
      timeout: 10_000,
    });
    await expect(runButton(page)).toBeDisabled();
  });

  test("cancel: Cancel aborts the request and the result says the run was cancelled", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(SQL_RUN_PATH, () => {});
    await open(page);
    const aborted = page.waitForEvent(
      "requestfailed",
      (request) =>
        request.method() === "POST" && SQL_RUN_PATH.test(new URL(request.url()).pathname),
    );
    await runButton(page).click();
    const cancel = page.getByRole("button", { name: "Cancel", exact: true });
    await expect(section(page, "Result").getByRole("progressbar")).toBeVisible();
    await cancel.click();
    expect((await aborted).failure()?.errorText).toContain("ERR_ABORTED");
    const result = section(page, "Result");
    await expect(result.locator("[data-cancelled]")).toHaveText("The run was cancelled.");
    await expect(result.getByRole("alert")).toHaveCount(0);
    await expect(cancel).toBeDisabled();
    await expect(runButton(page)).toBeEnabled();
    await expect.poll(() => api.count(SQL_RUNS_PATH)).toBe(2);
    await expectAccessible(page, "sql run cancelled");
  });

  test("keyboard: Ctrl+Enter in the editor runs the query", async ({ page, api }) => {
    healthy(api);
    let sent: Record<string, unknown> = {};
    api.on(SQL_RUN_PATH, async (route) => {
      sent = body(route);
      await route.fulfill(json(sqlResult()));
    });
    await open(page);
    await editor(page).focus();
    await page.keyboard.press("ControlOrMeta+A");
    await page.keyboard.type("SELECT 1");
    await page.keyboard.press("Control+Enter");
    await expect(section(page, "Result").getByRole("table")).toBeVisible();
    expect(sent.sql).toBe("SELECT 1");
    await expect(editor(page)).toBeFocused();
    expect(api.count(SQL_RUN_PATH)).toBe(1);
  });

  test("parameters: each {name:Type} besides the window gets a labelled field, sent as params", async ({
    page,
    api,
  }) => {
    healthy(api);
    let sent: Record<string, unknown> = {};
    api.on(SQL_RUN_PATH, async (route) => {
      sent = body(route);
      await route.fulfill(json(sqlResult()));
    });
    await open(page);
    const query = section(page, "Query");
    await expect(query.getByRole("group", { name: "Parameters" })).toHaveCount(0);
    await editor(page).fill(`${SQL_EXAMPLE}\n-- {model:String} {min_tokens:UInt64} {model:String}`);
    const parameters = query.getByRole("group", { name: "Parameters" });
    await expect(parameters.getByRole("textbox")).toHaveCount(2);
    await parameters.getByLabel("model").fill("gpt-4.1");
    await parameters.getByLabel("min_tokens").fill("100");
    await expectAccessible(page, "sql parameters");
    await parameters.getByLabel("min_tokens").press("Enter");
    await expect(section(page, "Result").getByRole("table")).toBeVisible();
    expect(sent.params).toEqual({ model: "gpt-4.1", min_tokens: "100" });
  });
});

test.describe("sql: export", () => {
  test("each format downloads the file for the current text and window", async ({ page, api }) => {
    healthy(api);
    let sent: Record<string, unknown> = {};
    api.on(SQL_EXPORT_PATH, async (route) => {
      sent = body(route);
      await route.fulfill({
        status: 200,
        contentType: "text/csv",
        headers: { "content-disposition": 'attachment; filename="crucible-sql.csv"' },
        body: "name,spans\nsupport agent,2\n",
      });
    });
    await open(page);
    const downloading = page.waitForEvent("download");
    await page.getByRole("button", { name: "Export CSV" }).click();
    const download = await downloading;
    expect(download.suggestedFilename()).toBe("crucible-sql.csv");
    expect(sent).toMatchObject({ sql: SQL_EXAMPLE, format: "csv", params: {} });
    expect(Date.parse(String(sent.to)) - Date.parse(String(sent.from))).toBe(24 * 60 * 60 * 1000);
    await expect(section(page, "Query").getByRole("status")).toContainText(
      "Downloaded crucible-sql.csv.",
    );
    await expect.poll(() => api.count(SQL_RUNS_PATH)).toBe(2);
    await expect(page.getByRole("button", { name: "Export JSON lines" })).toBeEnabled();
    await expect(page.getByRole("button", { name: "Export Parquet" })).toBeEnabled();
    await expectAccessible(page, "sql exported");
  });

  test("feedback while exporting, and a failure renders the fix", async ({ page, api }) => {
    healthy(api);
    const release = api.hold(
      SQL_EXPORT_PATH,
      sqlProblem(403, "forbidden", { detail: "Exporting needs the member role on this project." }),
    );
    await open(page);
    await page.getByRole("button", { name: "Export Parquet" }).click();
    const query = section(page, "Query");
    await expect(query.getByRole("progressbar", { name: "Exporting Parquet" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Export CSV" })).toBeDisabled();
    await expectAccessible(page, "sql exporting");
    release();
    const card = query.getByRole("alert");
    await expect(card).toContainText("Your role can't do this.");
    await expect(card.locator("[data-fix]")).toHaveText(
      "Exporting needs the member role on this project.",
    );
    await expect(query.getByRole("progressbar")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Export CSV" })).toBeEnabled();
    await expectAccessible(page, "sql export failed");
  });
});

test.describe("sql: saved queries", () => {
  test("loading: skeleton rows, announced", async ({ page, api }) => {
    healthy(api);
    const release = api.hold(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    await page.goto(SQL_URL);
    const saved = section(page, "Saved queries");
    await expect(saved.getByRole("status")).toHaveText("Loading saved queries");
    await expect(saved).toHaveAttribute("aria-busy", "true");
    await expectAccessible(page, "sql saved queries loading");
    release();
    await expect(saved.getByRole("button", { name: "Open Spans by model" })).toBeVisible();
  });

  test("success: name, description, visibility and parameters; actions follow can_edit and can_run; Open fills the editor", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(
      SQL_SAVED_PATH,
      json({
        data: [
          savedQuery(),
          savedQuery({
            id: "0193a5b2-0000-7000-8000-0000000000f2",
            name: "Private draft",
            description: "",
            visibility: "private",
            parameters: [],
            sql: "SELECT 2",
            can_edit: false,
            can_run: false,
          }),
        ],
      }),
    );
    await open(page);
    const saved = section(page, "Saved queries");
    const first = saved.getByRole("listitem").filter({ hasText: "Spans by model" });
    await expect(first).toContainText("Span counts for one model.");
    await expect(first).toContainText("Shared with the project. Parameters:");
    await expect(first).toContainText("model: string = gpt-4.1");
    await expect(first).toContainText("min_tokens: int");
    await expect(first.getByRole("button")).toHaveText([
      "Open Spans by model",
      "Run Spans by model",
      "Delete Spans by model",
    ]);
    const second = saved.getByRole("listitem").filter({ hasText: "Private draft" });
    await expect(second).toContainText("Private. No parameters.");
    await expect(second.getByRole("button")).toHaveText(["Open Private draft"]);
    await expectAccessible(page, "sql saved queries success");

    await first.getByRole("button", { name: "Open Spans by model" }).click();
    await expect(editor(page)).toHaveValue(savedQuery().sql);
    const query = section(page, "Query");
    await expect(query.locator("[data-loaded]")).toContainText("Spans by model");
    await expect(query.getByRole("group", { name: "Parameters" }).getByLabel("model")).toHaveValue(
      "gpt-4.1",
    );
    await expect(query.getByRole("button", { name: "Save changes" })).toBeVisible();

    await second.getByRole("button", { name: "Open Private draft" }).click();
    await expect(editor(page)).toHaveValue("SELECT 2");
    await expect(query.getByRole("button", { name: "Save changes" })).toHaveCount(0);
  });

  test("error: an in-section card whose retry refetches only the list", async ({ page, api }) => {
    healthy(api);
    api.on(
      SQL_SAVED_PATH,
      sqlProblem(
        503,
        "control_plane_unavailable",
        { detail: "The control plane database did not answer. Try again shortly." },
        "req-saved",
      ),
    );
    await page.goto(SQL_URL);
    const saved = section(page, "Saved queries");
    await expect(
      saved.getByText("Saved queries and history are unavailable right now."),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      saved.getByText("The control plane database did not answer. Try again shortly."),
    ).toBeVisible();
    await expect(saved.getByText("req-saved")).toBeVisible();
    expect(api.count(SQL_SAVED_PATH)).toBe(2);
    await expectAccessible(page, "sql saved queries error");

    api.on(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    await saved.getByRole("button", { name: "Retry saved queries" }).click();
    await expect(saved.getByRole("button", { name: "Open Spans by model" })).toBeVisible();
    expect(api.count(SQL_SAVED_PATH)).toBe(3);
    expect(api.count(SQL_SCHEMA_PATH)).toBe(1);
  });

  test("empty: no saved queries invites the first save", async ({ page, api }) => {
    healthy(api);
    await open(page);
    const saved = section(page, "Saved queries");
    await expect(saved.getByText("No saved queries yet.")).toBeVisible();
    await expect(saved.locator("[data-empty]")).toHaveAttribute("data-empty", "first-use");
    await expectAccessible(page, "sql saved queries empty");
  });

  test("run: a form built from the declared parameters, defaults prefilled, sends them", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    let sent: Record<string, unknown> = {};
    let path = "";
    api.on(SQL_SAVED_RUN_PATH, async (route, url) => {
      sent = body(route);
      path = url.pathname;
      await route.fulfill(json(sqlResult()));
    });
    await open(page);
    await page.getByRole("button", { name: "Run Spans by model" }).click();
    const dialog = page.getByRole("dialog", { name: "Run “Spans by model”" });
    await expect(dialog.getByLabel("model")).toHaveValue("gpt-4.1");
    await expect(dialog.getByLabel("min_tokens")).toHaveValue("");
    await expectAccessible(page, "sql run saved dialog");

    await dialog.getByRole("button", { name: "Run saved query" }).click();
    await expect(dialog.getByText("Enter a value for min_tokens.")).toBeVisible();
    expect(api.count(SQL_SAVED_RUN_PATH)).toBe(0);

    await dialog.getByLabel("min_tokens").fill("500");
    await dialog.getByLabel("model").fill("");
    await dialog.getByRole("button", { name: "Run saved query" }).click();
    await expect(dialog).toBeHidden();
    await expect(section(page, "Result").getByRole("table")).toBeVisible();
    expect(path).toContain(`/saved-queries/${savedQuery().id}/run`);
    expect(sent.params).toEqual({ min_tokens: "500" });
    expect(Object.keys(sent).toSorted()).toEqual(["from", "params", "to"]);
    await expect(editor(page)).toHaveValue(SQL_EXAMPLE);
  });

  test("run: a saved query without parameters runs at once, and Escape leaves the form without running", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(
      SQL_SAVED_PATH,
      json({
        data: [
          savedQuery(),
          savedQuery({
            id: "0193a5b2-0000-7000-8000-0000000000f3",
            name: "All spans",
            parameters: [],
          }),
        ],
      }),
    );
    api.on(SQL_SAVED_RUN_PATH, json(sqlResult({ rows: [] })));
    await open(page);
    const trigger = page.getByRole("button", { name: "Run Spans by model" });
    await trigger.focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Run “Spans by model”" });
    await expect(dialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(trigger).toBeFocused();
    expect(api.count(SQL_SAVED_RUN_PATH)).toBe(0);

    await page.getByRole("button", { name: "Run All spans" }).click();
    await expect(section(page, "Result").getByText("The query returned no rows.")).toBeVisible();
    expect(api.count(SQL_SAVED_RUN_PATH)).toBe(1);
  });

  test("save as new: the form declares the detected parameters and posts the query", async ({
    page,
    api,
  }) => {
    healthy(api);
    let sent: Record<string, unknown> = {};
    api.on(SQL_SAVED_PATH, async (route) => {
      if (route.request().method() !== "POST") {
        await route.fulfill(
          json({ data: sent.name ? [savedQuery({ name: String(sent.name) })] : [] }),
        );
        return;
      }
      sent = body(route);
      await route.fulfill({
        ...json(savedQuery({ name: String(sent.name), version: 1 }), 201),
        headers: { etag: '"1"' },
      });
    });
    await open(page);
    const text = `${SQL_EXAMPLE}\n-- {model:String} {min_tokens:UInt64}`;
    await editor(page).fill(text);
    await page.getByRole("button", { name: "Save as new" }).click();
    const dialog = page.getByRole("dialog", { name: "Save as a new query" });
    await expect(dialog.getByLabel("Name")).toBeFocused();
    await expect(dialog.getByLabel("Type of model")).toContainText("String");
    await expect(dialog.getByLabel("Type of min_tokens")).toContainText("Integer");
    await expectAccessible(page, "sql save dialog");

    await dialog.getByRole("button", { name: "Save query" }).click();
    await expect(dialog.getByText("Give the saved query a name.")).toBeVisible();

    await dialog.getByLabel("Name").fill("Tokens by model");
    await dialog.getByLabel("Description").fill("Counts per model.");
    await dialog.getByLabel("Visibility").click();
    await page.getByRole("option", { name: "Project: everyone in the project" }).click();
    await dialog.getByLabel("Type of model").click();
    await page.getByRole("option", { name: "UUID" }).click();
    await dialog.getByLabel("Default for min_tokens").fill("10");
    await dialog.getByRole("button", { name: "Save query" }).click();
    await expect(dialog).toBeHidden();
    expect(sent).toEqual({
      name: "Tokens by model",
      description: "Counts per model.",
      visibility: "project",
      sql: text,
      parameters: [
        { name: "model", type: "uuid" },
        { name: "min_tokens", type: "int", default: "10" },
      ],
    });
    const query = section(page, "Query");
    await expect(query.getByRole("status")).toContainText("Saved “Tokens by model”.");
    await expect(
      section(page, "Saved queries").getByRole("button", { name: "Open Tokens by model" }),
    ).toBeVisible();
    await expect(query.getByRole("button", { name: "Save changes" })).toBeVisible();
  });

  test("update: sends If-Match with the loaded version; a stale version says it changed and offers a reload", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    const seen: { method: string; ifMatch: string | undefined }[] = [];
    let stale = true;
    api.on(SQL_SAVED_ONE_PATH, async (route) => {
      const request = route.request();
      seen.push({ method: request.method(), ifMatch: request.headers()["if-match"] });
      if (request.method() === "GET") {
        await route.fulfill(json(savedQuery({ version: 4, sql: "SELECT 'newer'" })));
      } else if (stale) {
        await route.fulfill(
          sqlProblem(412, "precondition_failed", {
            detail:
              "The saved query changed since you loaded it. Reload it and apply your change again.",
          }),
        );
      } else {
        await route.fulfill(json(savedQuery({ version: 5, ...body(route) })));
      }
    });
    await open(page);
    await page.getByRole("button", { name: "Open Spans by model" }).click();
    await editor(page).fill("SELECT 'mine'");
    await page.getByRole("button", { name: "Save changes" }).click();
    const dialog = page.getByRole("dialog", { name: "Save changes" });
    await expect(dialog.getByLabel("Name")).toHaveValue("Spans by model");
    await expect(dialog.getByLabel("Visibility")).toContainText("Project");
    await dialog.getByRole("button", { name: "Save changes" }).click();
    const card = dialog.getByRole("alert");
    await expect(card).toContainText("This saved query changed since you opened it.");
    await expect(card.locator("[data-fix]")).toHaveText(
      "The saved query changed since you loaded it. Reload it and apply your change again.",
    );
    expect(seen).toEqual([{ method: "PATCH", ifMatch: '"3"' }]);
    await expectAccessible(page, "sql save conflict");

    await card.getByRole("button", { name: "Reload saved query" }).click();
    await expect(dialog).toBeHidden();
    await expect(editor(page)).toHaveValue("SELECT 'newer'");
    await expect(section(page, "Query").getByRole("status")).toContainText(
      "Reloaded “Spans by model”. Apply your change again.",
    );

    stale = false;
    await editor(page).fill("SELECT 'mine again'");
    await page.getByRole("button", { name: "Save changes" }).click();
    await dialog.getByRole("button", { name: "Save changes" }).click();
    await expect(dialog).toBeHidden();
    await expect(section(page, "Query").getByRole("status")).toContainText(
      "Saved changes to “Spans by model”.",
    );
    expect(seen.at(-1)).toEqual({ method: "PATCH", ifMatch: '"4"' });
  });

  test("delete: asks first; Keep it changes nothing, Delete removes the query and says so", async ({
    page,
    api,
  }) => {
    healthy(api);
    let deleted = false;
    api.on(SQL_SAVED_PATH, (route) => route.fulfill(json({ data: deleted ? [] : [savedQuery()] })));
    const methods: string[] = [];
    api.on(SQL_SAVED_ONE_PATH, async (route) => {
      methods.push(route.request().method());
      deleted = true;
      await route.fulfill({ status: 204 });
    });
    await open(page);
    const saved = section(page, "Saved queries");
    await saved.getByRole("button", { name: "Delete Spans by model" }).click();
    const dialog = page.getByRole("alertdialog", { name: "Delete “Spans by model”?" });
    await expect(dialog).toBeVisible();
    await expectAccessible(page, "sql delete confirmation");
    await dialog.getByRole("button", { name: "Keep it" }).click();
    await expect(dialog).toBeHidden();
    expect(methods).toEqual([]);

    await saved.getByRole("button", { name: "Delete Spans by model" }).click();
    await dialog.getByRole("button", { name: "Delete saved query" }).click();
    await expect(saved.getByRole("status")).toContainText("Deleted “Spans by model”.");
    await expect(saved.getByRole("listitem")).toHaveCount(0);
    await expect(saved.getByText("No saved queries yet.")).toBeVisible();
    expect(methods).toEqual(["DELETE"]);
  });

  test("delete: a refusal stays in the section with its fix", async ({ page, api }) => {
    healthy(api);
    api.on(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    api.on(
      SQL_SAVED_ONE_PATH,
      sqlProblem(403, "forbidden", {
        detail:
          "Only its owner, or a project admin for a project-visible query, can delete a saved query.",
      }),
    );
    await open(page);
    const saved = section(page, "Saved queries");
    await saved.getByRole("button", { name: "Delete Spans by model" }).click();
    await page.getByRole("button", { name: "Delete saved query" }).click();
    await expect(saved.getByRole("alert").locator("[data-fix]")).toHaveText(
      "Only its owner, or a project admin for a project-visible query, can delete a saved query.",
    );
    await expect(saved.getByRole("listitem")).toHaveCount(1);
  });
});

test.describe("sql: history", () => {
  test("loading: skeleton rows, announced", async ({ page, api }) => {
    healthy(api);
    const release = api.hold(SQL_RUNS_PATH, json({ data: [sqlRun()] }));
    await page.goto(SQL_URL);
    const history = section(page, "History");
    await expect(history.getByRole("status")).toHaveText("Loading history");
    await expect(history).toHaveAttribute("aria-busy", "true");
    await expectAccessible(page, "sql history loading");
    release();
    await expect(history.getByRole("table")).toBeVisible();
  });

  test("success: time, outcome as icon and label, rows, elapsed and the first line; a run loads into the editor", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(
      SQL_RUNS_PATH,
      json({
        data: [
          sqlRun(),
          sqlRun({
            id: "0193a5b2-0000-7000-8000-00000000000b",
            kind: "saved",
            outcome: "failed",
            error_code: "sql_invalid",
            sql: "SELECT broken",
            result_rows: 0,
            elapsed_ms: 12,
          }),
          sqlRun({
            id: "0193a5b2-0000-7000-8000-00000000000c",
            kind: "export",
            outcome: "cancelled",
            sql: "SELECT cancelled",
            result_rows: 0,
          }),
          sqlRun({
            id: "0193a5b2-0000-7000-8000-00000000000d",
            outcome: "running",
            finished_at: undefined,
            sql: "SELECT running",
            result_rows: 0,
          }),
        ],
      }),
    );
    await open(page);
    const history = section(page, "History");
    expect(
      new URL(`http://localhost${api.calls.find((call) => call.includes("/sql/runs"))}`).search,
    ).toBe("?limit=50");
    const rows = history.locator("tbody tr");
    await expect(rows).toHaveCount(4);
    await expect(rows.first()).toContainText("SELECT count() FROM crucible_sql.spans");
    await expect(rows.first()).toContainText("85 ms");
    await expect(rows.first()).toContainText("Ad hoc");
    for (const [index, outcome, label] of [
      [0, "succeeded", "Succeeded"],
      [1, "failed", "Failed"],
      [2, "cancelled", "Cancelled"],
      [3, "running", "Running"],
    ] as const) {
      const cell = rows.nth(index).locator(`[data-outcome="${outcome}"]`);
      await expect(cell).toHaveText(label);
      await expect(cell.locator("svg")).toHaveCount(1);
    }
    await expectAccessible(page, "sql history success");

    await rows
      .nth(1)
      .getByRole("button", { name: /Load into editor/ })
      .click();
    await expect(editor(page)).toHaveValue("SELECT broken");
    await expect(editor(page)).toBeFocused();
  });

  test("error: an in-section card whose retry refetches only the history", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(SQL_RUNS_PATH, problem(503, "telemetry_store_unavailable", "req-runs"));
    await page.goto(SQL_URL);
    const history = section(page, "History");
    await expect(history.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(history.getByText("req-runs")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    expect(api.count(SQL_RUNS_PATH)).toBe(2);
    await expectAccessible(page, "sql history error");

    api.on(SQL_RUNS_PATH, json({ data: [sqlRun()] }));
    await history.getByRole("button", { name: "Retry history" }).click();
    await expect(history.getByRole("table")).toBeVisible();
    expect(api.count(SQL_RUNS_PATH)).toBe(3);
    expect(api.count(SQL_SAVED_PATH)).toBe(1);
  });

  test("empty: no runs yet, and the list refreshes after a run", async ({ page, api }) => {
    healthy(api);
    api.on(SQL_RUN_PATH, json(sqlResult()));
    await open(page);
    const history = section(page, "History");
    await expect(history.getByText("No runs yet.")).toBeVisible();
    await expectAccessible(page, "sql history empty");

    api.on(SQL_RUNS_PATH, json({ data: [sqlRun()] }));
    await runButton(page).click();
    await expect(history.locator("tbody tr")).toHaveCount(1);
  });
});

test.describe("sql: an ended session", () => {
  const SIGN_IN_URL = `/login/?next=${encodeURIComponent(SQL_URL)}`;

  test("a read that answers 401 sends the browser to sign in, and signing in returns here", async ({
    page,
    api,
  }) => {
    signedIn(api);
    healthy(api);
    api.on(SQL_SCHEMA_PATH, problem(401, "unauthenticated"));
    await page.goto(SQL_URL);
    await expect(page).toHaveURL(SIGN_IN_URL);

    api.on(SQL_SCHEMA_PATH, json(sqlSchema()));
    const logins = capture(api, LOGIN_PATH, json(sessionInfo()));
    await page.getByLabel("Email").fill("ada@example.com");
    await page.getByLabel("Password", { exact: true }).fill("correct horse battery staple");
    await page.getByRole("button", { name: "Sign in", exact: true }).click();
    await expect(page).toHaveURL(SQL_URL);
    await expect(editor(page)).toHaveValue(SQL_EXAMPLE);
    expect(logins).toHaveLength(1);
  });

  test("a run that answers 401 sends the browser to sign in too", async ({ page, api }) => {
    signedIn(api);
    healthy(api);
    api.on(SQL_RUN_PATH, problem(401, "unauthenticated"));
    await open(page);
    await runButton(page).click();
    await expect(page).toHaveURL(SIGN_IN_URL);
  });
});

// The phone project runs only phone.spec.ts, so this surface sets the phone viewport itself.
test.describe("sql: phone at 390 px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test("the panes stack, the grid scrolls inside its frame and the page never scrolls sideways", async ({
    page,
    api,
  }) => {
    healthy(api);
    api.on(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    api.on(SQL_RUNS_PATH, json({ data: [sqlRun()] }));
    api.on(
      SQL_RUN_PATH,
      json(
        sqlResult({
          columns: Array.from({ length: 12 }, (_, index) => ({
            name: `column_${index}`,
            type: "String",
          })),
          rows: [Array.from({ length: 12 }, (_, index) => `a fairly long value ${index}`)],
        }),
      ),
    );
    await open(page);
    const schema = await section(page, "Schema").boundingBox();
    const query = await section(page, "Query").boundingBox();
    expect(schema && query && query.y >= schema.y + schema.height).toBe(true);
    await expectNoPageScrollX(page);
    await runButton(page).tap();
    const grid = section(page, "Result").getByRole("region", { name: "Result rows" });
    await expect(grid.getByRole("table")).toBeVisible();
    expect(await grid.evaluate((node) => node.scrollWidth > node.clientWidth)).toBe(true);
    await expectNoPageScrollX(page);
    await expectAccessible(page, "sql on a phone");
    await page.screenshot({ path: test.info().outputPath("sql-phone.png"), fullPage: true });
  });
});

test.describe("sql: desktop layout", () => {
  test("the schema sits beside the editor", async ({ page, api }) => {
    healthy(api);
    api.on(SQL_SAVED_PATH, json({ data: [savedQuery()] }));
    api.on(SQL_RUNS_PATH, json({ data: [sqlRun()] }));
    api.on(SQL_RUN_PATH, json(sqlResult()));
    await open(page);
    await runButton(page).click();
    await expect(section(page, "Result").getByRole("table")).toBeVisible();
    const schema = await section(page, "Schema").boundingBox();
    const query = await section(page, "Query").boundingBox();
    expect(schema && query && query.x >= schema.x + schema.width).toBe(true);
    await expectNoPageScrollX(page);
    await page.screenshot({ path: test.info().outputPath("sql-desktop.png"), fullPage: true });
  });
});
