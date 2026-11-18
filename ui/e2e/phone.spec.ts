import type { Locator, Page } from "@playwright/test";
import {
  expect,
  expectAccessible,
  expectNoPageScrollX,
  json,
  LIST_PATH,
  listPage,
  PROJECT,
  SESSION,
  SESSION_PATH,
  SESSIONS_PATH,
  SPANS_PATH,
  sessionItem,
  spanListItem,
  TRACE_PATH,
  test,
  traceDetail,
  traceItem,
  tracePage,
  traceUrl,
  USER,
  USER_PATH,
  USER_SESSIONS_PATH,
  USERS_PATH,
  userItem,
  userSessionItem,
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
  test("home reads without sideways scrolling", async ({ page }) => {
    await page.goto(`/home/?project=${PROJECT}`);
    await expect(page.getByRole("region", { name: "Set up" })).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "home on a phone");
  });

  test("search results and their snippets wrap within the page", async ({ page, api }) => {
    const words = "a long input that keeps going ".repeat(12);
    api.on(
      new RegExp(`/projects/${PROJECT}/search$`),
      json({
        data: [
          {
            trace_id: traceItem().trace_id,
            span: traceDetail().spans[1],
            matches: [
              {
                field: "input",
                fragments: [
                  { text: words, highlighted: false },
                  { text: "refund", highlighted: true },
                ],
                cut_before: true,
                cut_after: true,
              },
            ],
          },
        ],
        next_cursor: null,
        has_more: false,
        meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
      }),
    );
    await page.goto(`/search/?project=${PROJECT}&q=refund`);
    await expect(page.getByRole("link", { name: "plan reply" })).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "search on a phone");
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
});

test.describe("phone at 390 px: lists", () => {
  const whole = { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" };
  const longId = "conversation-".repeat(6);

  test("sessions, users, spans and their pages read without sideways scrolling", async ({
    page,
    api,
  }) => {
    api.on(SESSIONS_PATH, json(listPage([sessionItem({ session_id: longId })])));
    api.on(USERS_PATH, json(listPage([userItem()])));
    api.on(SPANS_PATH, json(listPage([spanListItem()])));
    api.on(SESSION_PATH, json({ session: sessionItem(), meta: whole }));
    api.on(USER_PATH, json({ user: userItem(), meta: whole }));
    api.on(USER_SESSIONS_PATH, json(listPage([userSessionItem()])));
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    for (const [url, region] of [
      [`/sessions/?project=${PROJECT}`, "Sessions"],
      [`/session/?project=${PROJECT}&session=${SESSION}`, "Summary"],
      [`/users/?project=${PROJECT}`, "Users"],
      [`/user/?project=${PROJECT}&user=${USER}`, "Sessions of this user"],
      [`/spans/?project=${PROJECT}&level=error&name=${longId}`, "Spans"],
      [`/traces/?project=${PROJECT}&environment=${longId}`, "Recent traces"],
    ] as const) {
      await page.goto(url);
      await expect(page.getByRole("region", { name: region })).toBeVisible();
      await expect(page.getByRole("region", { name: region }).getByRole("status")).toHaveCount(0);
      await expectNoPageScrollX(page);
    }
    await expectAccessible(page, "traces with a filter chip on a phone");
  });
});
