import type { Page } from "@playwright/test";
import {
  CSRF,
  capture,
  expect,
  expectAccessible,
  json,
  LIST_PATH,
  PROJECT,
  PROVIDERS_PATH,
  problem,
  SERVER_TEXT,
  SESSION_PATH,
  sessionInfo,
  signedIn,
  signInOptions,
  test,
  traceItem,
  tracePage,
} from "./fixtures";

const LOGIN_PATH = /\/api\/v1\/auth\/login$/;
const PROVIDER = {
  id: "0193a1b2-0000-7000-8000-0000000000p1",
  kind: "oidc",
  display_name: "Continue with Acme SSO",
} as const;
const TRACES = `/traces/?project=${PROJECT}`;

// Next's route announcer is an alert too; the page's own alerts are in its main region.
function alertIn(page: Page) {
  return page.getByRole("main").getByRole("alert");
}

async function fillAndSubmit(page: Page, password = "correct horse battery staple") {
  await page.getByLabel("Email").fill("ada@example.com");
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
}

test.describe("sign in", () => {
  test("loading, then providers and the password form; signing in returns to the page that asked", async ({
    page,
    api,
  }) => {
    const release = api.hold(PROVIDERS_PATH, json(signInOptions({ providers: [PROVIDER] })));
    await page.goto(`/login/?next=${encodeURIComponent(TRACES)}`);
    await expect(page.getByRole("status")).toHaveText("Loading the ways to sign in");
    await expectAccessible(page, "sign in loading");
    release();

    await expect(page.getByLabel("Email")).toBeVisible();
    await expect(page.getByRole("link", { name: "Continue with Acme SSO" })).toHaveAttribute(
      "href",
      `/api/v1/auth/oauth/${PROVIDER.id}/start?next=${encodeURIComponent(TRACES)}`,
    );
    await expect(page.getByRole("link", { name: "Forgot your password?" })).toHaveAttribute(
      "href",
      /^\/reset\/?$/,
    );
    await expectAccessible(page, "sign in");

    const logins = capture(api, LOGIN_PATH, json(sessionInfo()));
    signedIn(api);
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await fillAndSubmit(page);
    await expect(page).toHaveURL(TRACES);
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
    expect(logins[0]?.headers()["x-crucible-csrf"]).toBe("prelogin");
    expect(logins[0]?.postDataJSON()).toEqual({
      email: "ada@example.com",
      password: "correct horse battery staple",
    });
  });

  test("a next that leaves this server is dropped, so signing in opens the start page", async ({
    page,
    api,
  }) => {
    api.on(PROVIDERS_PATH, json(signInOptions()));
    await page.goto(`/login/?next=${encodeURIComponent("//evil.example/steal")}`);
    api.on(LOGIN_PATH, json(sessionInfo()));
    signedIn(api);
    await fillAndSubmit(page);
    await expect(page).toHaveURL(/\/home\/?$/);
    await expect(page.getByRole("region", { name: "Set up" })).toBeVisible();
  });

  test("error: a wrong password, a rate limit and a refused origin each say why, in the page's words", async ({
    page,
    api,
  }) => {
    api.on(PROVIDERS_PATH, json(signInOptions()));
    await page.goto("/login/");
    const cases = [
      [401, "invalid_credentials", "That email and password don't match an account."],
      [429, "rate_limited", "Too many attempts."],
      [403, "origin_invalid", "Crucible refused a request from this address."],
    ] as const;
    for (const [status, code, title] of cases) {
      api.on(LOGIN_PATH, problem(status, code, `req-${code}`));
      await fillAndSubmit(page, "a wrong password");
      const alert = alertIn(page);
      await expect(alert).toContainText(title);
      await expect(alert).toContainText(`req-${code}`);
      await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
      await expect(page).toHaveURL(/\/login\/$/);
    }
    await expectAccessible(page, "sign in error");
  });

  test("error: options that fail to load say why, and Try again loads them", async ({
    page,
    api,
  }) => {
    api.on(PROVIDERS_PATH, problem(503, "control_plane_unavailable", "req-503"));
    await page.goto("/login/");
    await expect(alertIn(page)).toContainText("Crucible can't reach its database.", {
      timeout: 10_000,
    });
    await expect(page.getByText("req-503")).toBeVisible();
    await expectAccessible(page, "sign-in options error");
    api.on(PROVIDERS_PATH, json(signInOptions()));
    await page.getByRole("button", { name: "Try again" }).click();
    await expect(page.getByLabel("Email")).toBeVisible();
  });

  test("error: each failure a provider sign-in comes back with has its own copy", async ({
    page,
    api,
  }) => {
    api.on(PROVIDERS_PATH, json(signInOptions({ providers: [PROVIDER] })));
    const cases = [
      ["oauth_denied", "Sign-in was cancelled."],
      ["oauth_state_invalid", "That sign-in attempt expired."],
      ["oauth_exchange_failed", "The sign-in provider didn't confirm who you are."],
      ["email_unverified", "Your email isn't verified with the sign-in provider."],
      ["not_invited", "This account hasn't been invited."],
      ["account_disabled", "This account is disabled."],
      ["provider_disabled", "That sign-in option is turned off."],
      ["sso_required", "Your organization signs in with its own provider."],
      ["<script>", "Couldn't sign you in."],
    ];
    for (const [code, title] of cases) {
      await page.goto(`/login/?error=${encodeURIComponent(code ?? "")}`);
      await expect(alertIn(page)).toContainText(title ?? "");
      await expect(page.getByLabel("Email")).toBeVisible();
    }
    await expect(page.getByText("<script>")).toHaveCount(0);
    await expectAccessible(page, "sign in after a provider error");

    // A provider that lets only some domains sign in names them, and the page shows only domains.
    await page.goto(
      `/login/?error=not_invited&domains=${encodeURIComponent("acme.test,<b>x</b>")}`,
    );
    await expect(alertIn(page)).toContainText("This provider doesn't accept your email address.");
    await expect(alertIn(page)).toContainText("Only addresses at acme.test can sign in with it.");
    await expect(page.getByText("<b>x</b>")).toHaveCount(0);
    await expectAccessible(page, "sign in after a domain refusal");
  });

  test("empty: a server with no owner points to setup; one without a control plane has no sign-in", async ({
    page,
    api,
  }) => {
    api.on(PROVIDERS_PATH, json(signInOptions({ setup_required: true })));
    await page.goto("/login/");
    await expect(page.getByText("Crucible isn't set up yet.")).toBeVisible();
    await expect(page.getByRole("link", { name: "Set up Crucible" })).toHaveAttribute(
      "href",
      /^\/setup\/?$/,
    );
    await expect(page.getByLabel("Email")).toHaveCount(0);
    await expectAccessible(page, "sign in before setup");

    api.on(PROVIDERS_PATH, problem(404, "feature_disabled"));
    await page.reload();
    await expect(page.getByText("This server has no sign-in.")).toBeVisible();
    await expect(page.getByLabel("Email")).toHaveCount(0);
  });
});

test.describe("sessions", () => {
  test("loading, then error: the page waits for the check, and a failed check offers a retry", async ({
    page,
    api,
  }) => {
    const release = api.hold(
      PROVIDERS_PATH,
      problem(503, "control_plane_unavailable", "req-check"),
    );
    await page.goto(TRACES);
    const main = page.getByRole("main");
    await expect(main.getByRole("status")).toHaveText("Connecting to Crucible");
    await expectAccessible(page, "sign-in check loading");
    release();
    const card = page.getByRole("region", { name: "Connection" });
    await expect(card.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(card.getByText("req-check")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    expect(api.count(LIST_PATH)).toBe(0);
    await expectAccessible(page, "sign-in check error");

    signedIn(api);
    api.on(LIST_PATH, json(tracePage([traceItem()])));
    await card.getByRole("button", { name: "Retry connection" }).click();
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
    await expect(page.getByRole("banner")).toContainText("Ada Lovelace");
  });

  test("a page without a session sends the browser to sign in, keeping where it was", async ({
    page,
    api,
  }) => {
    api.on(PROVIDERS_PATH, json(signInOptions()));
    api.on(SESSION_PATH, problem(401, "unauthenticated"));
    const from = `/traces/?project=${PROJECT}&range=7d`;
    await page.goto(from);
    await expect(page).toHaveURL(`/login/?next=${encodeURIComponent(from)}`);
    await expect(page.getByLabel("Email")).toBeVisible();
    expect(api.count(LIST_PATH)).toBe(0);
  });

  test("a session that ends while reading also goes to sign in", async ({ page, api }) => {
    signedIn(api);
    api.on(LIST_PATH, problem(401, "unauthenticated"));
    await page.goto(TRACES);
    await expect(page).toHaveURL(`/login/?next=${encodeURIComponent(TRACES)}`);
  });

  test("signed in: the header shows the user and Sign out, reads send no key, and signing out opens sign-in", async ({
    page,
    api,
  }) => {
    // A key left from a server without a control plane must not replace the session.
    await page.addInitScript(() =>
      localStorage.setItem("crucible.connection.key", `csk_${"7".repeat(43)}`),
    );
    signedIn(api);
    const reads = capture(api, LIST_PATH, json(tracePage([traceItem()])));
    await page.goto(TRACES);
    const banner = page.getByRole("banner");
    await expect(banner).toContainText("Ada Lovelace");
    await expect(banner.getByRole("button", { name: "Connect" })).toHaveCount(0);
    await expect(page.getByRole("link", { name: "support agent" })).toBeVisible();
    expect(reads[0]?.headers().authorization).toBeUndefined();

    await banner.getByRole("combobox", { name: "Project" }).click();
    await expect(page.getByRole("option")).toHaveText(["Checkout", "Search"]);
    await page.keyboard.press("Escape");
    await expectAccessible(page, "signed-in shell");

    const logouts = capture(api, /\/api\/v1\/auth\/logout$/, { status: 204 });
    api.on(PROVIDERS_PATH, json(signInOptions()));
    await banner.getByRole("button", { name: "Sign out" }).click();
    await expect(page).toHaveURL(/\/login\/$/);
    expect(logouts[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
  });
});
