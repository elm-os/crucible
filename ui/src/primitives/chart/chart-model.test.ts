import { describe, expect, test } from "vitest";
import {
  bucketTimes,
  chartModel,
  MARKER_NAMES,
  markerIndexes,
  nearestBucket,
  seriesStyle,
  tableRows,
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
