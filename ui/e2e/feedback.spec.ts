import type { BrowserContext, Request } from "@playwright/test";
import {
  expect,
  expectAccessible,
  json,
  LIST_PATH,
  PROJECT,
  signedIn,
  test,
  traceItem,
  tracePage,
} from "./fixtures";

const LIST_URL = `/traces/?project=${PROJECT}`;
const DISCUSSIONS = "https://github.com/elm-os/crucible/discussions";
const NEW_ISSUE = "https://github.com/elm-os/crucible/issues/new/choose";

// GitHub answers from the test, and each request it gets is kept to check what the browser sent.
async function stubGitHub(context: BrowserContext): Promise<Request[]> {
  const requests: Request[] = [];
  await context.route("https://github.com/**", (route) => {
    requests.push(route.request());
    return route.fulfill({ contentType: "text/html", body: "<title>GitHub</title>" });
  });
  return requests;
}

test.describe("shell: the Feedback menu", () => {
  test("keyboard: Enter opens it on the first link, Escape returns focus, Enter opens a link in a new tab", async ({
    page,
    api,
    context,
    baseURL,
  }) => {
    const github = await stubGitHub(context);
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    const elsewhere: string[] = [];
    page.on("request", (request) => {
      if (new URL(request.url()).origin !== new URL(String(baseURL)).origin) {
        elsewhere.push(request.url());
      }
    });
    await page.goto(LIST_URL);
    const trigger = page.getByRole("banner").getByRole("button", { name: "Feedback", exact: true });
    await trigger.focus();
    await page.keyboard.press("Enter");
    const menu = page.getByRole("menu", { name: "Feedback" });
    await expect(menu).toBeVisible();
    await expect(menu.getByRole("group", { name: "Opens GitHub in a new tab" })).toBeVisible();
    await expect(
      menu.getByRole("menuitem", { name: "Ask a question or suggest an idea" }),
    ).toBeFocused();
    await expectAccessible(page, "feedback menu open");
    expect(elsewhere, "requests that left the server before a link was chosen").toEqual([]);

    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(trigger).toBeFocused();

    await page.keyboard.press("Enter");
    await page.keyboard.press("ArrowDown");
    await expect(menu.getByRole("menuitem", { name: "Report a bug" })).toBeFocused();
    const opened = context.waitForEvent("page");
    await page.keyboard.press("Enter");
    const tab = await opened;
    await expect(tab).toHaveURL(NEW_ISSUE);
    expect(await tab.evaluate(() => window.opener)).toBeNull();
    await expect(menu).toBeHidden();
    await expect(page).toHaveURL(LIST_URL);
    expect(github.map((request) => request.url())).toEqual([NEW_ISSUE]);
    expect(github[0]?.headers().referer).toBeUndefined();
  });

  test("signed in: a press on a link opens GitHub in a new tab, with no referrer and no query", async ({
    page,
    api,
    context,
  }) => {
    const github = await stubGitHub(context);
    signedIn(api);
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(LIST_URL);
    const banner = page.getByRole("banner");
    await expect(banner.getByRole("button", { name: "Sign out" })).toBeVisible();
    await banner.getByRole("button", { name: "Feedback", exact: true }).click();
    const opened = context.waitForEvent("page");
    await page.getByRole("menuitem", { name: "Ask a question or suggest an idea" }).click();
    const tab = await opened;
    await expect(tab).toHaveURL(DISCUSSIONS);
    expect(await tab.evaluate(() => window.opener)).toBeNull();
    await expect(page.getByRole("menu", { name: "Feedback" })).toBeHidden();
    await expect(page).toHaveURL(LIST_URL);
    expect(github.map((request) => request.url())).toEqual([DISCUSSIONS]);
    expect(github[0]?.headers().referer).toBeUndefined();
  });
});
