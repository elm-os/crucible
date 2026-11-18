import {
  expect,
  expectAccessible,
  json,
  LIST_PATH,
  listPage,
  PROJECT,
  problem,
  SERVER_TEXT,
  SESSION,
  SESSION_PATH,
  SESSIONS_PATH,
  SPANS_PATH,
  sessionItem,
  spanListItem,
  test,
  traceItem,
  tracePage,
  USER,
  USER_PATH,
  USER_SESSIONS_PATH,
  USERS_PATH,
  userItem,
  userSessionItem,
} from "./fixtures";

const AS_OF = "2026-11-17T14:00:05.000000Z";
const WHOLE = { as_of: AS_OF, partial: false, consistency: "strong" } as const;
const LAGGING = { as_of: AS_OF, partial: false, consistency: "lagging" } as const;

test.describe("sessions: list", () => {
  const url = `/sessions/?project=${PROJECT}`;

  test("loading: skeleton rows after 300 ms, announced", async ({ page, api }) => {
    const release = api.hold(SESSIONS_PATH, json(listPage([sessionItem()])));
    await page.goto(url);
    const section = page.getByRole("region", { name: "Sessions" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    await expect(section.getByRole("status")).toHaveText("Loading sessions");
    await expectAccessible(page, "sessions loading");
    release();
    await expect(section.getByRole("table")).toBeVisible();
  });

  test("success: one row per session linking to it; Load more pages with the first window", async ({
    page,
    api,
  }) => {
    api.on(SESSIONS_PATH, (route, requestUrl) =>
      route.fulfill(
        requestUrl.searchParams.get("cursor") === "c-2"
          ? json(listPage([sessionItem({ session_id: "chat-7" })]))
          : json(listPage([sessionItem()], "c-2")),
      ),
    );
    await page.goto(url);
    const table = page.getByRole("table");
    await expect(table.getByRole("columnheader")).toHaveText([
      "Last seen",
      "Session",
      "Traces",
      "Spans",
      "Cost",
      "Tokens",
      "Status",
    ]);
    await expect(table.getByRole("link", { name: SESSION })).toHaveAttribute(
      "href",
      new RegExp(`session/?\\?project=${PROJECT}&session=${SESSION}`),
    );
    await expectAccessible(page, "sessions success");
    await page.getByRole("button", { name: "Load more sessions" }).click();
    await expect(table.getByRole("link", { name: "chat-7" })).toBeVisible();
    const [first, second] = api.calls.map((call) => new URL(`http://localhost${call}`));
    expect(second?.searchParams.get("from")).toBe(first?.searchParams.get("from"));
    expect(second?.searchParams.get("to")).toBe(first?.searchParams.get("to"));
  });

  test("error: an in-section card with the reference and a retry of the list only", async ({
    page,
    api,
  }) => {
    api.on(SESSIONS_PATH, problem(503, "telemetry_store_unavailable", "req-sessions"));
    await page.goto(url);
    const section = page.getByRole("region", { name: "Sessions" });
    await expect(section.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(section.getByText("req-sessions")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "sessions error");
    api.on(SESSIONS_PATH, json(listPage([sessionItem()])));
    await section.getByRole("button", { name: "Retry sessions" }).click();
    await expect(section.getByRole("table")).toBeVisible();
  });

  test("empty: the window offers 30 days, which invites the first session; incomplete offers to check again", async ({
    page,
    api,
  }) => {
    api.on(SESSIONS_PATH, json(listPage([])));
    await page.goto(url);
    await expect(page.getByText("No sessions in the last 24 hours.")).toBeVisible();
    await expectAccessible(page, "sessions empty");
    await page.getByRole("button", { name: "Show last 30 days" }).click();
    await expect(page.getByText("No sessions in the last 30 days.")).toBeVisible();
    api.on(SESSIONS_PATH, json({ ...listPage([]), meta: LAGGING }));
    await page.reload();
    await expect(page.getByText("No sessions to show yet.")).toBeVisible();
  });
});

test.describe("session: one session", () => {
  const url = `/session/?project=${PROJECT}&session=${SESSION}`;

  test("success: the summary and the session's traces, each in its own section", async ({
    page,
    api,
  }) => {
    api.on(SESSION_PATH, json({ session: sessionItem(), meta: WHOLE }));
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(url);
    const summary = page.getByRole("region", { name: "Summary" });
    await expect(summary).toContainText("1 error");
    await expect(summary.getByRole("link", { name: USER })).toBeVisible();
    const traces = page.getByRole("region", { name: "Traces in this session" });
    await expect(traces.getByRole("link", { name: "support agent" })).toBeVisible();
    const request = api.calls.find((call) => call.split("?")[0]?.endsWith("/traces"));
    expect(new URL(`http://localhost${request}`).searchParams.getAll("session_id")).toEqual([
      SESSION,
    ]);
    await expectAccessible(page, "session success");
  });

  test("error and empty: a missing session fails its summary only; an incomplete one says so", async ({
    page,
    api,
  }) => {
    api.on(SESSION_PATH, problem(404, "not_found"));
    api.on(LIST_PATH, json(tracePage([])));
    await page.goto(url);
    await expect(page.getByText("This isn't in this project.")).toBeVisible();
    await expect(page.getByText("No traces of this session in the last 24 hours.")).toBeVisible();
    await expectAccessible(page, "session error");
    api.on(SESSION_PATH, json({ session: null, meta: LAGGING }));
    await page.reload();
    await expect(page.getByText("This session has no data to show yet.")).toBeVisible();
  });
});

test.describe("users", () => {
  test("list: loading, success with session counts, error, empty", async ({ page, api }) => {
    const release = api.hold(USERS_PATH, json(listPage([userItem()])));
    await page.goto(`/users/?project=${PROJECT}`);
    const section = page.getByRole("region", { name: "Users" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    release();
    await expect(section.getByRole("link", { name: USER })).toBeVisible();
    await expect(section.getByRole("row").nth(1)).toContainText("2");
    await expectAccessible(page, "users success");

    api.on(USERS_PATH, problem(500, "internal_error", "req-users"));
    await page.reload();
    await expect(section.getByText("req-users")).toBeVisible({ timeout: 10_000 });

    api.on(USERS_PATH, json(listPage([])));
    await section.getByRole("button", { name: "Retry users" }).click();
    await expect(page.getByText("No users in the last 24 hours.")).toBeVisible();
  });

  test("one user: summary, sessions and traces; a failing section leaves the others standing", async ({
    page,
    api,
  }) => {
    api.on(USER_PATH, json({ user: userItem(), meta: WHOLE }));
    api.on(USER_SESSIONS_PATH, problem(503, "telemetry_store_unavailable"));
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(`/user/?project=${PROJECT}&user=${USER}`);
    await expect(page.getByRole("region", { name: "Summary" })).toContainText("Sessions");
    const sessions = page.getByRole("region", { name: "Sessions of this user" });
    await expect(sessions.getByText("This section couldn't load.")).toBeVisible({
      timeout: 10_000,
    });
    await expect(
      page.getByRole("region", { name: "Traces of this user" }).getByRole("link", {
        name: "support agent",
      }),
    ).toBeVisible();
    await expectAccessible(page, "user one section down");
    api.on(USER_SESSIONS_PATH, json(listPage([userSessionItem()])));
    await sessions.getByRole("button", { name: "Retry sessions of this user" }).click();
    await expect(sessions.getByRole("link", { name: SESSION })).toBeVisible();
  });
});

test.describe("spans: list and filters", () => {
  test("loading, then rows that open the span in its trace; filters live in the URL", async ({
    page,
    api,
  }) => {
    const release = api.hold(SPANS_PATH, json(listPage([spanListItem()])));
    await page.goto(`/spans/?project=${PROJECT}&level=error`);
    const section = page.getByRole("region", { name: "Spans" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    release();
    const link = section.getByRole("link", { name: "send_reply" });
    await expect(link).toHaveAttribute(
      "href",
      /trace\/?\?project=.*&trace=.*&span=a000000000000008/,
    );
    await expect(page.getByRole("button", { name: "Remove filter Level: error" })).toBeVisible();
    await expectAccessible(page, "spans success");

    await page.getByRole("textbox", { name: "Value" }).fill("send");
    await page.getByRole("button", { name: "Add filter" }).click();
    await expect(page).toHaveURL(/name=send/);
    const last = new URL(`http://localhost${api.calls.at(-1)}`);
    expect(last.searchParams.getAll("level")).toEqual(["error"]);
    expect(last.searchParams.get("name")).toBe("send");

    await page.getByRole("button", { name: "Remove filter Level: error" }).click();
    await expect(page).not.toHaveURL(/level=error/);
  });

  test("error and empty: a refused filter is mapped; no match offers to clear the filters", async ({
    page,
    api,
  }) => {
    api.on(SPANS_PATH, problem(422, "query_too_broad"));
    await page.goto(`/spans/?project=${PROJECT}&tags=rare`);
    await expect(page.getByText("The range is longer than this project allows.")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "spans error");
    api.on(SPANS_PATH, json(listPage([])));
    await page.reload();
    await expect(
      page.getByText("No spans match these filters in the last 24 hours."),
    ).toBeVisible();
    await page
      .getByRole("region", { name: "Spans" })
      .getByRole("button", { name: "Clear filters" })
      .click();
    await expect(page).not.toHaveURL(/tags=/);
  });
});

test.describe("traces: filters and live tail", () => {
  test("the filter bar narrows the list, and an empty filtered list offers to clear", async ({
    page,
    api,
  }) => {
    api.on(LIST_PATH, (route, requestUrl) =>
      route.fulfill(
        requestUrl.searchParams.has("environment")
          ? json(tracePage([]))
          : json(tracePage([traceItem()])),
      ),
    );
    await page.goto(`/traces/?project=${PROJECT}`);
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
    await page.getByRole("combobox", { name: "Filter by" }).click();
    await page.getByRole("option", { name: "Environment" }).click();
    await page.getByRole("textbox", { name: "Value" }).fill("staging");
    await page.getByRole("button", { name: "Add filter" }).click();
    await expect(page).toHaveURL(/environment=staging/);
    await expect(
      page.getByText("No traces match these filters in the last 24 hours."),
    ).toBeVisible();
    await expectAccessible(page, "traces filtered empty");
    await page
      .getByRole("region", { name: "Recent traces" })
      .getByRole("button", { name: "Clear filters" })
      .click();
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
  });

  test("live tail: new traces after the head cursor show as a button that refreshes the list", async ({
    page,
    api,
  }) => {
    let reads = 0;
    api.on(LIST_PATH, (route, requestUrl) => {
      if (requestUrl.searchParams.has("since")) {
        return route.fulfill(
          json({ ...tracePage([]), head_cursor: "h-tail", new_count: 4, has_more: true }),
        );
      }
      reads += 1;
      return route.fulfill(
        json(
          tracePage([traceItem({ trace_name: `support agent ${reads}` })], {
            head_cursor: `h-${reads}`,
          }),
        ),
      );
    });
    await page.goto(`/traces/?project=${PROJECT}`);
    const show = page.getByRole("button", { name: "Show 4 new traces" });
    await expect(show).toBeVisible();
    const tail = new URL(`http://localhost${api.calls.find((call) => call.includes("since="))}`);
    expect(tail.searchParams.get("since")).toBe("h-1");
    await expectAccessible(page, "traces live tail");
    await show.click();
    await expect(page.getByRole("link", { name: "support agent 2" })).toBeVisible();
  });
});
