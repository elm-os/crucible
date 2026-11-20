import type { Page, Request } from "@playwright/test";
import {
  type Api,
  CSRF,
  expect,
  expectAccessible,
  json,
  PROJECT,
  PROJECTS_PATH,
  projectList,
  problem,
  type Reply,
  SERVER_TEXT,
  SESSION_PATH,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const NEW_ID = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a3d";
const START = `/settings/project/?project=${PROJECT}`;

// New project posts to the path the project page reads its list from, so `reply` answers POST only.
function onCreate(api: Api, reply: Reply): Request[] {
  const posts: Request[] = [];
  api.on(PROJECTS_PATH, (route) => {
    if (route.request().method() !== "POST") return route.fulfill(json(projectList()));
    posts.push(route.request());
    return route.fulfill(reply);
  });
  return posts;
}

async function openDialog(page: Page) {
  await page.getByRole("button", { name: "New project" }).click();
  return page.getByRole("dialog", { name: "New project" });
}

test.describe("settings: new project", () => {
  test("create: the slug follows the name, and the new project opens on its API keys page", async ({
    page,
    api,
  }) => {
    signedIn(api);
    await page.goto(START);
    const dialog = await openDialog(page);
    const name = dialog.getByLabel("Name", { exact: true });
    await expect(name).toBeFocused();
    await name.fill("Payments API");
    await expect(dialog.getByLabel("Slug")).toHaveValue("payments-api");
    await expectAccessible(page, "new project dialog");

    const posts = onCreate(
      api,
      json(
        { id: NEW_ID, name: "Payments API", slug: "payments-api", role: "admin", version: 1 },
        201,
      ),
    );
    const session = sessionInfo();
    api.on(
      SESSION_PATH,
      json({
        ...session,
        projects: [
          ...session.projects,
          {
            id: NEW_ID,
            name: "Payments API",
            slug: "payments-api",
            role: "admin",
            permissions: ["read", "manage"],
          },
        ],
      }),
    );
    api.on(new RegExp(`/api/v1/projects/${NEW_ID}/api-keys$`), json({ data: [] }));
    await dialog.getByRole("button", { name: "Create project" }).click();

    await expect(page).toHaveURL(new RegExp(`/settings/api-keys/?\\?project=${NEW_ID}$`));
    const keys = page.getByRole("region", { name: "API keys for Payments API" });
    await expect(keys.getByText("No API keys yet.")).toBeVisible();
    await expect(keys.getByRole("button", { name: "Create key" })).toBeVisible();
    await expect(page.getByRole("banner").getByRole("combobox", { name: "Project" })).toHaveText(
      "Payments API",
    );
    await expect(dialog).toBeHidden();
    expect(posts[0]?.postDataJSON()).toEqual({ name: "Payments API", slug: "payments-api" });
    expect(posts[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
    await expectAccessible(page, "new project's API keys");
  });

  test("error: a taken slug says how to fix it, and the page stays", async ({ page, api }) => {
    signedIn(api);
    await page.goto(START);
    const dialog = await openDialog(page);
    await dialog.getByLabel("Name", { exact: true }).fill("Checkout");
    onCreate(api, problem(409, "conflict", "req-taken"));
    await dialog.getByRole("button", { name: "Create project" }).click();

    const alert = dialog.getByRole("alert");
    await expect(alert).toContainText("Another project already uses that slug.");
    await expect(alert).toContainText("req-taken");
    await expect(dialog.getByLabel("Slug")).toHaveAttribute("aria-invalid", "true");
    await expect(dialog.getByText("Choose a slug no other project uses.")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(page).toHaveURL(/\/settings\/project\//);
    await expectAccessible(page, "new project with a taken slug");
  });

  test("members and viewers don't get New project", async ({ page, api }) => {
    const session = sessionInfo();
    signedIn(api, { ...session, org: { ...session.org, role: "member" } });
    await page.goto(START);
    await expect(page.getByRole("navigation", { name: "Settings" })).toBeVisible();
    await expect(page.getByRole("button", { name: "New project" })).toHaveCount(0);
  });
});
