import { describe, expect, test } from "vitest";
import {
  barPath,
  bucketTimes,
  chartModel,
  MARKER_NAMES,
  markerIndexes,
  mergeBuckets,
  mergeFactor,
  minStep,
  nearestBucket,
  regularize,
  seriesStyle,
  tableRows,
  timeTickFormat,
} from "./chart-model";

const LAYOUT = { width: 400, height: 100, top: 0, right: 0, bottom: 0, left: 0 };
const T0 = Date.parse("2026-11-17T09:00:00Z");
const MINUTE = 60_000;

const spans = {
  id: "count",
  label: "Spans",
  points: [
    { t: T0 + 2 * MINUTE, v: 4 },
    { t: T0, v: 2 },
    { t: T0 + MINUTE, v: null },
  ],
};
const errors = { id: "errors", label: "Errors", points: [{ t: T0 + 3 * MINUTE, v: 1 }] };

describe("the chart model", () => {
  test("buckets are the union of every series, ascending", () => {
    expect(bucketTimes([spans, errors])).toEqual([
      T0,
      T0 + MINUTE,
      T0 + 2 * MINUTE,
      T0 + 3 * MINUTE,
    ]);
  });

  test("scales span the buckets and start the values at zero", () => {
    const model = chartModel([spans, errors], LAYOUT, "line");
    expect(model.x(new Date(T0))).toBe(0);
    expect(model.x(new Date(T0 + 3 * MINUTE))).toBe(400);
    expect(model.y.domain()[0]).toBe(0);
    expect(model.y(4)).toBeLessThanOrEqual(model.y(2));
    // A bucket without a value breaks the line instead of drawing it at zero.
    expect(model.paths[0]?.match(/M/g)?.length).toBe(2);
    expect(nearestBucket(model, 260)).toBe(2);
  });

  test("areas fill down to zero, break where a bucket has no value, and only the area form has them", () => {
    const model = chartModel([spans, errors], LAYOUT, "area");
    expect(model.areas).toHaveLength(2);
    // Two runs of values (before and after the empty bucket), each closed along the zero line.
    const spansArea = model.areas[0] ?? "";
    expect(spansArea.match(/M/g)?.length).toBe(2);
    expect(spansArea.match(/Z/g)?.length).toBe(2);
    expect(spansArea).toContain(`,${model.y(0)}`);
    // The line is drawn over the area, as for the line form.
    expect(model.paths).toEqual(chartModel([spans, errors], LAYOUT, "line").paths);
    expect(chartModel([spans], LAYOUT, "line").areas).toEqual([]);
    expect(chartModel([spans], LAYOUT, "bar").areas).toEqual([]);
  });

  test("bars end at the end of the last bucket, and an empty chart still has a scale", () => {
    const bars = chartModel([spans], LAYOUT, "bar");
    expect(bars.x(new Date(T0 + 3 * MINUTE))).toBe(400);
    expect(bars.step).toBeCloseTo(400 / 3);
    const empty = chartModel([], LAYOUT, "line");
    expect(empty.times).toEqual([]);
    expect(empty.y.domain()).toEqual([0, 1]);
  });

  test("the table holds every bucket with each series' value", () => {
    expect(tableRows([spans, errors])).toEqual([
      { t: T0, values: [2, null] },
      { t: T0 + MINUTE, values: [null, null] },
      { t: T0 + 2 * MINUTE, values: [4, null] },
      { t: T0 + 3 * MINUTE, values: [null, 1] },
    ]);
  });

  test("each series has its own dash and marker; a family keeps one ink", () => {
    const styles = [0, 1, 2, 3].map((slot) => seriesStyle(slot, false));
    expect(new Set(styles.map((s) => s.dash)).size).toBe(4);
    expect(styles.map((s) => s.marker)).toEqual([...MARKER_NAMES]);
    expect(new Set([0, 1, 2].map((slot) => seriesStyle(slot, true).color))).toEqual(
      new Set(["var(--color-series-1)"]),
    );
  });

  test("markers are spread over at most twelve points", () => {
    expect(markerIndexes(5)).toEqual([0, 1, 2, 3, 4]);
    const spread = markerIndexes(288);
    expect(spread).toHaveLength(12);
    expect(spread[0]).toBe(0);
    expect(spread.at(-1)).toBe(287);
  });
});

describe("readable buckets", () => {
  const step = MINUTE;
  const sparse = {
    id: "p50",
    label: "p50",
    points: [
      { t: T0, v: 5 },
      { t: T0 + 3 * step, v: 7 },
    ],
  };

  test("a bucket the server left out becomes null, so a line breaks instead of joining across the gap", () => {
    expect(regularize([sparse], { step })[0]?.points.map((p) => p.v)).toEqual([5, null, null, 7]);
  });

  test("a missing bucket of a count is zero", () => {
    expect(regularize([sparse], { step, zero: true })[0]?.points.map((p) => p.v)).toEqual([
      5, 0, 0, 7,
    ]);
  });

  test("without a known width, the smallest gap between buckets is the width", () => {
    expect(minStep([T0, T0 + 2 * step, T0 + 3 * step])).toBe(step);
    expect(minStep([T0])).toBeNull();
  });

  test("buckets that are not on one grid are left as they are", () => {
    const offGrid = {
      id: "x",
      label: "x",
      points: [
        { t: T0, v: 1 },
        { t: T0 + step + 1, v: 2 },
      ],
    };
    expect(regularize([offGrid], { step })[0]?.points).toHaveLength(2);
  });

  test("thin bars merge into a whole clock interval, and wide ones stay", () => {
    // 288 five-minute buckets across 1,100 px would be under 4 px each: 30-minute bars instead.
    expect(mergeFactor(288, 1_100, 5 * step)).toBe(6);
    expect(mergeFactor(24, 540, 60 * step)).toBe(1);
  });

  test("merged buckets add up and start on the clock", () => {
    const at = Date.parse("2026-11-17T09:05:00Z");
    const calls = {
      id: "calls",
      label: "Calls",
      points: [
        { t: at, v: 1 },
        { t: at + 5 * step, v: 2 },
        { t: at + 30 * step, v: 4 },
      ],
    };
    expect(mergeBuckets([calls], 6, 5 * step)[0]?.points).toEqual([
      { t: Date.parse("2026-11-17T09:00:00Z"), v: 3 },
      { t: Date.parse("2026-11-17T09:30:00Z"), v: 4 },
    ]);
  });

  test("a bar is rounded at its data end and square at the baseline, and an empty one draws nothing", () => {
    const d = barPath(10, 20, 8, 100, 4);
    expect(d.startsWith("M10,100")).toBe(true);
    expect(d).toContain("Q10,20 14,20");
    expect(barPath(10, 100, 8, 100)).toBe("");
  });

  test("time ticks read as clock times within a day and as dates beyond it", () => {
    expect(timeTickFormat(24 * 3_600_000)(T0)).toContain(":");
    expect(timeTickFormat(7 * 24 * 3_600_000)(T0)).not.toContain(":");
  });

  test("the range form puts each bucket inside its own slot, as bars do", () => {
    const model = chartModel([spans], LAYOUT, "range");
    expect(model.x(new Date(T0))).toBe(0);
    expect(model.step).toBeGreaterThan(0);
    expect(model.paths).toHaveLength(1);
  });
});
