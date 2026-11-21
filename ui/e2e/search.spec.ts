import type { SearchHit } from "../src/api/generated/SearchHit";
import type { SearchPage } from "../src/api/generated/SearchPage";
import {
  AS_OF,
  expect,
  expectAccessible,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  SIGN_IN_CHECK,
  span,
  TRACE,
  TRACE_PATH,
  test,
  traceDetail,
} from "./fixtures";

const SEARCH_PATH = new RegExp(`/projects/${PROJECT}/search$`);

function hit(id: string, name: string, fragments: [string, boolean][]): SearchHit {
  return {
    trace_id: TRACE,
    span: span(id, null, 0, 1_000, { name, observation_type: "generation" }),
    matches: [
      {
        field: "input",
        fragments: fragments.map(([text, highlighted]) => ({ text, highlighted })),
        cut_before: false,
        cut_after: true,
      },
    ],
  };
}

function page(data: SearchHit[], next: string | null = null): SearchPage {
  return {
    data,
    next_cursor: next,
    has_more: next !== null,
    meta: { as_of: AS_OF, partial: false, consistency: "strong" },
  };
}

const searchUrl = (query = "") => `/search/?project=${PROJECT}${query}`;

test.describe("search: results", () => {
  test("idle: without a query nothing is asked and the page says what to do", async ({
    page: browser,
    api,
  }) => {
    await browser.goto(searchUrl());
    await expect(browser.getByText("Type what to find.")).toBeVisible();
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
    await expectAccessible(browser, "search idle");
  });

  test("loading: the results section is busy and announced while the search runs", async ({
    page: browser,
    api,
  }) => {
    const release = api.hold(
      SEARCH_PATH,
      json(page([hit("a000000000000002", "plan reply", [["refund", true]])])),
    );
    await browser.goto(searchUrl("&q=refund"));
    const section = browser.getByRole("region", { name: "Results" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    await expect(section.getByRole("status")).toHaveText("Loading results");
    await expectAccessible(browser, "search loading");
    release();
    await expect(browser.getByRole("link", { name: "plan reply" })).toBeVisible();
  });

  test("success: hits mark what matched and open their span; more pages load on request", async ({
    page: browser,
    api,
  }) => {
    api.on(SEARCH_PATH, async (route) => {
      const body = route.request().postDataJSON() as { cursor?: string; query: string };
      expect(body.query).toBe("refund policy");
      await route.fulfill(
        body.cursor === "page-2"
          ? json(page([hit("a000000000000008", "send_reply", [["refund", true]])]))
          : json(
              page(
                [
                  hit("a000000000000002", "plan reply", [
                    ["The ", false],
                    ["refund", true],
                    [" policy", false],
                  ]),
                ],
                "page-2",
              ),
            ),
      );
    });
    await browser.goto(searchUrl());
    await browser.getByRole("searchbox", { name: "Find" }).fill("refund policy");
    await browser.getByRole("button", { name: "Search" }).click();
    const results = browser.getByRole("list", { name: "Matching spans" });
    await expect(results).toContainText("The refund policy…");
    await expect(results.locator("mark")).toHaveText(["refund"]);
    await expect(browser).toHaveURL(/q=refund\+policy/);
    await expectAccessible(browser, "search success");
    await browser.getByRole("button", { name: "Load more results" }).click();
    await expect(browser.getByRole("link", { name: "send_reply" })).toBeVisible();
    await expect(browser.getByRole("button", { name: "Load more results" })).toHaveCount(0);
    // The link opens the trace with the span selected.
    api.on(TRACE_PATH, json(traceDetail()));
    await browser.getByRole("link", { name: "plan reply" }).click();
    await expect(browser).toHaveURL(
      new RegExp(`/trace/\\?project=${PROJECT}&trace=${TRACE}&span=a000000000000002`),
    );
    await expect(browser.getByRole("row", { name: /plan reply/ })).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });

  test("error: a failed search shows the mapped copy and a retry that searches again", async ({
    page: browser,
    api,
  }) => {
    api.on(SEARCH_PATH, problem(503, "telemetry_store_unavailable", "req-search"));
    await browser.goto(searchUrl("&q=refund"));
    const section = browser.getByRole("region", { name: "Results" });
    await expect(section.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(section.getByText("req-search")).toBeVisible();
    await expect(browser.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(browser, "search error");
    api.on(SEARCH_PATH, json(page([hit("a000000000000002", "plan reply", [["refund", true]])])));
    await section.getByRole("button", { name: "Retry results" }).click();
    await expect(browser.getByRole("link", { name: "plan reply" })).toBeVisible();
  });

  test("empty: no match says so and offers a wider range, which searches again", async ({
    page: browser,
    api,
  }) => {
    api.on(SEARCH_PATH, json(page([])));
    await browser.goto(searchUrl("&q=nothing"));
    await expect(browser.getByText('No spans match "nothing" in the last 24 hours.')).toBeVisible();
    await expectAccessible(browser, "search empty");
    await browser.getByRole("button", { name: "Search the last 30 days" }).click();
    await expect(browser.getByText('No spans match "nothing".')).toBeVisible();
    await expect(browser).toHaveURL(/range=30d/);
    expect(api.count(SEARCH_PATH)).toBe(2);
  });

  test("filters: the list filter bar narrows the search, lives in the URL, and an empty answer offers to clear it", async ({
    page: browser,
    api,
  }) => {
    const bodies: { filters?: Record<string, unknown> }[] = [];
    api.on(SEARCH_PATH, async (route) => {
      const body = route.request().postDataJSON() as { filters?: { environment?: string[] } };
      bodies.push(body);
      const dev = body.filters?.environment?.includes("dev") ?? false;
      await route.fulfill(
        json(page(dev ? [] : [hit("a000000000000002", "plan reply", [["refund", true]])])),
      );
    });
    await browser.goto(searchUrl("&q=refund"));
    await expect(browser.getByRole("link", { name: "plan reply" })).toBeVisible();
    expect(bodies[0]).not.toHaveProperty("filters");

    const bar = browser.getByRole("form", { name: "Filter results" });
    await bar.getByRole("combobox", { name: "Filter by" }).click();
    await browser.getByRole("option", { name: "Environment" }).click();
    await bar.getByRole("textbox", { name: "Value" }).fill("prod");
    await bar.getByRole("button", { name: "Add filter" }).click();
    await expect(browser.getByRole("button", { name: "Remove filter Environment: prod" })).toBeVisible();
    await expect(browser).toHaveURL(/environment=prod/);
    await expect.poll(() => bodies.at(-1)?.filters).toEqual({ environment: ["prod"] });
    await expect(browser.getByRole("link", { name: "plan reply" })).toBeVisible();
    await expectAccessible(browser, "search with filters");

    await browser.goto(searchUrl("&q=refund&environment=dev"));
    const results = browser.getByRole("region", { name: "Results" });
    await expect(
      results.getByText('No spans match "refund" and these filters in the last 24 hours.'),
    ).toBeVisible();
    await expectAccessible(browser, "search empty under filters");
    await results.getByRole("button", { name: "Clear filters" }).click();
    await expect(browser.getByRole("link", { name: "plan reply" })).toBeVisible();
    await expect(browser).not.toHaveURL(/environment=/);
    expect(bodies.at(-1)).not.toHaveProperty("filters");
  });
});
