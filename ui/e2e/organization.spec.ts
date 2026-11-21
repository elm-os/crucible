import type { Page } from "@playwright/test";
import type { Org } from "../src/api/generated/Org";
import type { OrgRole } from "../src/api/generated/OrgRole";
import {
  CSRF,
  capture,
  expect,
  expectAccessible,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const ORG_PATH = /\/api\/v1\/org$/;
const URL = `/settings/organization/?project=${PROJECT}`;

function org(fields: Partial<Org> = {}, settings: Partial<Org["settings"]> = {}): Org {
  return {
    id: "0193a1b2-0000-7000-8000-0000000000aa",
    name: "Analytical Engines",
    settings: {
      signup_mode: "invite_only",
      allowed_email_domains: [],
      password_login_enabled: true,
      project_access: "all",
      ...settings,
    },
    version: 3,
    ...fields,
  };
}

function withRole(role: OrgRole) {
  const session = sessionInfo();
  return { ...session, org: { ...session.org, role } };
}

const region = (page: Page) => page.getByRole("region", { name: "Organization" });

test.describe("settings: organization", () => {
  test("loading, then success: a save sends only what changed, with If-Match and CSRF", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const release = api.hold(ORG_PATH, json(org({}, { allowed_email_domains: ["example.com"] })));
    await page.goto(URL);
    await expect(region(page).getByRole("status").first()).toHaveText(
      "Loading organization settings",
    );
    await expectAccessible(page, "organization loading");
    release();
    await expect(region(page).getByLabel("Organization name")).toHaveValue("Analytical Engines");
    await expect(region(page).getByLabel("Allowed email domains")).toHaveValue("example.com");
    await expect(region(page).getByRole("radio", { name: "Every project" })).toBeChecked();
    await expect(
      page.getByText("People join by invitation. There is no open sign-up."),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: "Organization" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    await expectAccessible(page, "organization success");

    const patches = capture(
      api,
      ORG_PATH,
      json(org({ version: 4 }, { allowed_email_domains: ["example.com", "example.org"] })),
    );
    await region(page).getByLabel("Allowed email domains").fill("example.com, @Example.org");
    await region(page).getByRole("radio", { name: "Only projects they're added to" }).click();
    await expect(page.getByText("POST /api/v1/projects/{project_id}/members")).toBeVisible();
    await region(page).getByRole("button", { name: "Save settings" }).click();
    await expect(region(page).getByText("Saved.")).toBeVisible();
    const patch = patches.find((sent) => sent.method() === "PATCH");
    expect(patch?.headers()["if-match"]).toBe('"3"');
    expect(patch?.headers()["x-crucible-csrf"]).toBe(CSRF);
    expect(patch?.postDataJSON()).toEqual({
      allowed_email_domains: ["example.com", "example.org"],
      project_access: "explicit",
    });
  });

  test("a save after someone else's change reloads the stored settings and keeps saying why", async ({
    page,
    api,
  }) => {
    signedIn(api);
    let stored = org();
    api.on(ORG_PATH, (route) => {
      if (route.request().method() === "PATCH") {
        stored = org({ name: "Difference Engines", version: 8 });
        return route.fulfill(problem(412, "precondition_failed", "req-stale"));
      }
      return route.fulfill(json(stored));
    });
    await page.goto(URL);
    const name = region(page).getByLabel("Organization name");
    await name.fill("Babbage and Co");
    await region(page).getByRole("button", { name: "Save settings" }).click();
    await expect(name).toHaveValue("Difference Engines");
    const alert = region(page).getByRole("alert");
    await expect(alert).toContainText("These settings changed since the page loaded.");
    await expect(alert).toContainText("Enter your change again, then save.");
    await expect(alert).toContainText("req-stale");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "organization reloaded after someone else's change");

    await name.fill("Babbage and Co");
    await expect(alert).toHaveCount(0);
  });

  test("password sign-in stays on while it is the only way in, and the page says what to turn on first", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(ORG_PATH, (route) =>
      route.request().method() === "PATCH"
        ? route.fulfill(problem(409, "conflict", "req-last"))
        : route.fulfill(json(org())),
    );
    await page.goto(URL);
    const password = region(page).getByRole("switch", { name: "Password sign-in" });
    await expect(password).toBeChecked();
    await password.click();
    await expect(password).not.toBeChecked();
    await region(page).getByRole("button", { name: "Save settings" }).click();
    const alert = region(page).getByRole("alert");
    await expect(alert).toContainText("Password sign-in is the only way to sign in.");
    await expect(alert).toContainText("Turn on a sign-in provider on the Sign-in page first");
    await expect(alert).toContainText("req-last");
    await expect(region(page).getByText("Turn on a sign-in provider first.")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "organization refusing to turn off the only way in");

    const patches = capture(
      api,
      ORG_PATH,
      json(org({ version: 4 }, { password_login_enabled: false })),
    );
    await region(page).getByRole("button", { name: "Save settings" }).click();
    await expect(region(page).getByText("Saved.")).toBeVisible();
    const patch = patches.find((sent) => sent.method() === "PATCH");
    expect(patch?.postDataJSON()).toEqual({ password_login_enabled: false });
    expect(patch?.headers()["if-match"]).toBe('"3"');
    await expect(password).not.toBeChecked();
    api.on(/\/api\/v1\/org\/auth-providers$/, json({ data: [] }));
    await region(page).getByRole("link", { name: "Sign-in page" }).click();
    await expect(page).toHaveURL(/\/settings\/sign-in\//);
    await expect(page.getByText("No sign-in providers yet.")).toBeVisible();
  });

  test("a domain list that isn't one is marked before anything is sent, and a refused name after", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const patches = capture(api, ORG_PATH, json(org()));
    await page.goto(URL);
    const domains = region(page).getByLabel("Allowed email domains");
    await domains.fill("ada@example.com");
    await expect(
      region(page).getByText("Enter domains only, like example.com, separated by commas."),
    ).toBeVisible();
    await region(page).getByRole("button", { name: "Save settings" }).click();
    expect(patches.filter((sent) => sent.method() === "PATCH")).toHaveLength(0);
    await expectAccessible(page, "organization with an invalid domain");

    await domains.fill("");
    api.on(ORG_PATH, (route) =>
      route.request().method() === "PATCH"
        ? route.fulfill({
            status: 422,
            contentType: "application/problem+json",
            body: JSON.stringify({
              type: "urn:crucible:problem:validation_failed",
              title: SERVER_TEXT,
              status: 422,
              detail: SERVER_TEXT,
              code: "validation_failed",
              request_id: "req-name",
              errors: [{ pointer: "/name", code: "invalid", message: SERVER_TEXT }],
            }),
          })
        : route.fulfill(json(org())),
    );
    await region(page).getByLabel("Organization name").fill("A".repeat(150));
    await region(page).getByRole("button", { name: "Save settings" }).click();
    await expect(region(page).getByText("Use 1 to 200 characters.")).toBeVisible();
    await expect(region(page).getByRole("alert")).toContainText("Some fields need attention.");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
  });

  test("error: the section keeps the failure to itself, with a retry, and shows no server text", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(ORG_PATH, problem(503, "control_plane_unavailable"));
    await page.goto(URL);
    const retry = region(page).getByRole("button", { name: "Retry organization settings" });
    await expect(retry).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(page.getByRole("navigation", { name: "Settings" })).toBeVisible();
    await expectAccessible(page, "organization error");
    api.on(ORG_PATH, json(org()));
    await retry.click();
    await expect(region(page).getByLabel("Organization name")).toHaveValue("Analytical Engines");
  });

  test("read-only: a member reads the settings and gets no save control", async ({ page, api }) => {
    signedIn(api, withRole("member"));
    api.on(ORG_PATH, json(org({}, { project_access: "explicit" })));
    await page.goto(URL);
    await expect(region(page).getByLabel("Organization name")).toHaveAttribute("readonly", "");
    await expect(region(page).getByLabel("Allowed email domains")).toHaveAttribute(
      "placeholder",
      "None",
    );
    await expect(
      region(page).getByRole("radio", { name: "Only projects they're added to" }),
    ).toBeChecked();
    await expect(
      region(page).getByText("Only organization owners and admins change these settings."),
    ).toBeVisible();
    await expect(region(page).getByRole("button", { name: "Save settings" })).toHaveCount(0);
    await expectAccessible(page, "organization read-only");
  });

  test("without a control plane the page says which setting turns it on", async ({ page, api }) => {
    await page.goto(URL);
    await expect(page.getByText("Organization settings need a control plane.")).toBeVisible();
    await expect(page.getByText(/CRUCIBLE_POSTGRES_URL/)).toBeVisible();
    expect(api.count(ORG_PATH)).toBe(0);
    await expectAccessible(page, "organization without a control plane");
  });
});
