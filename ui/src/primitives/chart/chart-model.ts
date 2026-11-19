import { type ScaleLinear, type ScaleTime, scaleLinear, scaleUtc } from "d3-scale";
import { line, symbol, symbolCircle, symbolDiamond, symbolSquare, symbolTriangle } from "d3-shape";

export interface ChartPoint {
  /** The first instant of the bucket, in milliseconds. */
  t: number;
  /** The value; null for a bucket without one. */
  v: number | null;
}

export interface ChartSeries {
  id: string;
  label: string;
  points: ChartPoint[];
}

export type ChartForm = "line" | "bar";

// Two of the four series colors are neutral, so every series also has its own dash and marker.
export const MAX_SERIES = 4;
const MARKERS = [symbolCircle, symbolSquare, symbolTriangle, symbolDiamond] as const;
export const MARKER_NAMES = ["circle", "square", "triangle", "diamond"] as const;

export interface SeriesStyle {
  color: string;
  dash: string;
  marker: (typeof MARKER_NAMES)[number];
}

// A family (percentiles of one measure) is drawn in ink and told apart by dash and marker alone.
export function seriesStyle(slot: number, family: boolean): SeriesStyle {
  const index = slot % MAX_SERIES;
  return {
    color: family ? "var(--color-series-1)" : `var(--color-series-${index + 1})`,
    dash: `var(--dash-series-${index + 1})`,
    marker: MARKER_NAMES[index] ?? "circle",
  };
}

export function markerPath(slot: number, size = 36): string {
  return symbol(MARKERS[slot % MAX_SERIES] ?? symbolCircle, size)() ?? "";
}

export interface ChartLayout {
  width: number;
  height: number;
  top: number;
  right: number;
  bottom: number;
  left: number;
}

export interface Tick {
  value: number;
  position: number;
}

export interface ChartModel {
  times: number[];
  x: ScaleTime<number, number>;
  y: ScaleLinear<number, number>;
  /** The width of one bucket in pixels, for bars and hit testing. */
  step: number;
  paths: string[];
  xTicks: Tick[];
  yTicks: Tick[];
}

/** The buckets of every series, ascending and without repeats. */
export function bucketTimes(series: readonly ChartSeries[]): number[] {
  return [...new Set(series.flatMap((s) => s.points.map((p) => p.t)))].toSorted((a, b) => a - b);
}

export function chartModel(
  series: readonly ChartSeries[],
  layout: ChartLayout,
  form: ChartForm,
): ChartModel {
  const times = bucketTimes(series);
  const first = times[0] ?? 0;
  const last = times.at(-1) ?? first;
  const width = times.length > 1 ? (last - first) / (times.length - 1) : 60_000;
  // Bars sit inside their bucket, so the axis runs to the end of the last one.
  const end = form === "bar" ? last + width : Math.max(last, first + 1);
  const x = scaleUtc()
    .domain([new Date(first), new Date(end)])
    .range([layout.left, layout.width - layout.right]);
  const values = series.flatMap((s) => s.points.flatMap((p) => (p.v === null ? [] : [p.v])));
  const max = Math.max(0, ...values);
  const y = scaleLinear()
    .domain([0, max > 0 ? max : 1])
    .nice(4)
    .range([layout.height - layout.bottom, layout.top]);
  const path = line<ChartPoint>()
    .defined((p) => p.v !== null)
    .x((p) => x(new Date(p.t)))
    .y((p) => y(p.v ?? 0));
  return {
    times,
    x,
    y,
    step: Math.max(1, x(new Date(first + width)) - x(new Date(first))),
    paths: series.map((s) => path(s.points.toSorted((a, b) => a.t - b.t)) ?? ""),
    xTicks: x.ticks(4).map((d) => ({ value: d.getTime(), position: x(d) })),
    yTicks: y.ticks(4).map((v) => ({ value: v, position: y(v) })),
  };
}

/** The bucket nearest to a horizontal position. */
export function nearestBucket(model: ChartModel, position: number): number | null {
  if (model.times.length === 0) return null;
  let best = 0;
  for (let i = 1; i < model.times.length; i += 1) {
    const at = model.x(new Date(model.times[i] ?? 0));
    const bestAt = model.x(new Date(model.times[best] ?? 0));
    if (Math.abs(at - position) < Math.abs(bestAt - position)) best = i;
  }
  return best;
}

/** The value of each series at a bucket. */
export function valuesAt(series: readonly ChartSeries[], t: number): (number | null)[] {
  return series.map((s) => s.points.find((p) => p.t === t)?.v ?? null);
}

/** Every bucket with the value of each series: the chart's table view. */
export function tableRows(
  series: readonly ChartSeries[],
): { t: number; values: (number | null)[] }[] {
  return bucketTimes(series).map((t) => ({ t, values: valuesAt(series, t) }));
}

/** At most `count` evenly spread indexes of `length` points: where markers are drawn. */
export function markerIndexes(length: number, count = 12): number[] {
  if (length <= count) return Array.from({ length }, (_, i) => i);
  const every = (length - 1) / (count - 1);
  return Array.from({ length: count }, (_, i) => Math.round(i * every));
}
