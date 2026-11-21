import type { Page, Request } from "@playwright/test";
import type { AuthProvider } from "../src/api/generated/AuthProvider";
import type { AuthProviderCreate } from "../src/api/generated/AuthProviderCreate";
import type { AuthProviderPatch } from "../src/api/generated/AuthProviderPatch";
import type { OrgRole } from "../src/api/generated/OrgRole";
import {
  CSRF,
  capture,
  expect,
  expectAccessible,
  json,
  PROJECT,
  PROVIDERS_PATH,
  problem,
  SERVER_TEXT,
  sessionInfo,
  signedIn,
  signInOptions,
  test,
} from "./fixtures";

const ORG_PROVIDERS_PATH = /\/api\/v1\/org\/auth-providers$/;
const URL = `/settings/sign-in/?project=${PROJECT}`;
const PUBLIC_URL = "https://crucible.example.com";
const SSO_ID = "0193a1b2-0000-7000-8000-0000000000c1";

const callback = (id: string) => `${PUBLIC_URL}/api/v1/auth/oauth/${id}/callback`;
const providerPath = (id: string) => new RegExp(`/api/v1/org/auth-providers/${id}$`);

function provider(fields: Partial<AuthProvider> = {}): AuthProvider {
  return {
    id: SSO_ID,
    kind: "oidc",
    display_name: "Continue with Acme SSO",
    issuer: "https://login.acme.example",
    client_id: "crucible-web",
    scopes: [],
    allowed_domains: ["acme.example"],
    enabled: true,
    seeded: false,
    redirect_uri: callback(SSO_ID),
    ...fields,
  };
}

const GITHUB_ID = "0193a1b2-0000-7000-8000-0000000000c2";
const GITHUB = provider({
  id: GITHUB_ID,
  kind: "github",
  display_name: "Continue with GitHub",
  issuer: "",
  client_id: "Iv1.0123456789abcdef",
  allowed_domains: [],
  redirect_uri: callback(GITHUB_ID),
});

function withRole(role: OrgRole) {
  const session = sessionInfo();
  return { ...session, org: { ...session.org, role } };
}

const region = (page: Page) => page.getByRole("region", { name: "Sign-in providers" });
const item = (page: Page, name: string) => page.locator(`[data-provider="${name}"]`);

test.describe("settings: sign-in providers", () => {
  test("loading, then success: each provider with its state, details and callback URL", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const release = api.hold(
      ORG_PROVIDERS_PATH,
      json({ data: [provider(), { ...GITHUB, enabled: false }] }),
    );
    await page.goto(URL);
    await expect(region(page).getByRole("status").first()).toHaveText("Loading sign-in providers");
    await expectAccessible(page, "sign-in providers loading");
    release();
    const sso = item(page, "Continue with Acme SSO");
    await expect(sso).toContainText("On");
    await expect(sso).toContainText("https://login.acme.example");
    await expect(sso).toContainText("crucible-web");
    await expect(sso).toContainText("Stored sealed; never shown");
    await expect(sso.getByText(callback(SSO_ID))).toBeVisible();
    await expect(item(page, "Continue with GitHub")).toContainText("Turned off");
    await expect(page.getByRole("link", { name: "Sign-in" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    await expectAccessible(page, "sign-in providers success");

    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await sso.getByRole("button", { name: "Copy callback URL of Continue with Acme SSO" }).click();
    await expect(sso.getByText("Copied.")).toBeVisible();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(callback(SSO_ID));
  });

  test("empty: first use invites the first provider, and without server.public_url the page says what to set", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(ORG_PROVIDERS_PATH, json({ data: [] }));
    await page.goto(URL);
    await expect(region(page).getByText("No sign-in providers yet.")).toBeVisible();
    await expect(region(page).getByRole("button", { name: "Add provider" })).toBeVisible();
    await expectAccessible(page, "sign-in providers empty");

    api.on(ORG_PROVIDERS_PATH, json({ data: [provider({ redirect_uri: "" })] }));
    await page.reload();
    await expect(
      region(page).getByText("The sign-in page offers no provider until server.public_url is set."),
    ).toBeVisible();
    await expect(item(page, "Continue with Acme SSO")).toContainText(
      "Not offered: server.public_url isn't set",
    );
    await expectAccessible(page, "sign-in providers without a public URL");
  });

  test("error: the section keeps the failure to itself, with a retry, and shows no server text", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(ORG_PROVIDERS_PATH, problem(503, "control_plane_unavailable"));
    await page.goto(URL);
    const retry = region(page).getByRole("button", { name: "Retry sign-in providers" });
    await expect(retry).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "sign-in providers error");
    api.on(ORG_PROVIDERS_PATH, json({ data: [provider()] }));
    await retry.click();
    await expect(item(page, "Continue with Acme SSO")).toContainText("On");
  });

  test("journey: an owner adds an OpenID Connect provider and it appears on the sign-in page", async ({
    page,
    api,
  }) => {
    signedIn(api);
    // The intercepted server keeps the providers the page adds, as the real one does.
    const stored: AuthProvider[] = [];
    const posts: Request[] = [];
    api.on(ORG_PROVIDERS_PATH, (route) => {
      if (route.request().method() === "POST") {
        posts.push(route.request());
        const body = route.request().postDataJSON() as AuthProviderCreate;
        const added = provider({
          display_name: body.display_name,
          issuer: body.issuer ?? "",
          client_id: body.client_id,
          scopes: body.scopes ?? [],
          allowed_domains: body.allowed_domains ?? [],
        });
        stored.push(added);
        return route.fulfill(json(added, 201));
      }
      return route.fulfill(json({ data: stored }));
    });
    api.on(PROVIDERS_PATH, (route) =>
      route.fulfill(
        json(
          signInOptions({
            providers: stored
              .filter((row) => row.enabled && row.redirect_uri !== "")
              .map(({ id, kind, display_name }) => ({ id, kind, display_name })),
          }),
        ),
      ),
    );

    await page.goto(URL);
    await expect(region(page).getByText("No sign-in providers yet.")).toBeVisible();
    await region(page).getByRole("button", { name: "Add provider" }).click();
    const dialog = page.getByRole("dialog", { name: "Add a sign-in provider" });
    await expect(dialog.getByLabel("Button text")).toHaveValue("Continue with GitHub");
    await dialog.getByRole("combobox", { name: "Provider" }).click();
    await page.getByRole("option", { name: "OpenID Connect" }).click();
    // Submitting with the required fields empty marks each one and sends nothing.
    await dialog.getByRole("button", { name: "Add OpenID Connect" }).click();
    for (const error of [
      "Enter the text for the sign-in button.",
      "Enter the issuer URL.",
      "Enter the client ID the provider issued.",
      "Enter the client secret the provider issued.",
    ]) {
      await expect(dialog.getByText(error)).toBeVisible();
    }
    expect(posts).toHaveLength(0);
    await expectAccessible(page, "add provider dialog with empty fields");
    await dialog.getByLabel("Button text").fill("Continue with Acme SSO");
    await dialog.getByLabel("Issuer URL").fill("https://login.acme.example");
    await dialog.getByLabel("Client ID").fill("crucible-web");
    await dialog.getByLabel("Client secret", { exact: true }).fill("s3cret-value");
    await dialog.getByLabel("Allowed email domains").fill("acme.example");
    await expectAccessible(page, "add provider dialog");
    await dialog.getByRole("button", { name: "Add OpenID Connect" }).click();

    const added = page.getByRole("dialog", { name: "Continue with Acme SSO is added" });
    await expect(added.getByText(callback(SSO_ID))).toBeVisible();
    await expect(
      added.getByText("Register this address with the provider as a redirect URI."),
    ).toBeVisible();
    await expectAccessible(page, "provider added");
    expect(posts).toHaveLength(1);
    expect(posts[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
    expect(posts[0]?.postDataJSON()).toEqual({
      kind: "oidc",
      display_name: "Continue with Acme SSO",
      issuer: "https://login.acme.example",
      client_id: "crucible-web",
      client_secret: "s3cret-value",
      allowed_domains: ["acme.example"],
    });
    await added.getByRole("button", { name: "Done" }).click();
    await expect(added).toBeHidden();
    const sso = item(page, "Continue with Acme SSO");
    await expect(sso).toContainText("On");
    await expect(sso.getByText(callback(SSO_ID))).toBeVisible();
    await expect(page.getByText("s3cret-value")).toHaveCount(0);

    capture(api, /\/api\/v1\/auth\/logout$/, { status: 204 });
    await page.getByRole("banner").getByRole("button", { name: "Sign out" }).click();
    await expect(page).toHaveURL(/\/login\/$/);
    await expect(page.getByRole("link", { name: "Continue with Acme SSO" })).toHaveAttribute(
      "href",
      `/api/v1/auth/oauth/${SSO_ID}/start?next=%2F`,
    );
    await expectAccessible(page, "sign-in page with the new provider");
  });

  test("replace secret, edit, turn off and delete each send what they change", async ({
    page,
    api,
  }) => {
    signedIn(api);
    let stored = [provider(), GITHUB];
    api.on(ORG_PROVIDERS_PATH, (route) => route.fulfill(json({ data: stored })));
    const patches: { body: unknown; csrf: string | undefined }[] = [];
    api.on(providerPath(SSO_ID), (route) => {
      const request = route.request();
      if (request.method() === "DELETE") {
        stored = stored.filter((row) => row.id !== SSO_ID);
        return route.fulfill({ status: 204 });
      }
      const body = request.postDataJSON() as AuthProviderPatch;
      patches.push({ body, csrf: request.headers()["x-crucible-csrf"] });
      stored = stored.map((row) =>
        row.id === SSO_ID
          ? {
              ...row,
              allowed_domains: body.allowed_domains ?? row.allowed_domains,
              enabled: body.enabled ?? row.enabled,
            }
          : row,
      );
      return route.fulfill(json(stored.find((row) => row.id === SSO_ID)));
    });
    await page.goto(URL);
    const sso = item(page, "Continue with Acme SSO");
    await expect(sso).toContainText("On");

    await sso.getByRole("button", { name: "Replace secret of Continue with Acme SSO" }).click();
    const replace = page.getByRole("dialog", {
      name: "New client secret for Continue with Acme SSO",
    });
    await replace.getByLabel("New client secret").fill("rotated-secret");
    await replace.getByRole("button", { name: "Save secret" }).click();
    await expect(page.getByRole("dialog", { name: "Client secret replaced" })).toBeVisible();
    await expectAccessible(page, "client secret replaced");
    await page.getByRole("button", { name: "Done" }).click();

    await sso.getByRole("button", { name: "Edit Continue with Acme SSO" }).click();
    const edit = page.getByRole("dialog", { name: "Change Continue with Acme SSO" });
    await expect(edit.getByLabel("Allowed email domains")).toHaveValue("acme.example");
    await edit.getByLabel("Allowed email domains").fill("acme.example, acme.test");
    await expectAccessible(page, "edit provider dialog");
    await edit.getByRole("button", { name: "Save changes" }).click();
    await expect(edit).toBeHidden();
    await expect(sso).toContainText("acme.example, acme.test");

    await sso.getByRole("button", { name: "Turn off Continue with Acme SSO" }).click();
    await expect(sso).toContainText("Turned off");
    expect(patches).toEqual([
      { body: { client_secret: "rotated-secret" }, csrf: CSRF },
      { body: { allowed_domains: ["acme.example", "acme.test"] }, csrf: CSRF },
      { body: { enabled: false }, csrf: CSRF },
    ]);

    await sso.getByRole("button", { name: "Delete Continue with Acme SSO" }).click();
    const question = page.getByRole("alertdialog", { name: "Delete Continue with Acme SSO?" });
    await expect(question).toContainText("This can't be undone.");
    await expectAccessible(page, "delete provider question");
    await question.getByRole("button", { name: "Delete provider" }).click();
    await expect(question).toBeHidden();
    await expect(sso).toHaveCount(0);
    await expect(item(page, "Continue with GitHub")).toBeVisible();
  });

  test("a member is told who manages providers, and nothing is requested", async ({
    page,
    api,
  }) => {
    signedIn(api, withRole("viewer"));
    await page.goto(URL);
    await expect(
      page.getByText("Only organization owners and admins manage sign-in providers."),
    ).toBeVisible();
    expect(api.count(ORG_PROVIDERS_PATH)).toBe(0);
    await expectAccessible(page, "sign-in providers for a viewer");
  });

  test("without a control plane the page says which setting turns it on", async ({ page, api }) => {
    await page.goto(URL);
    await expect(page.getByText("Sign-in providers need a control plane.")).toBeVisible();
    expect(api.count(ORG_PROVIDERS_PATH)).toBe(0);
  });
});
