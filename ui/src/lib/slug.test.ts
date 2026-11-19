import { expect, test } from "vitest";
import { SLUG_PATTERN, slugFromName } from "./slug";

test.each([
  ["Payments API", "payments-api"],
  ["  Checkout   Service  ", "checkout-service"],
  ["Café Ops", "cafe-ops"],
  ["R&D (EU)", "rd-eu"],
  ["Search - Staging", "search-staging"],
  ["already-a-slug", "already-a-slug"],
  ["Agents 2", "agents-2"],
  ["under_score.dot", "underscoredot"],
  ["!!!", ""],
  ["日本", ""],
])("%j suggests %j", (name, slug) => {
  expect(slugFromName(name)).toBe(slug);
});

test("a long name is cut to 64 characters, never ending in -", () => {
  expect(slugFromName("a".repeat(70))).toBe("a".repeat(64));
  expect(slugFromName(`${"x".repeat(63)} y`)).toBe("x".repeat(63));
});

test("every suggestion that isn't empty is a slug the server accepts", () => {
  const pattern = new RegExp(`^(?:${SLUG_PATTERN})$`, "v");
  for (const name of ["Payments API", "Café Ops", "R&D (EU)", "a".repeat(70), "-x-"]) {
    expect(slugFromName(name)).toMatch(pattern);
  }
});
