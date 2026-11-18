import type { Locator, Page } from "@playwright/test";
import {
  expect,
  expectAccessible,
  expectNoPageScrollX,
  json,
  LIST_PATH,
  PROJECT,
  PROVIDERS_PATH,
  SIGN_IN_CHECK,
  signedIn,
  signInOptions,
  TRACE_PATH,
  test,
  traceDetail,
  traceItem,
  tracePage,
  traceUrl,
} from "./fixtures";

// On a touch screen the sheet moves by native scroll snapping. Locator.tap scrolls its target into view
// first, which drags the sheet; a finger taps where the control rests once the sheet has settled.
async function tapWhereItRests(page: Page, target: Locator): Promise<void> {
  let last = "";
  await expect
    .poll(async () => {
      const box = await target.boundingBox();
      const now = box ? `${Math.round(box.x)},${Math.round(box.y)}` : "";
      const settled = now !== "" && now === last;
      last = now;
      return settled;
    })
    .toBe(true);
  await expect(target).toBeInViewport({ ratio: 1 });
  const box = await target.boundingBox();
  if (!box) throw new Error("the target left the page");
  await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
}

// Phones are for reading and triage: every page reads at 390 px without the page scrolling sideways.
test.describe("phone at 390 px", () => {
  test("home reads without sideways scrolling", async ({ page, api }) => {
    await page.goto(`/home/?project=${PROJECT}`);
    await expect(page.getByRole("region", { name: "Set up" })).toBeVisible();
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
    await expectNoPageScrollX(page);
    await expectAccessible(page, "home on a phone");
  });

  test("the traces table scrolls inside its frame, never the page", async ({ page, api }) => {
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(`/traces/?project=${PROJECT}`);
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "traces on a phone");
  });

  test("triage: choosing a span opens its sheet; Escape closes it and focus returns to the row", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    const grid = page.getByRole("treegrid", { name: "Spans" });
    await expect(grid).toBeVisible();
    await expect(page.getByRole("region", { name: "Span", exact: true })).toBeHidden();
    await expectNoPageScrollX(page);
    await page.screenshot({ path: test.info().outputPath("trace-phone.png"), fullPage: true });

    const row = grid.getByRole("row", { name: /send_reply/ });
    await row.focus();
    await page.keyboard.press("Enter");
    const sheet = page.getByRole("dialog", { name: "Span" });
    await expect(sheet).toBeVisible();
    await expect(sheet).toContainText("send_reply");
    await expect(sheet).toContainText("Error");
    await expect(page).toHaveURL(/span=a000000000000008/);
    await expectAccessible(page, "span sheet open");
    await page.screenshot({ path: test.info().outputPath("trace-phone-sheet.png") });

    await page.keyboard.press("Escape");
    await expect(sheet).toBeHidden();
    await expect(row).toBeFocused();
    await expect(row).toHaveAttribute("aria-selected", "true");
  });

  test("triage: a tap opens the sheet, Close shuts it, and the button opens it again", async ({
    page,
    api,
  }) => {
    api.on(TRACE_PATH, json(traceDetail()));
    await page.goto(traceUrl());
    await page.getByRole("row", { name: /plan reply/ }).tap();
    const sheet = page.getByRole("dialog", { name: "Span" });
    await expect(sheet).toContainText("gpt-4.1");
    await tapWhereItRests(page, sheet.getByRole("button", { name: "Close" }));
    await expect(sheet).toBeHidden();
    await page.getByRole("button", { name: "Show span details" }).tap();
    await expect(sheet).toContainText("plan reply");
  });

  test("sign in reads without sideways scrolling", async ({ page, api }) => {
    api.on(
      PROVIDERS_PATH,
      json(
        signInOptions({
          providers: [{ id: "p-1", kind: "oidc", display_name: "Continue with Acme SSO" }],
        }),
      ),
    );
    await page.goto("/login/?error=not_invited");
    await expect(page.getByLabel("Email")).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "sign in on a phone");
  });

  test("setup and invitation pages read without sideways scrolling", async ({ page }) => {
    await page.goto("/setup/");
    await expect(page.getByLabel("Setup token")).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "setup on a phone");
    await page.goto("/invite/?token=invite-token-1");
    await expect(page.getByLabel("Your name")).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "invitation on a phone");
  });

  test("members: the header and both tables fit, the tables scrolling inside their frames", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(
      /\/api\/v1\/org\/members$/,
      json({
        data: [
          {
            user_id: "u-2",
            email: "grace.brewster.murray.hopper@example.com",
            name: "Grace Hopper",
            role: "member",
            status: "active",
          },
        ],
      }),
    );
    api.on(
      /\/api\/v1\/org\/invitations$/,
      json({
        data: [
          {
            id: "i-1",
            email: "barbara.liskov.of.mit@example.com",
            org_role: "viewer",
            project_roles: [],
            expires_at: "2099-01-01T00:00:00.000000Z",
            accepted_at: null,
            revoked_at: null,
          },
        ],
      }),
    );
    await page.goto("/settings/members/");
    await expect(page.getByRole("cell", { name: "Grace Hopper", exact: true })).toBeVisible();
    await expect(
      page.getByRole("cell", { name: "barbara.liskov.of.mit@example.com", exact: true }),
    ).toBeVisible();
    await expect(page.getByRole("banner")).toContainText("Ada Lovelace");
    await expectNoPageScrollX(page);
    await expectAccessible(page, "members on a phone");
  });

  test("API keys: the table scrolls inside its frame, and a new key's secret wraps inside its dialog", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const secret = `csk_${"s".repeat(43)}`;
    const key = {
      id: "k-1",
      public_key: `cpk_${"a".repeat(43)}`,
      scopes: ["ingest", "read", "write"],
      note: "checkout-service",
      expires_at: null,
      last_used_at: null,
      revoked_at: null,
      version: 1,
    };
    api.on(new RegExp(`/api/v1/projects/${PROJECT}/api-keys$`), (route) =>
      route.fulfill(
        route.request().method() === "POST"
          ? json({ key: { ...key, id: "k-2", note: "" }, secret_key: secret }, 201)
          : json({ data: [key] }),
      ),
    );
    await page.goto(`/settings/api-keys/?project=${PROJECT}`);
    await expect(page.getByRole("cell", { name: "checkout-service" })).toBeVisible();
    await expectNoPageScrollX(page);
    await page.getByRole("button", { name: "Create key" }).tap();
    await page.getByRole("dialog").getByRole("button", { name: "Create key" }).tap();
    await expect(page.getByText(secret)).toBeVisible();
    await expect(page.getByText(secret)).toBeInViewport({ ratio: 1 });
    await expectNoPageScrollX(page);
    await expectAccessible(page, "new key on a phone");
  });
});
