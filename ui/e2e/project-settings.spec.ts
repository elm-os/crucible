import type { Page, Request } from "@playwright/test";
import type { Project } from "../src/api/generated/Project";
import {
  type Api,
  CSRF,
  expect,
  expectAccessible,
  expectNoPageScrollX,
  json,
  PROJECT,
  PROJECT_B,
  PROJECTS_PATH,
  problem,
  type Reply,
  SERVER_TEXT,
  SIGN_IN_CHECK,
  sessionInfo,
  signedIn,
  test,
} from "./fixtures";

const URL = `/settings/project/?project=${PROJECT}`;
const PROJECT_PATH = new RegExp(`/api/v1/projects/${PROJECT}$`);

function project(fields: Partial<Project> = {}): Project {
  return { id: PROJECT, name: "Checkout", slug: "checkout", role: "admin", version: 3, ...fields };
}

const SEARCH = project({
  id: PROJECT_B,
  name: "Search",
  slug: "search",
  role: "viewer",
  version: 1,
});

// Answers each rename with `reply` and keeps the request.
function onRename(api: Api, reply: Reply): Request[] {
  const patches: Request[] = [];
  api.on(PROJECT_PATH, (route) => {
    patches.push(route.request());
    return route.fulfill(reply);
  });
  return patches;
}

const details = (page: Page) => page.getByRole("region", { name: "Project details" });
const projectMenu = (page: Page) =>
  page.getByRole("banner").getByRole("combobox", { name: "Project" });

test.describe("settings: project", () => {
  test("loading, then success: the name, slug and id, the id copied, and where the related settings are", async ({
    page,
    context,
    api,
  }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    signedIn(api);
    const release = api.hold(PROJECTS_PATH, json({ data: [project(), SEARCH] }));
    await page.goto(URL);
    const region = details(page);
    await expect(region).toHaveAttribute("aria-busy", "true");
    await expect(region.getByRole("status")).toHaveText("Loading project details");
    await expectAccessible(page, "project details loading");
    release();

    await expect(region.getByLabel("Project name")).toHaveValue("Checkout");
    await expect(region.getByText("checkout", { exact: true })).toBeVisible();
    await expect(region.getByText(PROJECT, { exact: true })).toBeVisible();
    await expect(region.getByRole("button", { name: "Rename" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await expect(
      page.getByRole("navigation", { name: "Settings" }).getByRole("link", { name: "Project" }),
    ).toHaveAttribute("aria-current", "page");
    const related = page.getByRole("region", { name: "Related settings" });
    await expect(related.getByRole("link", { name: "Retention" })).toHaveAttribute(
      "href",
      new RegExp(`^/settings/retention/?\\?project=${PROJECT}$`),
    );
    await expect(related).toContainText(
      "Organization owners and admins delete a whole project in the REST API, with DELETE /api/v1/projects/{project_id}.",
    );
    await expectAccessible(page, "project details success");

    await region.getByRole("button", { name: "Copy id" }).click();
    await expect(region.getByText("Copied.")).toBeVisible();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(PROJECT);
  });

  test("rename: Enter sends the name with If-Match and the CSRF token, and the project menu shows it", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(PROJECTS_PATH, json({ data: [project(), SEARCH] }));
    const patches = onRename(api, json(project({ name: "Payments", version: 4 })));
    await page.goto(URL);
    const name = details(page).getByLabel("Project name");
    await name.fill("Payments");
    await name.press("Enter");
    await expect(
      details(page).getByText("Renamed. The project menu shows the new name."),
    ).toBeVisible();
    await expect(projectMenu(page)).toHaveText("Payments");
    await expect(name).toHaveValue("Payments");
    expect(patches).toHaveLength(1);
    expect(patches[0]?.method()).toBe("PATCH");
    expect(patches[0]?.headers()["if-match"]).toBe('"3"');
    expect(patches[0]?.headers()["x-crucible-csrf"]).toBe(CSRF);
    expect(patches[0]?.postDataJSON()).toEqual({ name: "Payments" });
    await expectAccessible(page, "project renamed");
  });

  test("a rename after someone else's reloads the project and says so", async ({ page, api }) => {
    signedIn(api);
    let stored = [project(), SEARCH];
    api.on(PROJECTS_PATH, (route) => route.fulfill(json({ data: stored })));
    api.on(PROJECT_PATH, (route) => {
      stored = [project({ name: "Checkout EU", version: 4 }), SEARCH];
      return route.fulfill(problem(412, "precondition_failed", "req-stale"));
    });
    await page.goto(URL);
    const region = details(page);
    await region.getByLabel("Project name").fill("Payments");
    await region.getByRole("button", { name: "Rename" }).click();

    const alert = region.getByRole("alert");
    await expect(alert).toContainText("Someone renamed this project since the page loaded.");
    await expect(alert).toContainText("req-stale");
    await expect(region.getByLabel("Project name")).toHaveValue("Checkout EU");
    await expect(projectMenu(page)).toHaveText("Checkout EU");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "project renamed elsewhere");
  });

  test("a rename made in another session reaches the project menu once the page reads it", async ({
    page,
    api,
  }) => {
    signedIn(api);
    const release = api.hold(
      PROJECTS_PATH,
      json({ data: [project({ name: "Checkout EU", version: 4 }), SEARCH] }),
    );
    await page.goto(URL);
    await expect(projectMenu(page)).toHaveText("Checkout");
    release();
    await expect(details(page).getByLabel("Project name")).toHaveValue("Checkout EU");
    await expect(projectMenu(page)).toHaveText("Checkout EU");
    await projectMenu(page).click();
    await expect(page.getByRole("option")).toHaveText(["Checkout EU", "Search"]);
    await page.keyboard.press("Escape");
    await expectAccessible(page, "project renamed in another session");
  });

  test("a blank name is refused before sending, and a name the server refuses is marked", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(PROJECTS_PATH, json({ data: [project()] }));
    const patches = onRename(api, {
      status: 422,
      contentType: "application/problem+json",
      body: JSON.stringify({
        code: "validation_failed",
        status: 422,
        title: SERVER_TEXT,
        detail: SERVER_TEXT,
        request_id: "req-name",
        errors: [{ pointer: "/name", code: "invalid", message: SERVER_TEXT }],
      }),
    });
    await page.goto(URL);
    const region = details(page);
    const name = region.getByLabel("Project name");
    await name.fill("   ");
    await region.getByRole("button", { name: "Rename" }).click();
    await expect(region.getByText("Enter a name for the project.")).toBeVisible();
    expect(patches).toHaveLength(0);

    await name.fill("Payments");
    await region.getByRole("button", { name: "Rename" }).click();
    await expect(name).toHaveAttribute("aria-invalid", "true");
    await expect(region.getByText("Use 1 to 200 characters.")).toBeVisible();
    await expect(region.getByRole("alert")).toContainText("req-name");
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "project name refused");
  });

  test("read-only: a viewer sees the details and who renames the project, without the control", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(PROJECTS_PATH, json({ data: [project(), SEARCH] }));
    await page.goto(`/settings/project/?project=${PROJECT_B}`);
    const region = details(page);
    await expect(
      region.getByText("Only project admins rename a project. Ask an admin of Search."),
    ).toBeVisible();
    await expect(region.getByText("search", { exact: true })).toBeVisible();
    await expect(region.getByLabel("Project name")).toHaveCount(0);
    await expect(region.getByRole("button", { name: "Rename" })).toHaveCount(0);
    await expectAccessible(page, "project details read-only");
  });

  test("error: a failed list says so in its section, and the retry reads it again", async ({
    page,
    api,
  }) => {
    signedIn(api);
    api.on(PROJECTS_PATH, problem(503, "control_plane_unavailable", "req-projects"));
    await page.goto(URL);
    const region = details(page);
    await expect(region.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(region.getByText("req-projects")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expect(
      page
        .getByRole("region", { name: "Related settings" })
        .getByRole("link", { name: "Retention" }),
    ).toBeVisible();
    await expectAccessible(page, "project details error");

    api.on(PROJECTS_PATH, json({ data: [project()] }));
    await region.getByRole("button", { name: "Retry project details" }).click();
    await expect(region.getByLabel("Project name")).toHaveValue("Checkout");
  });

  test("empty: an organization without projects points to New project, and asks for no list", async ({
    page,
    api,
  }) => {
    signedIn(api, sessionInfo({ projects: [] }));
    await page.goto("/settings/project/");
    await expect(page.getByText("This organization has no projects yet.")).toBeVisible();
    await expect(page.getByRole("button", { name: "New project" })).toBeVisible();
    expect(api.count(PROJECTS_PATH)).toBe(0);
    await expectAccessible(page, "project details empty");
  });

  test("without a control plane the page says which setting turns it on, and asks for nothing", async ({
    page,
    api,
  }) => {
    await page.goto(URL);
    await expect(page.getByText("Project settings need a control plane.")).toBeVisible();
    await expect(page.getByText(/CRUCIBLE_POSTGRES_URL/)).toBeVisible();
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
    await expectAccessible(page, "project settings without a control plane");
  });
});

// The phone project runs only phone.spec.ts, so this surface sets the phone viewport itself.
test.describe("settings: project at 390 px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test("a long slug, the id and the routes wrap, and the page never scrolls sideways", async ({
    page,
    api,
  }) => {
    signedIn(api);
    // The longest slug, with no hyphen for a line to break at.
    const slug = "payments".repeat(8);
    api.on(PROJECTS_PATH, json({ data: [project({ slug })] }));
    await page.goto(URL);
    await expect(details(page).getByText(slug, { exact: true })).toBeVisible();
    await expect(details(page).getByText(PROJECT, { exact: true })).toBeVisible();
    await expectNoPageScrollX(page);
    await expectAccessible(page, "project details on a phone");
  });
});
