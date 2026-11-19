import type { Page } from "@playwright/test";
import type { DeletionRequest } from "../src/api/generated/DeletionRequest";
import type { Retention } from "../src/api/generated/Retention";
import {
  CSRF,
  capture,
  expect,
  expectAccessible,
  expectNoPageScrollX,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const RETENTION_PATH = new RegExp(`/api/v1/projects/${PROJECT}/retention$`);
const REQUESTS_PATH = new RegExp(`/api/v1/projects/${PROJECT}/deletion-requests$`);
const URL = `/settings/retention/?project=${PROJECT}`;
const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";

function retention(fields: Partial<Retention> = {}): Retention {
  return {
    retention_days: null,
    payload_retention_days: null,
    mcp_content_retention_days: null,
    hidden_before: { retention: null, payload: null, mcp_content: null },
    version: 4,
    ...fields,
  };
}

function request(fields: Partial<DeletionRequest> = {}): DeletionRequest {
  return {
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
  };
}

// The permissions a project admin's session lists (docs/architecture/authz-matrix.md).
function admin(permissions = ["read", "manage", "delete_traces", "delete_by_user"]) {
  const session = sessionInfo();
  const [first] = session.projects;
  return { ...session, projects: first ? [{ ...first, permissions }] : [] };
}

const retentionRegion = (page: Page) =>
  page.getByRole("region", { name: "Retention for Checkout" });
const requestsRegion = (page: Page) => page.getByRole("region", { name: "Deletion requests" });

test.describe("settings: retention", () => {
  test("loading, then success: the stored settings, and a longer setting saves with If-Match", async ({
    page,
    api,
  }) => {
    signedIn(api, admin());
    api.on(REQUESTS_PATH, json({ data: [], next_cursor: null }));
    const release = api.hold(RETENTION_PATH, json(retention({ retention_days: 30 })));
    await page.goto(URL);
    const region = retentionRegion(page);
    await expect(region.getByRole("status").first()).toHaveText("Loading retention");
    await expectAccessible(page, "retention loading");
    release();
    await expect(region.getByLabel("Keep data for")).toHaveValue("30");
    await expect(region.getByRole("button", { name: "Save retention" })).toBeDisabled();
    await expectAccessible(page, "retention success");

    const puts = capture(api, RETENTION_PATH, json(retention({ retention_days: 90, version: 5 })));
    await region.getByLabel("Keep data for").fill("90");
    await region.getByRole("button", { name: "Save retention" }).click();
    await expect(region.getByText("Saved. Reads follow the new settings now.")).toBeVisible();
    const put = puts.find((sent) => sent.method() === "PUT");
    expect(put?.headers()["if-match"]).toBe('"4"');
    expect(put?.headers()["x-crucible-csrf"]).toBe(CSRF);
    expect(put?.postDataJSON()).toEqual({
      retention_days: 90,
      payload_retention_days: null,
      mcp_content_retention_days: null,
    });
  });

  test("shortening asks first and says what it does to existing data", async ({ page, api }) => {
    signedIn(api, admin());
    api.on(REQUESTS_PATH, json({ data: [], next_cursor: null }));
    const calls = capture(api, RETENTION_PATH, json(retention()));
    await page.goto(URL);
    const region = retentionRegion(page);
    await region.getByLabel("Keep data for").fill("14");
    await region.getByLabel("Keep inputs and outputs for").fill("30");
    await expect(region.getByText(/can't be longer than the data is kept/)).toBeVisible();
    await region.getByLabel("Keep inputs and outputs for").fill("7 days");
    await region.getByRole("button", { name: "Save retention" }).click();
    const dialog = page.getByRole("alertdialog", { name: "Shorten retention?" });
    await expect(dialog).toContainText("removed from storage within a day");
    await expectAccessible(page, "shorten retention dialog");
    expect(calls.filter((sent) => sent.method() === "PUT")).toHaveLength(0);
    await dialog.getByRole("button", { name: "Shorten retention" }).click();
    await expect(dialog).toBeHidden();
    const put = calls.find((sent) => sent.method() === "PUT");
    expect(put?.postDataJSON()).toEqual({
      retention_days: 14,
      payload_retention_days: 7,
      mcp_content_retention_days: null,
    });
  });

  test("error: the section keeps the failure to itself, with a retry, and shows no server text", async ({
    page,
    api,
  }) => {
    signedIn(api, admin());
    api.on(REQUESTS_PATH, json({ data: [request()], next_cursor: null }));
    api.on(RETENTION_PATH, problem(503, "control_plane_unavailable"));
    await page.goto(URL);
    const region = retentionRegion(page);
    await expect(region.getByRole("button", { name: /Retry/ })).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(requestsRegion(page).getByText("2 traces")).toBeVisible();
    await expectAccessible(page, "retention error");
    api.on(RETENTION_PATH, json(retention()));
    await region.getByRole("button", { name: /Retry/ }).click();
    await expect(region.getByLabel("Keep data for")).toHaveValue("");
  });

  test("empty and read-only: nothing set reads as forever, and a viewer gets no controls", async ({
    page,
    api,
  }) => {
    signedIn(api, admin(["read"]));
    api.on(RETENTION_PATH, json(retention()));
    api.on(REQUESTS_PATH, json({ data: [], next_cursor: null }));
    await page.goto(URL);
    const region = retentionRegion(page);
    await expect(region.getByLabel("Keep data for")).toHaveAttribute("placeholder", "Forever");
    await expect(region.getByLabel("Keep data for")).toHaveAttribute("readonly", "");
    await expect(region.getByText(/Only project admins change retention/)).toBeVisible();
    await expect(region.getByRole("button", { name: "Save retention" })).toHaveCount(0);
    await expect(requestsRegion(page).getByText("No deletion requests yet.")).toBeVisible();
    await expect(page.getByRole("button", { name: "Delete data" })).toHaveCount(0);
    await expectAccessible(page, "retention read-only");
  });

  test("without a control plane the page says which setting turns it on", async ({ page, api }) => {
    await page.goto(URL);
    await expect(page.getByText("Retention needs a control plane.")).toBeVisible();
    await expect(page.getByText("CRUCIBLE_POSTGRES_URL")).toBeVisible();
    expect(api.count(RETENTION_PATH)).toBe(0);
    expect(api.count(REQUESTS_PATH)).toBe(0);
  });
});

test.describe("settings: deletion requests", () => {
  test("loading, then success: each request with its status and progress", async ({
    page,
    api,
  }) => {
    signedIn(api, admin());
    api.on(RETENTION_PATH, json(retention()));
    const release = api.hold(
      REQUESTS_PATH,
      json({
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
            error: SERVER_TEXT,
          }),
        ],
        next_cursor: null,
      }),
    );
    await page.goto(URL);
    const region = requestsRegion(page);
    await expect(region.getByRole("status").first()).toHaveText("Loading deletion requests");
    release();
    await expect(region.locator("tbody tr")).toHaveCount(3);
    await expect(region.getByRole("row", { name: /User ada/ })).toContainText("Deleted");
    await expect(region.getByRole("row", { name: /User ada/ })).toContainText(
      "3 of 3 deletes done, 1 removed from storage",
    );
    await expect(region.getByRole("row", { name: /2 traces/ })).toContainText("Removed");
    await expect(region.getByRole("row", { name: /Session s-9/ })).toContainText("Failed");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "deletion requests success");
    await expectNoPageScrollX(page);
  });

  test("a request that is still running is followed until it ends", async ({ page, api }) => {
    signedIn(api, admin());
    api.on(RETENTION_PATH, json(retention()));
    let state: DeletionRequest["state"] = "pending";
    api.on(REQUESTS_PATH, (route) =>
      route.fulfill(json({ data: [request({ state, verified: false })], next_cursor: null })),
    );
    await page.goto(URL);
    const row = requestsRegion(page).getByRole("row", { name: /2 traces/ });
    await expect(row).toContainText("Hidden");
    state = "completed";
    await expect(row).toContainText("Removed", { timeout: 15_000 });
  });

  test("delete data by trace: checks the ids, sends an idempotency key and lists the request", async ({
    page,
    api,
  }) => {
    signedIn(api, admin());
    api.on(RETENTION_PATH, json(retention()));
    let list: DeletionRequest[] = [];
    const posts: { body: unknown; key: string | undefined }[] = [];
    api.on(REQUESTS_PATH, (route) => {
      if (route.request().method() !== "POST") {
        return route.fulfill(json({ data: list, next_cursor: null }));
      }
      posts.push({
        body: route.request().postDataJSON(),
        key: route.request().headers()["idempotency-key"],
      });
      list = [request({ state: "pending", verified: false, trace_count: 1 })];
      return route.fulfill(json(list[0], 201));
    });
    await page.goto(URL);
    await expect(page.getByText("No deletion requests yet.")).toBeVisible();
    await expectAccessible(page, "deletion requests empty");
    await page.getByRole("button", { name: "Delete data" }).click();
    const dialog = page.getByRole("dialog", { name: "Delete data from Checkout" });
    await expect(dialog).toContainText("This can't be undone.");
    await expectAccessible(page, "delete data dialog");

    await dialog.getByRole("button", { name: "Delete data" }).click();
    await expect(dialog.getByText("Enter at least one trace id.")).toBeVisible();
    await dialog.getByLabel("Trace ids").fill("nope");
    await dialog.getByRole("button", { name: "Delete data" }).click();
    await expect(dialog.getByText(/isn't a trace id/)).toBeVisible();
    expect(posts).toHaveLength(0);

    await dialog.getByLabel("Trace ids").fill(`${TRACE.toUpperCase()}, ${TRACE}`);
    await dialog.getByRole("button", { name: "Delete data" }).click();
    await expect(dialog).toBeHidden();
    await expect(requestsRegion(page).getByRole("row", { name: /1 trace/ })).toContainText(
      "Hidden",
    );
    expect(posts[0]?.body).toEqual({ kind: "trace", trace_ids: [TRACE] });
    expect(posts[0]?.key).toMatch(/^[0-9a-f-]{36}$/);
  });

  test("delete data by user and by session, for admins only", async ({ page, api }) => {
    signedIn(api, admin());
    api.on(RETENTION_PATH, json(retention()));
    const posts: unknown[] = [];
    api.on(REQUESTS_PATH, (route) => {
      if (route.request().method() !== "POST") {
        return route.fulfill(json({ data: [], next_cursor: null }));
      }
      posts.push(route.request().postDataJSON());
      return route.fulfill(json(request({ kind: "user", user_id: "ada", state: "pending" }), 201));
    });
    await page.goto(URL);
    for (const [choice, label, value, body] of [
      ["A user", "User id", "ada", { kind: "user", user_id: "ada" }],
      ["A session", "Session id", "s-9", { kind: "session", session_id: "s-9" }],
    ] as const) {
      await page.getByRole("button", { name: "Delete data" }).click();
      const dialog = page.getByRole("dialog", { name: "Delete data from Checkout" });
      await dialog.getByRole("radio", { name: choice }).click();
      await dialog.getByRole("button", { name: "Delete data" }).click();
      await expect(
        dialog.getByText(new RegExp(`Enter the ${label.toLowerCase()} to delete`)),
      ).toBeVisible();
      await dialog.getByLabel(label).fill(value);
      await dialog.getByRole("button", { name: "Delete data" }).click();
      await expect(dialog).toBeHidden();
      expect(posts.at(-1)).toEqual(body);
    }
  });

  test("a member deletes traces only, and a failed request stays in the dialog", async ({
    page,
    api,
  }) => {
    signedIn(api, admin(["read", "delete_traces"]));
    api.on(RETENTION_PATH, json(retention()));
    api.on(REQUESTS_PATH, (route) =>
      route.request().method() === "POST"
        ? route.fulfill(problem(503, "control_plane_unavailable"))
        : route.fulfill(json({ data: [], next_cursor: null })),
    );
    await page.goto(URL);
    await page.getByRole("button", { name: "Delete data" }).click();
    const dialog = page.getByRole("dialog", { name: "Delete data from Checkout" });
    await expect(dialog.getByRole("radio")).toHaveCount(0);
    await expect(dialog).toContainText("Deleting by user or session needs a project admin.");
    await dialog.getByLabel("Trace ids").fill(TRACE);
    await dialog.getByRole("button", { name: "Delete data" }).click();
    await expect(dialog.getByRole("alert")).toContainText("Crucible can't reach its database.");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(dialog).toBeVisible();
  });

  test("error: the list keeps the failure to itself, with a retry", async ({ page, api }) => {
    signedIn(api, admin());
    api.on(RETENTION_PATH, json(retention({ retention_days: 30 })));
    api.on(REQUESTS_PATH, problem(503, "control_plane_unavailable"));
    await page.goto(URL);
    const region = requestsRegion(page);
    await expect(region.getByRole("button", { name: /Retry/ })).toBeVisible();
    await expect(retentionRegion(page).getByLabel("Keep data for")).toHaveValue("30");
    await expectAccessible(page, "deletion requests error");
    api.on(REQUESTS_PATH, json({ data: [request()], next_cursor: null }));
    await region.getByRole("button", { name: /Retry/ }).click();
    await expect(region.getByText("2 traces")).toBeVisible();
  });
});
