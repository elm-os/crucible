import { describe, expect, test } from "vitest";
import { loginHref, safeNext } from "./next-path";

describe("next keeps only paths on this server (contract 11.4)", () => {
  test.each(["/", "/traces/?project=p1&range=7d", "/settings/members/", "/apiary"])(
    "%s is kept",
    (value) => {
      expect(safeNext(value)).toBe(value);
    },
  );

  test.each([
    null,
    undefined,
    "",
    "traces",
    "https://evil.example/",
    "javascript:alert(1)",
    "//evil.example/x",
    "/\\evil.example",
    "/\t/evil.example",
    "/a\nb",
    "/\u0000",
    "/api",
    "/api/v1/auth/session",
    "/api?x=1",
  ])("%j becomes /", (value) => {
    expect(safeNext(value)).toBe("/");
  });
});

test("the sign-in link carries a safe next, and none for the root", () => {
  expect(loginHref("/traces/?project=p1&range=7d")).toBe(
    "/login/?next=%2Ftraces%2F%3Fproject%3Dp1%26range%3D7d",
  );
  expect(loginHref("/")).toBe("/login/");
  expect(loginHref("//evil.example")).toBe("/login/");
});
