import type { Page } from "@playwright/test";
import type { AlertEvent } from "../src/api/generated/AlertEvent";
import type { AlertPreview } from "../src/api/generated/AlertPreview";
import type { AlertRule } from "../src/api/generated/AlertRule";
import type { Channel } from "../src/api/generated/Channel";
import {
  type Api,
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

const RULE_ID = "0193a1b2-0000-7000-8000-00000000a001";
const CHANNEL_ID = "0193a1b2-0000-7000-8000-00000000c001";
const RULES_PATH = new RegExp(`/projects/${PROJECT}/alerts$`);
const RULE_PATH = new RegExp(`/projects/${PROJECT}/alerts/${RULE_ID}$`);
const HISTORY_PATH = new RegExp(`/projects/${PROJECT}/alerts/${RULE_ID}/history$`);
const PREVIEW_PATH = new RegExp(`/projects/${PROJECT}/alerts/preview$`);
const CHANNELS_PATH = new RegExp(`/projects/${PROJECT}/channels$`);
const CHANNEL_PATH = new RegExp(`/projects/${PROJECT}/channels/${CHANNEL_ID}$`);
const TEST_PATH = new RegExp(`/projects/${PROJECT}/channels/${CHANNEL_ID}/test$`);

const RULES_URL = `/alerts/?project=${PROJECT}`;
const RULE_URL = `/alert/?project=${PROJECT}&id=${RULE_ID}`;
const CHANNELS_URL = `/alerts/channels/?project=${PROJECT}`;

// An admin of Checkout, as the server lists an admin's permissions.
function admin(api: Api): void {
  const session = sessionInfo();
  const [checkout, search] = session.projects;
  signedIn(api, {
    ...session,
    projects: [
      {
        ...checkout,
        permissions: ["read", "write_facts", "edit_content", "channels", "manage"],
      },
      search,
    ],
  });
}

function rule(fields: Partial<AlertRule> = {}): AlertRule {
  return {
    id: RULE_ID,
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
    channels: [CHANNEL_ID],
    status: "firing",
    series: [
      {
        key: [{ dimension: "environment", value: "prod" }],
        status: "firing",
        since: "2026-11-17T13:50:00.000000Z",
        value: 12,
        last_notified_at: "2026-11-17T13:50:00.000000Z",
      },
      {
        key: [{ dimension: "environment", value: "dev" }],
        status: "ok",
        since: "2026-11-17T13:40:00.000000Z",
        value: 1,
      },
    ],
    last_evaluated_at: "2026-11-17T14:00:00.000000Z",
    eval_failures: 0,
    skipped_ticks: 0,
    created_at: "2026-11-17T12:00:00.000000Z",
    updated_at: "2026-11-17T12:00:00.000000Z",
    saturated: false,
    version: 3,
    ...fields,
  };
}

function preview(fields: Partial<AlertPreview> = {}): AlertPreview {
  return {
    from: "2026-11-17T13:55:00.000000Z",
    to: "2026-11-17T14:00:00.000000Z",
    unit: "count",
    series: [
      { key: [{ dimension: "environment", value: "prod" }], value: 12, breach: true },
      { key: [{ dimension: "environment", value: "dev" }], value: 1, breach: false },
    ],
    truncated: false,
    saturated: false,
    ...fields,
  };
}

function event(n: number, fields: Partial<AlertEvent> = {}): AlertEvent {
  return {
    id: `0193a1b2-0000-7000-8000-0000000e00${String(n).padStart(2, "0")}`,
    kind: "transition",
    key: [{ dimension: "environment", value: "prod" }],
    from_status: "ok",
    to_status: "firing",
    value: 12,
    notified: true,
    at: `2026-11-17T13:${String(50 - n).padStart(2, "0")}:00.000000Z`,
    ...fields,
  };
}

function channel(fields: Partial<Channel> = {}): Channel {
  return {
    id: CHANNEL_ID,
    name: "On call",
    kind: "webhook",
    enabled: true,
    failures: 0,
    url: "http://10.0.0.8:9000/hook",
    to: [],
    version: 2,
    ...fields,
  };
}

const rulesRegion = (page: Page) => page.getByRole("region", { name: "Alert rules for Checkout" });
const channelsRegion = (page: Page) => page.getByRole("region", { name: "Channels for Checkout" });

test.describe("alerts: rules", () => {
  test("loading, then success: each rule with its state and condition", async ({ page, api }) => {
    admin(api);
    const release = api.hold(
      RULES_PATH,
      json({
        data: [
          rule(),
          rule({ id: "r-2", name: "Slow replies", enabled: false, status: "ok", series: [] }),
        ],
      }),
    );
    await page.goto(RULES_URL);
    const rules = rulesRegion(page);
    await expect(rules.getByRole("status")).toHaveText("Loading alert rules");
    await expectAccessible(page, "rules loading");
    release();
    await expect(rules.locator("tbody tr")).toHaveCount(2);
    const first = rules.getByRole("row", { name: /Checkout errors/ });
    await expect(first).toContainText("Firing");
    await expect(first).toContainText("Errors above 5 over 5 min");
    await expect(first).toContainText("every 1 min");
    await expect(rules.getByRole("row", { name: /Slow replies/ })).toContainText("Turned off");
    await expectAccessible(page, "rules success");
    await expectNoPageScrollX(page);
  });

  test("empty: the first-use state offers New rule", async ({ page, api }) => {
    admin(api);
    api.on(RULES_PATH, json({ data: [] }));
    await page.goto(RULES_URL);
    await expect(page.getByText("No alert rules yet.")).toBeVisible();
    await expect(rulesRegion(page).getByRole("button", { name: "New rule" })).toBeVisible();
    await expectAccessible(page, "rules empty");
  });

  test("error: a failed list says so in the page's words, and its retry works", async ({
    page,
    api,
  }) => {
    admin(api);
    api.on(RULES_PATH, problem(503, "control_plane_unavailable", "req-rules"));
    await page.goto(RULES_URL);
    const rules = rulesRegion(page);
    await expect(rules.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(rules.getByText("req-rules")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "rules error");
    api.on(RULES_PATH, json({ data: [rule()] }));
    await rules.getByRole("button", { name: "Retry alert rules" }).click();
    await expect(rules.locator("tbody tr")).toHaveCount(1);
  });

  test("create: preview first, then the rule is sent in stored units", async ({ page, api }) => {
    admin(api);
    const posts: string[] = [];
    api.on(RULES_PATH, (route) => {
      if (route.request().method() !== "POST") return route.fulfill(json({ data: [] }));
      posts.push(route.request().postData() ?? "");
      return route.fulfill(json(rule(), 201));
    });
    api.on(CHANNELS_PATH, json({ data: [channel()] }));
    const previews = capture(api, PREVIEW_PATH, json(preview()));
    await page.goto(RULES_URL);
    await page.getByRole("button", { name: "New rule" }).click();
    const dialog = page.getByRole("dialog", { name: "New alert rule for Checkout" });
    await dialog.getByLabel("Name").fill("Checkout errors");
    await dialog.getByLabel("Threshold").fill("5");
    await dialog.getByLabel("Over the last (minutes)").fill("5");
    await dialog.getByRole("checkbox", { name: /On call/ }).click();
    await expectAccessible(page, "rule form");

    await dialog.getByRole("button", { name: "Preview" }).click();
    await expect(dialog.getByText("1 series would breach now.")).toBeVisible();
    await expect(dialog.getByRole("row", { name: /environment=prod/ })).toContainText("Breaches");
    expect(previews[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
    await expectAccessible(page, "rule form with preview");

    await dialog.getByRole("button", { name: "Create rule" }).click();
    await expect(dialog).toBeHidden();
    expect(JSON.parse(posts[0] ?? "{}")).toMatchObject({
      name: "Checkout errors",
      measure: "error_count",
      aggregation: "sum",
      comparison: "gt",
      threshold: 5,
      window_secs: 300,
      interval_secs: 60,
      channels: [CHANNEL_ID],
    });
  });

  test("create: a threshold that is not a number is caught before sending", async ({
    page,
    api,
  }) => {
    admin(api);
    api.on(RULES_PATH, json({ data: [] }));
    api.on(CHANNELS_PATH, json({ data: [] }));
    await page.goto(RULES_URL);
    await page.getByRole("button", { name: "New rule" }).click();
    const dialog = page.getByRole("dialog", { name: "New alert rule for Checkout" });
    await dialog.getByLabel("Name").fill("Errors");
    await dialog.getByLabel("Threshold").fill("five");
    await dialog.getByRole("button", { name: "Create rule" }).click();
    await expect(dialog.getByText("Enter a number, like 5 or 0.25.")).toBeVisible();
    await expect(dialog.getByText(/No channels yet, so this rule records its state/)).toBeVisible();
    expect(api.calls.filter((call) => call.endsWith("/alerts")).length).toBe(1);
  });

  test("a viewer reads the rules and gets no New rule", async ({ page, api }) => {
    signedIn(api);
    api.on(new RegExp(`/projects/${PROJECT_B}/alerts$`), json({ data: [rule()] }));
    await page.goto(`/alerts/?project=${PROJECT_B}`);
    const rules = page.getByRole("region", { name: "Alert rules for Search" });
    await expect(rules.getByRole("row", { name: /Checkout errors/ })).toBeVisible();
    await expect(page.getByRole("button", { name: "New rule" })).toHaveCount(0);
  });

  test("without a control plane the page says which setting adds alerts, and asks for nothing", async ({
    page,
    api,
  }) => {
    await page.goto(RULES_URL);
    await expect(page.getByText("Alerts need a control plane.")).toBeVisible();
    expect(api.calls.filter((call) => call.includes("/alerts"))).toEqual([]);
    await expectAccessible(page, "alerts without a control plane");
  });
});

function serveRule(api: Api, fields: Partial<AlertRule> = {}): void {
  admin(api);
  api.on(RULE_PATH, json(rule(fields)));
  api.on(PREVIEW_PATH, json(preview()));
  api.on(HISTORY_PATH, json({ data: [event(1)], has_more: false, saturated: false }));
}

test.describe("alerts: one rule", () => {
  test("loading, then success: the rule, its series, its preview and its history", async ({
    page,
    api,
  }) => {
    serveRule(api);
    const release = api.hold(RULE_PATH, json(rule()));
    await page.goto(RULE_URL);
    const section = page.getByRole("region", { name: "Rule", exact: true });
    await expect(section.getByRole("status")).toHaveText("Loading the rule");
    release();
    await expect(section.getByRole("heading", { name: "Checkout errors" })).toBeVisible();
    await expect(section).toContainText("Errors above 5 over 5 min");
    await expect(section).toContainText("every 4 h while firing");
    const series = page.getByRole("region", { name: "Series" });
    await expect(series.getByRole("row", { name: /environment=prod/ })).toContainText("Firing");
    await expect(series.getByRole("row", { name: /environment=dev/ })).toContainText("OK");
    const next = page.getByRole("region", { name: "Preview of the next check" });
    await expect(next.getByText("1 series would breach now.")).toBeVisible();
    const history = page.getByRole("region", { name: "History" });
    await expect(history.getByText("environment=prod: OK to Firing")).toBeVisible();
    await expect(history.getByText("Value 12. Channels were notified.")).toBeVisible();
    await expectAccessible(page, "rule success");
    await expectNoPageScrollX(page);
  });

  test("a rule that is not there says so, and the other sections ask for nothing", async ({
    page,
    api,
  }) => {
    admin(api);
    api.on(RULE_PATH, problem(404, "not_found", "req-rule"));
    await page.goto(RULE_URL);
    const section = page.getByRole("region", { name: "Rule", exact: true });
    await expect(section.getByText("This isn't in this project.")).toBeVisible();
    expect(api.calls.filter((call) => /preview|history/.test(call))).toEqual([]);
    await expectAccessible(page, "rule not found");
  });

  test("preview: empty and error keep the rule and its history on the page", async ({
    page,
    api,
  }) => {
    serveRule(api);
    api.on(PREVIEW_PATH, json(preview({ series: [] })));
    await page.goto(RULE_URL);
    const next = page.getByRole("region", { name: "Preview of the next check" });
    await expect(next.getByText(/^No data between/)).toBeVisible();
    await expectAccessible(page, "preview empty");

    api.on(PREVIEW_PATH, problem(503, "telemetry_store_unavailable", "req-preview"));
    await page.reload();
    await expect(next.getByText("req-preview")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(page.getByRole("heading", { name: "Checkout errors" })).toBeVisible();
    await expect(page.getByText("environment=prod: OK to Firing")).toBeVisible();
    api.on(PREVIEW_PATH, json(preview()));
    await next.getByRole("button", { name: "Retry preview" }).click();
    await expect(next.getByText("1 series would breach now.")).toBeVisible();
  });

  test("history: empty, error with retry, and older pages on request", async ({ page, api }) => {
    serveRule(api, { series: [], status: "ok", last_evaluated_at: undefined });
    api.on(HISTORY_PATH, json({ data: [], has_more: false, saturated: false }));
    await page.goto(RULE_URL);
    const history = page.getByRole("region", { name: "History" });
    await expect(history.getByText("No history yet.")).toBeVisible();
    await expect(page.getByText("No series yet.")).toBeVisible();
    await expectAccessible(page, "history empty");

    api.on(HISTORY_PATH, problem(503, "control_plane_unavailable", "req-history"));
    await page.reload();
    await expect(history.getByText("req-history")).toBeVisible({ timeout: 10_000 });
    await expectAccessible(page, "history error");

    api.on(HISTORY_PATH, (route, url) =>
      route.fulfill(
        url.searchParams.get("cursor") === "next-1"
          ? json({
              data: [event(2, { kind: "renotify", from_status: "firing" })],
              has_more: false,
              saturated: false,
            })
          : json({ data: [event(1)], next_cursor: "next-1", has_more: true, saturated: false }),
      ),
    );
    await history.getByRole("button", { name: "Retry history" }).click();
    await expect(history.getByText("environment=prod: OK to Firing")).toBeVisible();
    await history.getByRole("button", { name: "Load older history" }).click();
    await expect(history.getByText("environment=prod: still firing")).toBeVisible();
    await expect(history.getByRole("button", { name: "Load older history" })).toHaveCount(0);
  });

  test("turn off sends the version, and delete asks first and returns to the list", async ({
    page,
    api,
  }) => {
    serveRule(api);
    const changes: { method: string; match: string | undefined; body: string | null }[] = [];
    let current = rule();
    api.on(RULE_PATH, (route) => {
      const request = route.request();
      if (request.method() === "GET") return route.fulfill(json(current));
      changes.push({
        method: request.method(),
        match: request.headers()["if-match"],
        body: request.postData(),
      });
      if (request.method() === "DELETE") return route.fulfill({ status: 204 });
      current = rule({ enabled: false, version: 4 });
      return route.fulfill(json(current));
    });
    api.on(RULES_PATH, json({ data: [] }));
    await page.goto(RULE_URL);
    const section = page.getByRole("region", { name: "Rule", exact: true });
    await section.getByRole("button", { name: "Turn off" }).click();
    await expect(section.getByText("Turned off: not checked, notifies nobody")).toBeVisible();
    expect(changes[0]).toEqual({ method: "PATCH", match: '"3"', body: '{"enabled":false}' });

    await section.getByRole("button", { name: "Delete" }).click();
    const dialog = page.getByRole("alertdialog", { name: 'Delete the rule "Checkout errors"?' });
    await expectAccessible(page, "delete rule dialog");
    await dialog.getByRole("button", { name: "Delete rule" }).click();
    await expect(page).toHaveURL(/\/alerts\/?\?project=/);
    await expect(page.getByText("No alert rules yet.")).toBeVisible();
    expect(changes[1]).toMatchObject({ method: "DELETE", match: '"4"' });
  });
});

test.describe("alerts: channels", () => {
  test("loading, then success: each channel with its kind, target and state", async ({
    page,
    api,
  }) => {
    admin(api);
    const release = api.hold(
      CHANNELS_PATH,
      json({
        data: [
          channel(),
          channel({
            id: "c-2",
            name: "Team mail",
            kind: "email",
            url: undefined,
            to: ["team@example.com"],
            enabled: false,
            disabled_reason: "50 deliveries in a row failed",
          }),
        ],
      }),
    );
    await page.goto(CHANNELS_URL);
    const channels = channelsRegion(page);
    await expect(channels.getByRole("status")).toHaveText("Loading channels");
    release();
    const first = channels.locator('[data-channel="On call"]');
    await expect(first).toContainText("Webhook");
    await expect(first).toContainText("http://10.0.0.8:9000/hook");
    const second = channels.locator('[data-channel="Team mail"]');
    await expect(second).toContainText("Turned off by Crucible: 50 deliveries in a row failed");
    await expect(second.getByRole("button", { name: "Turn on Team mail" })).toBeVisible();
    await expect(page.getByText("50 deliveries in a row failed", { exact: true })).toHaveCount(0);
    await expectAccessible(page, "channels success");
    await expectNoPageScrollX(page);
  });

  test("empty, then create: the signing secret shows once", async ({ page, api }) => {
    admin(api);
    const secret = `whsec_${"a".repeat(64)}`;
    const posts: string[] = [];
    api.on(CHANNELS_PATH, (route) => {
      if (route.request().method() !== "POST") return route.fulfill(json({ data: [] }));
      posts.push(route.request().postData() ?? "");
      return route.fulfill(json(channel({ version: 1, signing_secret: secret }), 201));
    });
    await page.goto(CHANNELS_URL);
    await expect(page.getByText("No channels yet.")).toBeVisible();
    await expectAccessible(page, "channels empty");
    await page.getByRole("button", { name: "New channel" }).click();
    const dialog = page.getByRole("dialog", { name: "New channel for Checkout" });
    await dialog.getByLabel("Name").fill("On call");
    await dialog.getByLabel("URL").fill("http://10.0.0.8:9000/hook");
    await expectAccessible(page, "channel form");
    await dialog.getByRole("button", { name: "Create channel" }).click();

    const shown = page.getByRole("dialog", { name: "Copy the signing secret" });
    await expect(shown.getByText(secret)).toBeVisible();
    await expect(shown).toContainText("You won't see this secret again");
    expect(JSON.parse(posts[0] ?? "{}")).toEqual({
      name: "On call",
      kind: "webhook",
      url: "http://10.0.0.8:9000/hook",
    });
    await shown.getByRole("button", { name: "Done" }).click();
    await expect(page.getByText(secret)).toHaveCount(0);
  });

  test("a refused address says what to change, never the server's sentence", async ({
    page,
    api,
  }) => {
    admin(api);
    api.on(CHANNELS_PATH, (route) =>
      route.request().method() === "POST"
        ? route.fulfill({
            status: 422,
            contentType: "application/problem+json",
            headers: { "x-request-id": "req-ssrf" },
            body: JSON.stringify({
              type: "urn:crucible:problem:ssrf_blocked",
              title: SERVER_TEXT,
              status: 422,
              detail: SERVER_TEXT,
              code: "ssrf_blocked",
              request_id: "req-ssrf",
              errors: [{ pointer: "/url", code: "metadata_address", message: SERVER_TEXT }],
            }),
          })
        : route.fulfill(json({ data: [] })),
    );
    await page.goto(CHANNELS_URL);
    await page.getByRole("button", { name: "New channel" }).click();
    const dialog = page.getByRole("dialog", { name: "New channel for Checkout" });
    await dialog.getByLabel("Name").fill("Metadata");
    await dialog.getByLabel("URL").fill("http://169.254.169.254/latest");
    await dialog.getByRole("button", { name: "Create channel" }).click();
    await expect(
      dialog.getByText(/cloud metadata address, which Crucible never calls/),
    ).toBeVisible();
    await expect(dialog.getByText("Crucible won't call that address.")).toBeVisible();
    await expect(dialog.getByText("req-ssrf")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "channel refused");
  });

  test("send test: pending, delivered, and a failure in the page's words", async ({
    page,
    api,
  }) => {
    admin(api);
    api.on(CHANNELS_PATH, json({ data: [channel()] }));
    const release = api.hold(TEST_PATH, json({ ok: true, status_class: "2xx", latency_ms: 42 }));
    await page.goto(CHANNELS_URL);
    const row = channelsRegion(page).locator('[data-channel="On call"]');
    await row.getByRole("button", { name: "Send test to On call" }).click();
    await expect(row.getByRole("button", { name: /Sending a test/ })).toBeDisabled();
    release();
    await expect(row.getByText("The test was delivered in 42 ms.")).toBeVisible();
    await expectAccessible(page, "test delivered");

    api.on(
      TEST_PATH,
      json({ ok: false, status_class: "4xx", latency_ms: 9, error_code: "receiver_rejected" }),
    );
    await row.getByRole("button", { name: "Send test to On call" }).click();
    await expect(row.getByRole("alert")).toContainText("The receiver refused the test");

    api.on(TEST_PATH, problem(429, "rate_limited", "req-limit"));
    await row.getByRole("button", { name: "Send test to On call" }).click();
    await expect(row.getByText("Too many tests.")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
  });

  test("turn off sends the version, and delete asks first", async ({ page, api }) => {
    admin(api);
    let rows: Channel[] = [channel()];
    api.on(CHANNELS_PATH, (route) => route.fulfill(json({ data: rows })));
    const changes: { method: string; match: string | undefined }[] = [];
    api.on(CHANNEL_PATH, (route) => {
      const request = route.request();
      changes.push({ method: request.method(), match: request.headers()["if-match"] });
      if (request.method() === "DELETE") {
        rows = [];
        return route.fulfill({ status: 204 });
      }
      rows = [channel({ enabled: false, version: 3 })];
      return route.fulfill(json(rows[0]));
    });
    await page.goto(CHANNELS_URL);
    const row = channelsRegion(page).locator('[data-channel="On call"]');
    await row.getByRole("button", { name: "Turn off On call" }).click();
    await expect(row.getByText("Turned off", { exact: true })).toBeVisible();
    expect(changes[0]).toEqual({ method: "PATCH", match: '"2"' });

    await row.getByRole("button", { name: "Delete On call" }).click();
    const dialog = page.getByRole("alertdialog", { name: 'Delete the channel "On call"?' });
    await dialog.getByRole("button", { name: "Delete channel" }).click();
    await expect(page.getByText("No channels yet.")).toBeVisible();
    expect(changes[1]).toEqual({ method: "DELETE", match: '"3"' });
  });

  test("error: a failed list says so, and its retry works", async ({ page, api }) => {
    admin(api);
    api.on(CHANNELS_PATH, problem(503, "control_plane_unavailable", "req-channels"));
    await page.goto(CHANNELS_URL);
    const channels = channelsRegion(page);
    await expect(channels.getByText("req-channels")).toBeVisible({ timeout: 10_000 });
    await expectAccessible(page, "channels error");
    api.on(CHANNELS_PATH, json({ data: [channel()] }));
    await channels.getByRole("button", { name: "Retry channels" }).click();
    await expect(channels.locator('[data-channel="On call"]')).toBeVisible();
  });

  test("a viewer gets an explanation, and no channel is requested", async ({ page, api }) => {
    signedIn(api);
    await page.goto(`/alerts/channels/?project=${PROJECT_B}`);
    await expect(page.getByText("Only project members and admins see channels.")).toBeVisible();
    expect(api.calls.filter((call) => call.includes("/channels"))).toEqual([]);
    await expectAccessible(page, "channels without permission");
  });
});

// The phone project runs only phone.spec.ts, so this surface sets the phone viewport itself.
test.describe("alerts: phone at 390 px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test("the rules, a rule, the rule form and the channels fit without scrolling sideways", async ({
    page,
    api,
  }) => {
    admin(api);
    api.on(RULES_PATH, json({ data: [rule()] }));
    api.on(RULE_PATH, json(rule()));
    api.on(PREVIEW_PATH, json(preview()));
    api.on(HISTORY_PATH, json({ data: [event(1)], has_more: false, saturated: false }));
    api.on(CHANNELS_PATH, json({ data: [channel()] }));

    await page.goto(RULES_URL);
    await expect(rulesRegion(page).getByRole("row", { name: /Checkout errors/ })).toBeVisible();
    await expectNoPageScrollX(page);
    await rulesRegion(page).getByRole("button", { name: "New rule" }).click();
    const dialog = page.getByRole("dialog", { name: "New alert rule for Checkout" });
    await expect(dialog.getByRole("button", { name: "Create rule" })).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "rule form on a phone");
    await page.keyboard.press("Escape");

    await page.goto(RULE_URL);
    await expect(page.getByText("environment=prod: OK to Firing")).toBeVisible();
    await expectNoPageScrollX(page);

    await page.goto(CHANNELS_URL);
    await expect(channelsRegion(page).locator('[data-channel="On call"]')).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "channels on a phone");
  });
});
