import { expect, expectAccessible, PROJECT, problem, SIGN_IN_CHECK, test } from "./fixtures";

// Home has no section that fetches yet: the set-up steps and the tiles are static until the data-bounds
// and metrics screens land, so their only state is the rendered one, and no failing API can blank them.
test.describe("home", () => {
  test("static: set-up steps and planned tiles render with no telemetry request", async ({
    page,
    api,
  }) => {
    api.on(/\/api\/v1\/projects\//, problem(503, "telemetry_store_unavailable"));
    await page.goto(`/home/?project=${PROJECT}`);
    const setup = page.getByRole("region", { name: "Set up" });
    await expect(setup.getByRole("listitem")).toHaveCount(4);
    await expect(setup).toContainText("Send your first trace");
    for (const tile of ["Traces", "Error rate", "p95 latency", "Cost", "Tokens"]) {
      await expect(page.getByRole("region", { name: tile, exact: true })).toContainText("Planned");
    }
    expect(api.calls).toEqual([SIGN_IN_CHECK]);
    await expectAccessible(page, "home");
  });
});
