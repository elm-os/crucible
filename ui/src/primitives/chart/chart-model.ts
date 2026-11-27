import { type ScaleLinear, type ScaleTime, scaleLinear, scaleUtc } from "d3-scale";
import {
  area,
  line,
  symbol,
  symbolCircle,
  symbolDiamond,
  symbolSquare,
  symbolTriangle,
} from "d3-shape";

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

export type ChartForm = "line" | "bar" | "area";

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
  /** The area of each series down to zero, for the area form; empty for the others. */
  areas: string[];
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
  const fill = area<ChartPoint>()
    .defined((p) => p.v !== null)
    .x((p) => x(new Date(p.t)))
    .y0(y(0))
    .y1((p) => y(p.v ?? 0));
  const sorted = series.map((s) => s.points.toSorted((a, b) => a.t - b.t));
  return {
    times,
    x,
    y,
    step: Math.max(1, x(new Date(first + width)) - x(new Date(first))),
    paths: sorted.map((points) => path(points) ?? ""),
    areas: form === "area" ? sorted.map((points) => fill(points) ?? "") : [],
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

/** The smallest gap between neighbouring buckets: the bucket width when some buckets are missing. */
export function minStep(times: readonly number[]): number | null {
  let step: number | null = null;
  for (let i = 1; i < times.length; i += 1) {
    const gap = (times[i] ?? 0) - (times[i - 1] ?? 0);
    if (gap > 0 && (step === null || gap < step)) step = gap;
  }
  return step;
}

/**
 * Every bucket from the first to the last, `step` apart. A bucket the server left out becomes null, so a
 * line breaks across the gap instead of drawing a stroke over time that had no data, or zero for counts.
 * Series whose buckets don't sit on one grid are returned as they are.
 */
export function regularize(
  series: readonly ChartSeries[],
  options: { step?: number | null; zero?: boolean } = {},
): ChartSeries[] {
  const times = bucketTimes(series);
  const step = options.step ?? minStep(times);
  const first = times[0];
  const last = times.at(-1);
  if (step === null || step <= 0 || first === undefined || last === undefined) return [...series];
  if (times.some((t) => (t - first) % step !== 0)) return [...series];
  const count = Math.floor((last - first) / step) + 1;
  if (count > 2_000) return [...series];
  return series.map((s) => {
    const byTime = new Map(s.points.map((p) => [p.t, p.v]));
    const points = Array.from({ length: count }, (_, i) => {
      const t = first + i * step;
      const v = byTime.get(t) ?? null;
      return { t, v: v === null && options.zero ? 0 : v };
    });
    return { ...s, points };
  });
}

const NICE_WIDTHS = [
  60_000,
  5 * 60_000,
  10 * 60_000,
  15 * 60_000,
  30 * 60_000,
  3_600_000,
  2 * 3_600_000,
  3 * 3_600_000,
  6 * 3_600_000,
  12 * 3_600_000,
  86_400_000,
  2 * 86_400_000,
  7 * 86_400_000,
];

/**
 * How many neighbouring buckets to add together so each bar is at least `minWidth` pixels wide. With the bucket
 * width known, the merged width is a whole clock interval (15 min, 30 min, 1 h, ...), so bars start on the hour.
 */
export function mergeFactor(
  buckets: number,
  plotWidth: number,
  step: number | null = null,
  minWidth = 16,
): number {
  if (buckets <= 1 || plotWidth <= 0) return 1;
  const raw = Math.max(1, Math.ceil((buckets * minWidth) / plotWidth));
  if (raw <= 1 || step === null || step <= 0) return raw;
  const nice = NICE_WIDTHS.find((width) => width >= raw * step && width % step === 0);
  return nice === undefined ? raw : nice / step;
}

/**
 * Adds neighbouring buckets together, `factor` at a time: only for counts and sums, whose buckets add up. With
 * the bucket width known, groups line up with the clock (a 30-minute bar covers :00 to :30).
 */
export function mergeBuckets(
  series: readonly ChartSeries[],
  factor: number,
  step: number | null = null,
): ChartSeries[] {
  if (factor <= 1) return [...series];
  const times = bucketTimes(series);
  const width = step !== null && step > 0 ? step * factor : null;
  const groupOf = (t: number, i: number) =>
    width === null ? Math.floor(i / factor) : Math.floor(t / width);
  const groups = new Map<number, number[]>();
  times.forEach((t, i) => {
    const key = groupOf(t, i);
    const group = groups.get(key);
    if (group) group.push(t);
    else groups.set(key, [t]);
  });
  return series.map((s) => {
    const byTime = new Map(s.points.map((p) => [p.t, p.v]));
    const points: ChartPoint[] = [...groups.entries()].map(([key, group]) => {
      const values = group.flatMap((t) => {
        const v = byTime.get(t);
        return v === null || v === undefined ? [] : [v];
      });
      const t = width === null ? (group[0] ?? 0) : key * width;
      return { t, v: values.length === 0 ? null : values.reduce((a, b) => a + b, 0) };
    });
    return { ...s, points };
  });
}

/** A bar from the baseline up to `top`, its data end rounded and its base square. */
export function barPath(
  x: number,
  top: number,
  width: number,
  baseline: number,
  radius = 4,
): string {
  const height = baseline - top;
  if (height <= 0 || width <= 0) return "";
  const r = Math.min(radius, width / 2, height);
  return [
    `M${x},${baseline}`,
    `V${top + r}`,
    `Q${x},${top} ${x + r},${top}`,
    `H${x + width - r}`,
    `Q${x + width},${top} ${x + width},${top + r}`,
    `V${baseline}`,
    "Z",
  ].join("");
}

const HOUR = 3_600_000;
const clock = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const dayOfMonth = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });

/** How a time-axis tick reads: the clock time within about a day and a half, else the date. */
export function timeTickFormat(span: number): (t: number) => string {
  return span <= 36 * HOUR ? (t) => clock.format(t) : (t) => dayOfMonth.format(t);
}
