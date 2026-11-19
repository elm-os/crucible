import {
  AS_OF,
  expect,
  expectAccessible,
  json,
  LIST_PATH,
  PROJECT,
  problem,
  SERVER_TEXT,
  TRACE,
  test,
  traceItem,
  tracePage,
} from "./fixtures";

const LIST_URL = `/traces/?project=${PROJECT}`;

test.describe("traces: recent traces", () => {
  test("loading: skeleton rows after 300 ms, announced to screen readers", async ({
    page,
    api,
  }) => {
    const release = api.hold(LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(LIST_URL);
    const section = page.getByRole("region", { name: "Recent traces" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    await expect(section.getByRole("status")).toHaveText("Loading recent traces");
    await expectAccessible(page, "recent traces loading");
    release();
    await expect(section.getByRole("table")).toBeVisible();
  });

  test("success: one row per trace, linking to the trace with the project", async ({
    page,
    api,
  }) => {
    api.on(
      LIST_PATH,
      json(
        tracePage([
          traceItem(),
          traceItem({
            trace_id: "a".repeat(32),
            trace_name: "",
            name: "POST /checkout",
            error_count: 0,
          }),
        ]),
      ),
    );
    await page.goto(LIST_URL);
    const table = page.getByRole("table");
    await expect(table.getByRole("columnheader")).toHaveText([
      "Started",
      "Name",
      "Duration",
      "Spans",
      "Cost",
      "Status",
    ]);
    await expect(table.locator("tbody tr")).toHaveCount(2);
    await expect(table.locator("tbody tr").first()).toContainText("1 error");
    await expect(table.getByRole("link", { name: "POST /checkout" })).toBeVisible();
    const link = table.getByRole("link", { name: "support agent" });
    const href = new URL((await link.getAttribute("href")) ?? "", "http://localhost");
    expect(href.pathname).toMatch(/^\/trace\/?$/);
    expect(Object.fromEntries(href.searchParams)).toEqual({ project: PROJECT, trace: TRACE });
    const listCall = api.calls.find((call) => LIST_PATH.test(call.split("?")[0] ?? ""));
    const request = new URL(`http://localhost${listCall}`);
    expect([...request.searchParams.keys()]).toEqual(["from", "to"]);
    await expectAccessible(page, "recent traces success");
  });

  test("error: a failed list is an in-section error whose retry refetches only the list", async ({
    page,
    api,
  }) => {
    api.on(LIST_PATH, problem(503, "telemetry_store_unavailable", "req-list"));
    await page.goto(LIST_URL);
    const section = page.getByRole("region", { name: "Recent traces" });
    await expect(section.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(section.getByText("req-list")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    expect(api.count(LIST_PATH)).toBe(2);
    await expectAccessible(page, "recent traces error");

    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await section.getByRole("button", { name: "Retry recent traces" }).click();
    await expect(section.getByRole("table")).toBeVisible();
    expect(api.count(LIST_PATH)).toBe(3);
  });

  test("error: without a key the section asks to connect, and offers the dialog", async ({
    page,
    api,
  }) => {
    api.on(LIST_PATH, problem(401, "unauthenticated"));
    await page.goto(LIST_URL);
    await expect(page.locator('[data-error="unauthenticated"]')).toContainText(
      "Connect to a project to see this.",
    );
    await expect(
      page.getByRole("region", { name: "Recent traces" }).getByRole("button", { name: "Connect" }),
    ).toBeVisible();
    await expectAccessible(page, "recent traces unauthenticated");
  });

  test("empty: an empty window offers a wider range; the widest invites the first trace", async ({
    page,
    api,
  }) => {
    api.on(LIST_PATH, json(tracePage([])));
    await page.goto(LIST_URL);
    await expect(page.getByText("No traces in the last 24 hours.")).toBeVisible();
    await expectAccessible(page, "recent traces empty");
    await page.getByRole("button", { name: "Show last 30 days" }).click();
    await expect(page.getByText("No traces in the last 30 days.")).toBeVisible();
    await expect(page).toHaveURL(/range=30d/);
  });

  test("empty: an empty page that may be incomplete offers to check again", async ({
    page,
    api,
  }) => {
    api.on(
      LIST_PATH,
      json(tracePage([], { meta: { as_of: AS_OF, partial: false, consistency: "lagging" } })),
    );
    await page.goto(LIST_URL);
    await expect(page.getByText("No traces to show yet.")).toBeVisible();
    await expect(page.getByText(/No traces in/)).toHaveCount(0);
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await page.getByRole("button", { name: "Check again" }).click();
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
  });

  test("keyboard: a trace link opens the trace page", async ({ page, api }) => {
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    api.on(
      new RegExp(`/traces/${TRACE}$`),
      json({
        trace_id: TRACE,
        span_count: 0,
        paged: false,
        spans: [],
        meta: { as_of: AS_OF, partial: false, consistency: "lagging" },
        saturated: false,
      }),
    );
    await page.goto(LIST_URL);
    await page.getByRole("link", { name: "support agent" }).focus();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(new RegExp(`/trace/\\?project=${PROJECT}&trace=${TRACE}`));
  });
});

test.describe("shell: the Connect dialog", () => {
  test("keyboard: Enter opens it, Escape closes it and focus returns to its trigger", async ({
    page,
    api,
  }) => {
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(LIST_URL);
    const trigger = page.getByRole("banner").getByRole("button", { name: "Connect" });
    await trigger.focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Connect to a project" });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByLabel("Project id")).toBeFocused();
    await expectAccessible(page, "connect dialog open");
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(trigger).toBeFocused();
  });
});
