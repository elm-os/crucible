import { describe, expect, test } from "vitest";
import type { SchemaMeasure } from "@/api/generated/SchemaMeasure";
import type { WidgetKind } from "@/api/generated/WidgetKind";
import type { WidgetLayout } from "@/api/generated/WidgetLayout";
import {
  byReadingOrder,
  canPlace,
  chartSeries,
  firstFree,
  fits,
  KINDS,
  maxChartKeys,
  minSize,
  overlaps,
  type Placed,
  shapeProblem,
} from "./rules";

const CATALOG: SchemaMeasure[] = [
  { id: "count", label: "Spans", unit: "count", aggregations: ["sum"], additive: true, tier: "rollup" },
  { id: "duration", label: "Duration", unit: "us", aggregations: ["p50", "p95"], additive: false, tier: "rollup" },
];

// A small deterministic generator, so a failing case reruns the same.
function generator(seed: number) {
  let state = seed;
  return (below: number) => {
    state = (state * 1_103_515_245 + 12_345) % 2_147_483_648;
    return state % below;
  };
}

const at = (x: number, y: number, w: number, h: number): WidgetLayout => ({ x, y, w, h });

describe("rectangles", () => {
  test("overlap means a shared cell; touching edges and empty rectangles share none", () => {
    expect(overlaps(at(0, 0, 4, 2), at(3, 1, 4, 2))).toBe(true);
    expect(overlaps(at(0, 0, 4, 2), at(4, 0, 4, 2))).toBe(false);
    expect(overlaps(at(0, 0, 4, 2), at(0, 2, 4, 2))).toBe(false);
    expect(overlaps(at(5, 0, 0, 1), at(0, 0, 10, 1))).toBe(false);
  });

  // The same model as the server's property test: the cells each rectangle covers.
  test("overlap agrees with the cells two rectangles cover", () => {
    const next = generator(7);
    const cells = (r: WidgetLayout) => {
      const set = new Set<string>();
      for (let x = r.x; x < r.x + r.w; x += 1) for (let y = r.y; y < r.y + r.h; y += 1) set.add(`${x},${y}`);
      return set;
    };
    for (let i = 0; i < 2_000; i += 1) {
      const a = at(next(14), next(20), next(15), next(15));
      const b = at(next(14), next(20), next(15), next(15));
      const shared = [...cells(a)].some((cell) => cells(b).has(cell));
      expect(overlaps(a, b), JSON.stringify({ a, b })).toBe(shared);
    }
  });

  test("a widget fits between its kind's smallest size and 12 by 12, inside 12 columns", () => {
    expect(fits("timeseries", at(0, 0, 3, 2))).toBe(true);
    expect(fits("timeseries", at(0, 0, 2, 2))).toBe(false);
    expect(fits("stat", at(0, 0, 2, 1))).toBe(true);
    expect(fits("table", at(10, 0, 3, 2))).toBe(false);
    expect(fits("markdown", at(0, 0, 13, 1))).toBe(false);
    expect(fits("bar", at(0, 598, 3, 2))).toBe(true);
    expect(fits("bar", at(0, 599, 3, 2))).toBe(false);
    for (const kind of KINDS) expect(fits(kind, { x: 0, y: 0, ...minSize(kind) })).toBe(true);
  });
});

describe("placing", () => {
  const widgets: Placed[] = [
    { id: "a", kind: "timeseries", layout: at(0, 0, 6, 3) },
    { id: "b", kind: "stat", layout: at(6, 0, 3, 2) },
  ];

  test("a widget may move where it overlaps only itself", () => {
    expect(canPlace(widgets, "a", "timeseries", at(0, 1, 6, 3))).toBe(true);
    expect(canPlace(widgets, "a", "timeseries", at(1, 0, 6, 3))).toBe(false);
    expect(canPlace(widgets, "b", "stat", at(9, 0, 3, 2))).toBe(true);
  });

  test("a new widget takes the first free place, row by row", () => {
    expect(firstFree(widgets, { w: 3, h: 2 })).toEqual(at(9, 0, 3, 2));
    expect(firstFree(widgets, { w: 6, h: 3 })).toEqual(at(6, 2, 6, 3));
    expect(firstFree([], { w: 12, h: 12 })).toEqual(at(0, 0, 12, 12));
  });

  // Whatever is already there, the place found is free and inside the grid.
  test("the first free place never overlaps", () => {
    const next = generator(11);
    for (let round = 0; round < 300; round += 1) {
      const placed: Placed[] = [];
      for (let i = 0; i < 8; i += 1) {
        const kind = KINDS[next(KINDS.length)] as WidgetKind;
        const size = { w: 2 + next(5), h: 1 + next(4) };
        const layout = firstFree(placed, size);
        expect(layout).not.toBeNull();
        if (!layout) break;
        expect(placed.some((p) => overlaps(p.layout, layout))).toBe(false);
        expect(layout.x + layout.w).toBeLessThanOrEqual(12);
        placed.push({ id: String(i), kind, layout });
      }
    }
  });

  test("reading order is by row, then by column", () => {
    const sorted = [
      { id: "c", layout: at(0, 2, 2, 1) },
      { id: "b", layout: at(4, 0, 2, 1) },
      { id: "a", layout: at(0, 0, 2, 1) },
    ].toSorted(byReadingOrder);
    expect(sorted.map((w) => w.id)).toEqual(["a", "b", "c"]);
  });
});

describe("query shapes", () => {
  const count = { measure: "count", aggregation: "sum" };
  const p95 = { measure: "duration", aggregation: "p95" };

  test("a chart counts each key of each measure, and `other` for an additive one", () => {
    expect(chartSeries({ measures: [count, count], dimensions: [], topN: null }, CATALOG)).toBe(2);
    expect(chartSeries({ measures: [count], dimensions: ["model"], topN: 3 }, CATALOG)).toBe(4);
    expect(chartSeries({ measures: [p95], dimensions: ["model"], topN: 4 }, CATALOG)).toBe(4);
    expect(chartSeries({ measures: [count], dimensions: ["model"], topN: null }, CATALOG)).toBe(21);
  });

  test("the most keys a chart takes keeps it at four series", () => {
    expect(maxChartKeys([count], CATALOG)).toBe(3);
    expect(maxChartKeys([p95], CATALOG)).toBe(4);
    expect(maxChartKeys([count, count], CATALOG)).toBe(1);
    for (const measures of [[count], [p95], [count, p95], [count, count]]) {
      const keys = maxChartKeys(measures, CATALOG);
      const shape = { measures, dimensions: ["model"], topN: keys };
      expect(chartSeries(shape, CATALOG)).toBeLessThanOrEqual(4);
      expect(chartSeries({ ...shape, topN: keys + 1 }, CATALOG)).toBeGreaterThan(4);
    }
  });

  test("each kind takes its own shapes", () => {
    const shape = (measures: number, dimensions: number, topN: number | null = 3) => ({
      measures: Array.from({ length: measures }, () => count),
      dimensions: ["model", "level"].slice(0, dimensions),
      topN,
    });
    expect(shapeProblem("stat", shape(1, 0), CATALOG)).toBeNull();
    expect(shapeProblem("stat", shape(2, 0), CATALOG)).toBe("Choose one measure.");
    expect(shapeProblem("stat", shape(1, 1), CATALOG)).toMatch(/remove the grouping/);
    expect(shapeProblem("bar", shape(1, 0), CATALOG)).toMatch(/Group by at least one/);
    expect(shapeProblem("bar", shape(1, 2, 20), CATALOG)).toBeNull();
    expect(shapeProblem("table", shape(4, 2, 20), CATALOG)).toBeNull();
    expect(shapeProblem("timeseries", shape(1, 1, 3), CATALOG)).toBeNull();
    expect(shapeProblem("timeseries", shape(1, 1, 4), CATALOG)).toMatch(/at most 4 series/);
  });
});
