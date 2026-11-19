import type { Page } from "@playwright/test";
import {
  capture,
  expect,
  expectAccessible,
  json,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const SETUP_PATH = /\/api\/v1\/auth\/setup$/;
const ACCEPT_PATH = /\/api\/v1\/auth\/invitations\/accept$/;
const RESET_REQUEST_PATH = /\/api\/v1\/auth\/password\/reset-request$/;
const RESET_CONFIRM_PATH = /\/api\/v1\/auth\/password\/reset-confirm$/;
const PASSWORD = "correct horse battery staple";

// Next's route announcer is an alert too; the page's own alerts are in its main region.
function alertIn(page: Page) {
  return page.getByRole("main").getByRole("alert");
}

async function fillSetup(page: Page, password = PASSWORD) {
  await page.getByLabel("Setup token").fill("  setup-token-1 ");
  await page.getByLabel("Organization name").fill("Analytical Engines");
  await page.getByLabel("Your name").fill("Ada Lovelace");
  await page.getByLabel("Email").fill("ada@example.com");
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByRole("button", { name: "Create owner account" }).click();
}

test.describe("first-run setup", () => {
  test("success: the owner is created with prelogin and Crucible opens signed in", async ({
    page,
    api,
  }) => {
    await page.goto("/setup/");
    const requirement = page.getByText("12 to 128 characters");
    await expect(requirement).toContainText("To do:");
    await expectAccessible(page, "setup");
    const setups = capture(api, SETUP_PATH, json(sessionInfo()));
    signedIn(api);
    await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
    await expect(requirement).toContainText("Done:");
    await fillSetup(page);
    await expect(page).toHaveURL(/\/home\/?$/);
    await expect(page.getByRole("banner")).toContainText("Ada Lovelace");
    expect(setups[0]?.headers()["x-crucible-csrf"]).toBe("prelogin");
    expect(setups[0]?.postDataJSON()).toEqual({
      token: "setup-token-1",
      org_name: "Analytical Engines",
      name: "Ada Lovelace",
      email: "ada@example.com",
      password: PASSWORD,
    });
  });

  test("error: a short password is caught before anything is sent", async ({ page, api }) => {
    await page.goto("/setup/");
    await fillSetup(page, "too short");
    await expect(page.getByText("Use 12 to 128 characters.")).toBeVisible();
    expect(api.count(SETUP_PATH)).toBe(0);
  });

  test("error: a used token marks the token field; an existing owner points to sign-in", async ({
    page,
    api,
  }) => {
    await page.goto("/setup/");
    api.on(SETUP_PATH, problem(422, "token_invalid"));
    await fillSetup(page);
    await expect(alertIn(page)).toContainText("The setup token isn't valid.");
    await expect(page.getByLabel("Setup token")).toHaveAttribute("aria-invalid", "true");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "setup token error");

    api.on(SETUP_PATH, problem(409, "conflict"));
    await page.getByRole("button", { name: "Create owner account" }).click();
    await expect(alertIn(page)).toContainText("Crucible already has an owner.");
    await expect(alertIn(page).getByRole("link", { name: "Sign in" })).toHaveAttribute(
      "href",
      /^\/login\/?$/,
    );
  });
});

test.describe("invitations", () => {
  test("success: joining sends the link's token with prelogin, and Crucible opens", async ({
    page,
    api,
  }) => {
    await page.goto("/invite/?token=invite-token-1");
    await expectAccessible(page, "invitation");
    const accepts = capture(api, ACCEPT_PATH, json(sessionInfo()));
    signedIn(api);
    await page.getByLabel("Your name").fill(" Grace Hopper ");
    await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
    await page.getByRole("button", { name: "Accept invitation" }).click();
    await expect(page).toHaveURL(/\/home\/?$/);
    expect(accepts[0]?.headers()["x-crucible-csrf"]).toBe("prelogin");
    expect(accepts[0]?.postDataJSON()).toEqual({
      token: "invite-token-1",
      name: "Grace Hopper",
      password: PASSWORD,
    });
  });

  test("error: an invitation that can't be used says what to do", async ({ page, api }) => {
    await page.goto("/invite/?token=invite-token-1");
    api.on(ACCEPT_PATH, problem(422, "token_invalid"));
    await page.getByLabel("Your name").fill("Grace Hopper");
    await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
    await page.getByRole("button", { name: "Accept invitation" }).click();
    await expect(alertIn(page)).toContainText("This invitation can't be used.");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "invitation error");
  });

  test("empty: a link without its token says so and asks for nothing", async ({ page, api }) => {
    await page.goto("/invite/");
    await expect(page.getByText("This invitation link is incomplete.")).toBeVisible();
    await expect(page.getByLabel("Your name")).toHaveCount(0);
    expect(api.calls).toEqual([]);
    await expectAccessible(page, "invitation without a token");
  });
});

test.describe("password reset", () => {
  test("request: the page names the administrator's command, then confirms the request", async ({
    page,
    api,
  }) => {
    await page.goto("/reset/");
    await expect(page.getByRole("main")).toContainText(
      "crucible admin reset-password <your email>",
    );
    await expectAccessible(page, "reset request");
    const requests = capture(api, RESET_REQUEST_PATH, { status: 202 });
    await page.getByLabel("Email").fill("ada@example.com");
    await page.getByRole("button", { name: "Request a reset link" }).click();
    const heading = page.getByRole("heading", { name: "Request received" });
    await expect(heading).toBeFocused();
    await expect(page.getByRole("main")).toContainText(
      "crucible admin reset-password ada@example.com",
    );
    expect(requests[0]?.headers()["x-crucible-csrf"]).toBe("prelogin");
    expect(requests[0]?.postDataJSON()).toEqual({ email: "ada@example.com" });
    await expectAccessible(page, "reset requested");
  });

  test("confirm: a new password with the link's token, then sign in", async ({ page, api }) => {
    await page.goto("/reset/?token=reset-token-1");
    await expectAccessible(page, "reset confirm");
    const confirms = capture(api, RESET_CONFIRM_PATH, { status: 204 });
    await page.getByLabel("New password", { exact: true }).fill(PASSWORD);
    await page.getByRole("button", { name: "Change password" }).click();
    await expect(page.getByRole("heading", { name: "Password changed" })).toBeFocused();
    await expect(page.getByRole("link", { name: "Sign in" })).toHaveAttribute(
      "href",
      /^\/login\/?$/,
    );
    expect(confirms[0]?.postDataJSON()).toEqual({ token: "reset-token-1", new_password: PASSWORD });
  });

  test("error: a used reset link says how to get a new one", async ({ page, api }) => {
    await page.goto("/reset/?token=reset-token-1");
    api.on(RESET_CONFIRM_PATH, problem(422, "token_invalid"));
    await page.getByLabel("New password", { exact: true }).fill(PASSWORD);
    await page.getByRole("button", { name: "Change password" }).click();
    await expect(alertIn(page)).toContainText("This reset link can't be used.");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "reset error");
  });
});
