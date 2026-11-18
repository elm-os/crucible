import type { Page, Request } from "@playwright/test";
import type { ApiKey } from "../src/api/generated/ApiKey";
import type { Invitation } from "../src/api/generated/Invitation";
import type { OrgMember } from "../src/api/generated/OrgMember";
import {
  CSRF,
  capture,
  expect,
  expectAccessible,
  json,
  PROJECT,
  PROJECT_B,
  problem,
  SERVER_TEXT,
  SIGN_IN_CHECK,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const MEMBERS_PATH = /\/api\/v1\/org\/members$/;
const INVITATIONS_PATH = /\/api\/v1\/org\/invitations$/;
const KEYS_PATH = new RegExp(`/api/v1/projects/${PROJECT}/api-keys$`);
const SELF = sessionInfo().user.id;

function member(fields: Partial<OrgMember>): OrgMember {
  return {
    user_id: "u-2",
    email: "grace@example.com",
    name: "Grace Hopper",
    role: "member",
    status: "active",
    ...fields,
  };
}

const MEMBERS = [
  member({ user_id: SELF, email: "ada@example.com", name: "Ada Lovelace", role: "owner" }),
  member({}),
  member({ user_id: "u-3", email: "edsger@example.com", name: "Edsger Dijkstra", role: "admin" }),
];

function invitation(fields: Partial<Invitation>): Invitation {
  return {
    id: "i-1",
    email: "barbara@example.com",
    org_role: "viewer",
    project_roles: [],
    expires_at: "2099-01-01T00:00:00.000000Z",
    accepted_at: null,
    revoked_at: null,
    ...fields,
  };
}

function apiKey(fields: Partial<ApiKey>): ApiKey {
  return {
    id: "k-1",
    public_key: `cpk_${"a".repeat(43)}`,
    scopes: ["ingest", "read", "write"],
    note: "checkout-service",
    expires_at: null,
    last_used_at: "2026-11-17T14:00:00.000000Z",
    revoked_at: null,
    version: 3,
    ...fields,
  };
}

const membersRegion = (page: Page) => page.getByRole("region", { name: "Members" });
const invitationsRegion = (page: Page) => page.getByRole("region", { name: "Invitations" });
const keysRegion = (page: Page) => page.getByRole("region", { name: "API keys for Checkout" });

test.describe("settings: members", () => {
  test("loading, then success: members and invitations, controls only where the owner may change things", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const release = api.hold(MEMBERS_PATH, json({ data: MEMBERS }));
    api.on(INVITATIONS_PATH, json({ data: [invitation({})] }));
    await page.goto("/settings/members/");
    const members = membersRegion(page);
    await expect(members).toHaveAttribute("aria-busy", "true");
    await expect(members.getByRole("status")).toHaveText("Loading members");
    // The invitations list doesn't wait for the members list.
    await expect(
      invitationsRegion(page).getByRole("cell", { name: "barbara@example.com", exact: true }),
    ).toBeVisible();
    await expectAccessible(page, "members loading");
    release();

    await expect(members.locator("tbody tr")).toHaveCount(3);
    await expect(members.getByRole("row", { name: /Ada Lovelace/ })).toContainText("(you)");
    await expect(members.getByRole("combobox", { name: "Role for Ada Lovelace" })).toHaveCount(0);
    await expect(members.getByRole("button", { name: "Remove Ada Lovelace" })).toHaveCount(0);
    await expect(members.getByRole("combobox", { name: "Role for Grace Hopper" })).toHaveText(
      "Member",
    );
    await expect(
      page.getByRole("navigation", { name: "Settings" }).getByRole("link", { name: "Members" }),
    ).toHaveAttribute("aria-current", "page");
    await expectAccessible(page, "members success");
  });

  test("a role change sends the new role with the session's CSRF token", async ({ page, api }) => {
    signedIn(api);
    api.on(MEMBERS_PATH, json({ data: MEMBERS }));
    api.on(INVITATIONS_PATH, json({ data: [] }));
    const patches = capture(api, /\/api\/v1\/org\/members\/u-2$/, json(member({ role: "admin" })));
    await page.goto("/settings/members/");
    const role = membersRegion(page).getByRole("combobox", { name: "Role for Grace Hopper" });
    await role.click();
    await page.getByRole("option", { name: "Admin" }).click();
    await expect(role).toHaveText("Admin");
    expect(patches[0]?.method()).toBe("PATCH");
    expect(patches[0]?.postDataJSON()).toEqual({ role: "admin" });
    expect(patches[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
  });

  test("remove: Cancel keeps the member; confirming removes them", async ({ page, api }) => {
    signedIn(api);
    api.on(MEMBERS_PATH, json({ data: MEMBERS }));
    api.on(INVITATIONS_PATH, json({ data: [] }));
    const deletes = capture(api, /\/api\/v1\/org\/members\/u-2$/, { status: 204 });
    await page.goto("/settings/members/");
    const trigger = membersRegion(page).getByRole("button", { name: "Remove Grace Hopper" });
    await trigger.focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("alertdialog", { name: "Remove Grace Hopper?" });
    await expect(dialog.getByRole("button", { name: "Cancel" })).toBeFocused();
    await expectAccessible(page, "remove member dialog");
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(trigger).toBeFocused();
    await trigger.click();
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toBeHidden();
    await expect(trigger).toBeFocused();
    expect(deletes).toHaveLength(0);

    api.on(MEMBERS_PATH, json({ data: MEMBERS.filter((row) => row.user_id !== "u-2") }));
    await trigger.click();
    await dialog.getByRole("button", { name: "Remove member" }).click();
    await expect(dialog).toBeHidden();
    await expect(
      membersRegion(page).getByRole("cell", { name: "Grace Hopper", exact: true }),
    ).toHaveCount(0);
    expect(deletes[0]?.method()).toBe("DELETE");
    expect(deletes[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
  });

  test("invite: the link shows once, and is gone after Done", async ({ page, api }) => {
    signedIn(api);
    api.on(MEMBERS_PATH, json({ data: MEMBERS }));
    api.on(INVITATIONS_PATH, json({ data: [] }));
    const posts: Request[] = [];
    api.on(INVITATIONS_PATH, (route) => {
      if (route.request().method() !== "POST") return route.fulfill(json({ data: [] }));
      posts.push(route.request());
      return route.fulfill(
        json(
          {
            invitation: invitation({ email: "barbara@example.com" }),
            accept_path: "/invite/?token=tok-1",
          },
          201,
        ),
      );
    });
    await page.goto("/settings/members/");
    await expect(invitationsRegion(page).getByText("No invitations yet.")).toBeVisible();
    await page.getByRole("button", { name: "Invite member" }).click();
    const dialog = page.getByRole("dialog", { name: "Invite a member" });
    await dialog.getByLabel("Email").fill("barbara@example.com");
    await dialog.getByRole("combobox", { name: "Role" }).click();
    await page.getByRole("option", { name: "Viewer" }).click();
    await expectAccessible(page, "invite dialog");
    await dialog.getByRole("button", { name: "Create invitation" }).click();

    const link = `${new URL(page.url()).origin}/invite/?token=tok-1`;
    const shown = page.getByRole("dialog", { name: "Send this link to barbara@example.com" });
    await expect(shown.getByText(link)).toBeVisible();
    await expect(shown).toContainText("doesn't send email yet");
    await expectAccessible(page, "invitation link");
    expect(posts[0]?.postDataJSON()).toEqual({ email: "barbara@example.com", org_role: "viewer" });
    expect(posts[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);

    await shown.getByRole("button", { name: "Done" }).click();
    await expect(page.getByText(link)).toHaveCount(0);
    await page.getByRole("button", { name: "Invite member" }).click();
    await expect(page.getByRole("dialog", { name: "Invite a member" })).toBeVisible();
    await expect(page.getByText(link)).toHaveCount(0);
  });

  test("revoke an invitation; an accepted one offers nothing", async ({ page, api }) => {
    signedIn(api);
    api.on(MEMBERS_PATH, json({ data: MEMBERS }));
    api.on(
      INVITATIONS_PATH,
      json({
        data: [
          invitation({}),
          invitation({
            id: "i-2",
            email: "alan@example.com",
            accepted_at: "2026-11-01T00:00:00.000000Z",
          }),
        ],
      }),
    );
    const deletes = capture(api, /\/api\/v1\/org\/invitations\/i-1$/, { status: 204 });
    await page.goto("/settings/members/");
    const invitations = invitationsRegion(page);
    await expect(invitations.getByRole("row", { name: /alan@example\.com/ })).toContainText(
      "Accepted",
    );
    await expect(invitations.getByRole("button", { name: /alan@example\.com/ })).toHaveCount(0);
    await invitations
      .getByRole("button", { name: "Revoke the invitation for barbara@example.com" })
      .click();
    await page
      .getByRole("alertdialog", { name: "Revoke the invitation for barbara@example.com?" })
      .getByRole("button", { name: "Revoke invitation" })
      .click();
    await expect.poll(() => deletes.length).toBe(1);
    expect(deletes[0]?.method()).toBe("DELETE");
  });

  test("error: a failed members list is an in-section error whose retry refetches only that list", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(MEMBERS_PATH, problem(503, "control_plane_unavailable", "req-members"));
    api.on(INVITATIONS_PATH, json({ data: [invitation({})] }));
    await page.goto("/settings/members/");
    const members = membersRegion(page);
    await expect(members.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(members.getByText("req-members")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(
      invitationsRegion(page).getByRole("cell", { name: "barbara@example.com", exact: true }),
    ).toBeVisible();
    await expectAccessible(page, "members error");
    const invitationReads = api.count(INVITATIONS_PATH);

    api.on(MEMBERS_PATH, json({ data: MEMBERS }));
    await members.getByRole("button", { name: "Retry members" }).click();
    await expect(members.locator("tbody tr")).toHaveCount(3);
    expect(api.count(INVITATIONS_PATH)).toBe(invitationReads);
  });

  test("members and viewers see both lists without controls", async ({ page, api }) => {
    const session = sessionInfo();
    signedIn(api, { ...session, org: { ...session.org, role: "viewer" } });
    api.on(MEMBERS_PATH, json({ data: MEMBERS }));
    api.on(INVITATIONS_PATH, json({ data: [invitation({})] }));
    await page.goto("/settings/members/");
    await expect(membersRegion(page).locator("tbody tr")).toHaveCount(3);
    await expect(page.getByRole("main").getByRole("combobox")).toHaveCount(0);
    await expect(page.getByRole("main").getByRole("button")).toHaveCount(0);
    await expectAccessible(page, "members read-only");
  });

  test("without a control plane the page says which setting adds members, and asks for nothing", async ({
    page,
    api,
  }) => {
    await page.goto("/settings/members/");
    await expect(page.getByText("Members need a control plane.")).toBeVisible();
    await expect(page.getByText(/postgres\.url/)).toBeVisible();
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
    await expectAccessible(page, "members without a control plane");
  });
});

test.describe("settings: API keys", () => {
  const KEYS_URL = `/settings/api-keys/?project=${PROJECT}`;

  test("loading, then success: the project's keys", async ({ page, api }) => {
    signedIn(api);
    const release = api.hold(
      KEYS_PATH,
      json({
        data: [
          apiKey({}),
          apiKey({ id: "k-2", note: "old", revoked_at: "2026-11-01T00:00:00.000000Z" }),
        ],
      }),
    );
    await page.goto(KEYS_URL);
    const keys = keysRegion(page);
    await expect(keys.getByRole("status")).toHaveText("Loading API keys");
    await expectAccessible(page, "keys loading");
    release();
    await expect(keys.locator("tbody tr")).toHaveCount(2);
    await expect(keys.getByRole("row", { name: /checkout-service/ })).toContainText("Active");
    await expect(keys.getByRole("row", { name: /old/ })).toContainText("Revoked");
    await expect(keys.getByRole("button", { name: /^Revoke/ })).toHaveCount(1);
    await expectAccessible(page, "keys success");
  });

  test("create: the secret shows once with the public key, and is gone after Done", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const secret = `csk_${"s".repeat(43)}`;
    const created = apiKey({ id: "k-2", public_key: `cpk_${"b".repeat(43)}`, note: "batch" });
    const posts: string[] = [];
    api.on(KEYS_PATH, (route) => {
      if (route.request().method() !== "POST") return route.fulfill(json({ data: [] }));
      posts.push(route.request().postData() ?? "");
      return route.fulfill(json({ key: created, secret_key: secret }, 201));
    });
    await page.goto(KEYS_URL);
    await expect(page.getByText("No API keys yet.")).toBeVisible();
    await page.getByRole("button", { name: "Create key" }).click();
    const dialog = page.getByRole("dialog", { name: "Create an API key for Checkout" });
    await dialog.getByLabel("Note").fill("batch");
    await dialog.getByRole("button", { name: "Create key" }).click();

    const shown = page.getByRole("dialog", { name: "Copy your secret key" });
    await expect(shown.getByText(secret)).toBeVisible();
    await expect(shown.getByText(created.public_key)).toBeVisible();
    await expect(shown).toContainText("You won't see this secret key again");
    await expectAccessible(page, "new key");
    expect(JSON.parse(posts[0] ?? "{}")).toEqual({ note: "batch" });

    await shown.getByRole("button", { name: "Done" }).click();
    await expect(page.getByText(secret)).toHaveCount(0);
    await page.getByRole("button", { name: "Create key" }).click();
    await expect(
      page.getByRole("dialog", { name: "Create an API key for Checkout" }),
    ).toBeVisible();
    await expect(page.getByText(secret)).toHaveCount(0);
  });

  test("revoke sends the key's version as If-Match", async ({ page, api }) => {
    signedIn(api);
    api.on(KEYS_PATH, json({ data: [apiKey({})] }));
    const deletes = capture(api, new RegExp(`/projects/${PROJECT}/api-keys/k-1$`), { status: 204 });
    await page.goto(KEYS_URL);
    await keysRegion(page)
      .getByRole("button", { name: `Revoke ${apiKey({}).public_key}` })
      .click();
    const dialog = page.getByRole("alertdialog", { name: 'Revoke the key "checkout-service"?' });
    await expectAccessible(page, "revoke key dialog");
    await dialog.getByRole("button", { name: "Revoke key" }).click();
    await expect(dialog).toBeHidden();
    expect(deletes[0]?.headers()["if-match"]).toBe('"3"');
    expect(deletes[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
  });

  test("error: a failed list says so, and its retry works", async ({ page, api }) => {
    signedIn(api);
    api.on(KEYS_PATH, problem(503, "control_plane_unavailable", "req-keys"));
    await page.goto(KEYS_URL);
    const keys = keysRegion(page);
    await expect(keys.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(keys.getByText("req-keys")).toBeVisible();
    await expectAccessible(page, "keys error");
    api.on(KEYS_PATH, json({ data: [apiKey({})] }));
    await keys.getByRole("button", { name: "Retry API keys" }).click();
    await expect(keys.locator("tbody tr")).toHaveCount(1);
  });

  test("without the manage permission the page explains, and asks for no keys", async ({
    page,
    api,
  }) => {
    signedIn(api);
    await page.goto(`/settings/api-keys/?project=${PROJECT_B}`);
    await expect(page.getByText("Only project admins manage API keys.")).toBeVisible();
    expect(api.calls.filter((call) => call.includes("/api-keys"))).toEqual([]);
    await expectAccessible(page, "keys without permission");
  });
});
