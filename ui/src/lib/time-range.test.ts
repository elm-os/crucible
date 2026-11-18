import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, test, vi } from "vitest";
import { parseRange, rangeStorageKey, rangeWindow, useTimeRange } from "./time-range";

// The router applies history.replaceState to useSearchParams in a transition. `routerLag` holds a
// query string the router still reports after the address bar has changed.
let routerLag: string | null = null;
vi.mock("next/navigation", () => ({
  useSearchParams: () => new URLSearchParams(routerLag ?? window.location.search),
}));

afterEach(() => {
  routerLag = null;
  vi.restoreAllMocks();
});

function visit(path: string) {
  window.history.replaceState(null, "", path);
}

describe("useTimeRange", () => {
  test("defaults to 24 hours", () => {
    const { result } = renderHook(() => useTimeRange("p1"));
    expect(result.current[0]).toBe("24h");
  });

  test("the URL wins over the range stored for the project", () => {
    localStorage.setItem(rangeStorageKey("p1"), "30d");
    visit("/traces/?range=7d");
    const { result } = renderHook(() => useTimeRange("p1"));
    expect(result.current[0]).toBe("7d");
  });

  test("an invalid URL value falls through to the stored range, then the default", () => {
    localStorage.setItem(rangeStorageKey("p1"), "30d");
    visit("/traces/?range=99y");
    expect(renderHook(() => useTimeRange("p1")).result.current[0]).toBe("30d");
    localStorage.setItem(rangeStorageKey("p1"), "forever");
    expect(renderHook(() => useTimeRange("p1")).result.current[0]).toBe("24h");
  });

  test("stored ranges are kept per project", () => {
    localStorage.setItem(rangeStorageKey("p1"), "30d");
    expect(renderHook(() => useTimeRange("p1")).result.current[0]).toBe("30d");
    expect(renderHook(() => useTimeRange("p2")).result.current[0]).toBe("24h");
  });

  test("choosing a range replaces the URL, keeps other params and stores it for the project", () => {
    visit("/trace/?project=p1&trace=abc");
    const replace = vi.spyOn(window.history, "replaceState");
    const push = vi.spyOn(window.history, "pushState");
    const length = window.history.length;
    const { result } = renderHook(() => useTimeRange("p1"));

    act(() => result.current[1]("1h"));

    expect(result.current[0]).toBe("1h");
    expect(replace).toHaveBeenCalledTimes(1);
    expect(push).not.toHaveBeenCalled();
    expect(window.history.length).toBe(length);
    expect(window.location.pathname).toBe("/trace/");
    expect(Object.fromEntries(new URLSearchParams(window.location.search))).toEqual({
      project: "p1",
      trace: "abc",
      range: "1h",
    });
    expect(localStorage.getItem(rangeStorageKey("p1"))).toBe("1h");
    expect(localStorage.getItem(rangeStorageKey("p2"))).toBeNull();
  });

  test("the choice shows at once, before the router reports the new URL", () => {
    visit("/traces/?range=24h");
    routerLag = "?range=24h";
    const { result, rerender } = renderHook(() => useTimeRange("p1"));

    act(() => result.current[1]("7d"));
    expect(result.current[0]).toBe("7d");

    routerLag = null;
    rerender();
    expect(result.current[0]).toBe("7d");

    visit("/traces/?range=30d");
    rerender();
    expect(result.current[0]).toBe("30d");
  });
});

describe("range helpers", () => {
  test("parseRange accepts only the presets", () => {
    expect(["1h", "24h", "7d", "30d"].map(parseRange)).toEqual(["1h", "24h", "7d", "30d"]);
    expect([null, undefined, "", "2h", "24H"].map(parseRange)).toEqual([
      null,
      null,
      null,
      null,
      null,
    ]);
  });

  test("rangeWindow ends at the next minute and is stable within a minute", () => {
    const now = Date.UTC(2026, 10, 17, 15, 4, 30, 250);
    expect(rangeWindow("1h", now)).toEqual({
      from: "2026-11-17T14:05:00.000000Z",
      to: "2026-11-17T15:05:00.000000Z",
    });
    expect(rangeWindow("1h", now + 20_000)).toEqual(rangeWindow("1h", now));
    expect(rangeWindow("30d", now).from).toBe("2026-10-18T15:05:00.000000Z");
  });
});
