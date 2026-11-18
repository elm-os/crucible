import { expect, test } from "vitest";
import { type PageRequests, withFirstPage, withNextPage, withRestart } from "./child-pages";

const none: PageRequests = new Map();

test("the first page is requested once per span", () => {
  const once = withFirstPage(none, "a");
  expect([...once]).toEqual([["a", [null]]]);
  expect(withFirstPage(once, "a")).toBe(once);
});

test("each next page is requested once, after the pages before it", () => {
  const first = withFirstPage(none, "a");
  const second = withNextPage(first, "a", "c1");
  expect(second.get("a")).toEqual([null, "c1"]);
  expect(withNextPage(second, "a", "c1")).toBe(second);
  expect(withNextPage(second, "a", "c2").get("a")).toEqual([null, "c1", "c2"]);
  expect(first.get("a")).toEqual([null]);
});

test("a restart goes back to the first page of that span only", () => {
  const requests = withNextPage(withFirstPage(withFirstPage(none, "a"), "b"), "a", "c1");
  const restarted = withRestart(requests, "a");
  expect(restarted.get("a")).toEqual([null]);
  expect(restarted.get("b")).toEqual([null]);
});
