import type { Page } from "@playwright/test";
import type { Export } from "../src/api/generated/Export";
import {
  CSRF,
  expect,
  expectAccessible,
  json,
  PROJECT,
  PROJECT_B,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const EXPORTS_URL = `/settings/exports/?project=${PROJECT}`;
const EXPORTS_PATH = new RegExp(`/api/v1/projects/${PROJECT}/exports$`);
const DOWNLOAD = `/api/v1/projects/${PROJECT}/exports/e-1/download`;

function item(fields: Partial<Export>): Export {
  return {
    id: "e-1",
    format: "crucible-jsonl",
    state: "ready",
    from: "2026-11-16T14:00:00.000000Z",
    to: "2026-11-17T14:00:00.000000Z",
    trace_count: 0,
    gzip: false,
    keep_truncated: false,
    created_at: "2026-11-17T14:00:05.000000Z",
    as_of: "2026-11-17T14:00:05.000000Z",
    spans: 1200,
    skipped: 0,
    bytes: 2_500_000,
    checksum: "ab".repeat(32),
    expires_at: "2026-11-24T14:01:00.000000Z",
    download_path: DOWNLOAD,
    attempts: 1,
    saturated: false,
    ...fields,
  };
}

const WRITING = { download_path: undefined, checksum: undefined, expires_at: undefined };

// The owner's project with the permission to create exports, as the server lists it for a project admin.
function exporter() {
  const session = sessionInfo();
  const [checkout, search] = session.projects;
  return sessionInfo({
    projects: [{ ...checkout, permissions: [...checkout.permissions, "export"] }, search],
  });
}

function region(page: Page) {
  return page.getByRole("region", { name: "Exports of Checkout" });
}

test.describe("settings: exports", () => {
  test("loading, then success: each export with its state, and a download once ready", async ({
    page,
    api,
  }) => {
    signedIn(api, exporter());
    const release = api.hold(
      EXPORTS_PATH,
      json({
        data: [
          item({}),
          item({ id: "e-2", state: "failed", error_code: "query_timeout", spans: 0, ...WRITING }),
          item({ id: "e-3", state: "expired", ...WRITING }),
        ],
      }),
    );
    await page.goto(EXPORTS_URL);
    await expect(region(page).getByRole("status")).toHaveText("Loading Exports");
    await expectAccessible(page, "exports loading");
    release();
    const rows = region(page).locator("tbody tr");
    await expect(rows).toHaveCount(3);
    await expect(rows.nth(0)).toContainText("Ready");
    await expect(rows.nth(0).getByRole("link", { name: /Download/ })).toHaveAttribute(
      "href",
      DOWNLOAD,
    );
    await expect(rows.nth(1)).toContainText("Failed");
    await expect(rows.nth(1)).toContainText("Reading the spans took too long.");
    await expect(rows.nth(2)).toContainText("Expired");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "exports success");
  });

  test("start: the export is queued with one key and the session's CSRF token, then shows its progress", async ({
    page,
    api,
  }) => {
    signedIn(api, exporter());
    const posts: { body: string; key?: string; csrf?: string }[] = [];
    let polls = 0;
    api.on(EXPORTS_PATH, (route) => {
      const request = route.request();
      if (request.method() === "POST") {
        posts.push({
          body: request.postData() ?? "",
          key: request.headers()["idempotency-key"],
          csrf: request.headers()["x-crucible-csrf"],
        });
        return route.fulfill(json(item({ state: "queued", spans: 0, ...WRITING }), 202));
      }
      if (posts.length === 0) return route.fulfill(json({ data: [] }));
      polls += 1;
      const state =
        polls < 2
          ? item({ state: "running", spans: 600, ...WRITING })
          : item({ state: "ready", spans: 1200 });
      return route.fulfill(json({ data: [state] }));
    });
    await page.goto(EXPORTS_URL);
    await expect(page.getByText("No exports yet.")).toBeVisible();
    await page.getByRole("button", { name: "Export spans" }).click();
    const dialog = page.getByRole("dialog", { name: "Export Checkout's spans" });
    await expectAccessible(page, "export dialog");
    await dialog.getByRole("radio", { name: "OTLP JSON" }).click();
    await dialog.getByLabel("Compress the file with gzip").check();
    await dialog.getByRole("button", { name: "Start export" }).click();
    await expect(dialog).toBeHidden();
    await expect(region(page)).toContainText("Writing, 600 spans so far");
    await expect(region(page).getByRole("link", { name: /Download/ })).toBeVisible({
      timeout: 10_000,
    });
    expect(posts).toHaveLength(1);
    expect(JSON.parse(posts[0]?.body ?? "{}")).toMatchObject({ format: "otlp-json", gzip: true });
    expect(posts[0]?.key).toMatch(/^[0-9a-f]{32}$/);
    expect(posts[0]?.csrf).toBe(CSRF);
  });

  test("a refused start says what to do, in the dialog", async ({ page, api }) => {
    signedIn(api, exporter());
    api.on(EXPORTS_PATH, (route) =>
      route.request().method() === "POST"
        ? route.fulfill(problem(429, "rate_limited"))
        : route.fulfill(json({ data: [item({})] })),
    );
    await page.goto(EXPORTS_URL);
    await page.getByRole("button", { name: "Export spans" }).click();
    const dialog = page.getByRole("dialog", { name: "Export Checkout's spans" });
    await dialog.getByRole("button", { name: "Start export" }).click();
    await expect(dialog.getByRole("alert")).toContainText("A project can start 5 exports an hour.");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
  });

  test("error: a failed list says so, and its retry works", async ({ page, api }) => {
    signedIn(api, exporter());
    api.on(EXPORTS_PATH, problem(503, "control_plane_unavailable", "req-exports"));
    await page.goto(EXPORTS_URL);
    const exports = region(page);
    await expect(exports.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(exports.getByText("req-exports")).toBeVisible();
    await expectAccessible(page, "exports error");
    api.on(EXPORTS_PATH, json({ data: [item({})] }));
    await exports.getByRole("button", { name: "Retry Exports" }).click();
    await expect(exports.locator("tbody tr")).toHaveCount(1);
  });

  test("empty: a viewer sees the empty list without the control to start an export", async ({
    page,
    api,
  }) => {
    signedIn(api, exporter());
    api.on(new RegExp(`/api/v1/projects/${PROJECT_B}/exports$`), json({ data: [] }));
    await page.goto(`/settings/exports/?project=${PROJECT_B}`);
    const exports = page.getByRole("region", { name: "Exports of Search" });
    await expect(exports.getByText("No exports yet.")).toBeVisible();
    await expect(exports).toContainText("Members and admins of Search can export");
    await expect(page.getByRole("button", { name: "Export spans" })).toHaveCount(0);
    await expectAccessible(page, "exports empty");
  });
});
