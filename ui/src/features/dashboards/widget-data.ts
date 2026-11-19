import type { Meta } from "@/api/generated/Meta";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import type { MetricsSchema } from "@/api/generated/MetricsSchema";
import type { MetricsSeries } from "@/api/generated/MetricsSeries";
import type { SchemaMeasure } from "@/api/generated/SchemaMeasure";
import { formatCost, formatCount, formatDuration } from "@/lib/format";
import type { ChartSeries } from "@/primitives/chart/chart-model";

const compact = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
const percent = new Intl.NumberFormat("en-US", { style: "percent", maximumFractionDigits: 2 });

/** How a value of a unit reads. A rate in parts per million reads as a percentage. */
export function formatter(unit: string): (value: number) => string {
  if (unit === "usd") return formatCost;
  if (unit === "us") return (us) => formatDuration(us);
  if (unit === "ppm") return (ppm) => percent.format(ppm / 1_000_000);
  return formatCount;
}

/** A headline number: counts of ten thousand and more in compact form. */
export function formatHeadline(unit: string, value: number): string {
  if ((unit === "count" || unit === "tokens") && Math.abs(value) >= 10_000) {
    return compact.format(value);
  }
  return formatter(unit)(value);
}

export function catalogOf(schema: MetricsSchema | undefined, source: string): SchemaMeasure[] {
  return schema?.sources.find((s) => s.source === source)?.measures ?? [];
}

/** A measure's label from the schema, or its id while the schema is not at hand. */
export function measureLabel(catalog: readonly SchemaMeasure[], id: string): string {
  return catalog.find((m) => m.id === id)?.label ?? id;
}

export function isAdditive(catalog: readonly SchemaMeasure[], series: MetricsSeries): boolean {
  return catalog.find((m) => m.id === series.measure)?.additive ?? series.aggregation === "sum";
}

/** The words of a key: its dimension values, or "Other" for the remainder. */
export function keyLabel(series: MetricsSeries): string {
  if (series.other) return "Other";
  return series.key.map((k) => (k.value === "" ? "(none)" : k.value)).join(" · ");
}

/** A series' name in a legend: the measure, its percentile, and its key. */
export function seriesLabel(
  catalog: readonly SchemaMeasure[],
  series: MetricsSeries,
  measures: number,
): string {
  const measure = measureLabel(catalog, series.measure);
  const aggregation = series.aggregation === "sum" ? "" : ` ${series.aggregation}`;
  const name = `${measure}${aggregation}`;
  const keyed = series.other || series.key.length > 0;
  if (!keyed) return name;
  return measures > 1 ? `${name} · ${keyLabel(series)}` : keyLabel(series);
}

export function chartSeries(
  catalog: readonly SchemaMeasure[],
  response: MetricsResponse,
  measures: number,
): ChartSeries[] {
  return response.series.map((series) => ({
    id: `${series.measure}-${series.aggregation}-${series.other ? "other" : keyLabel(series)}`,
    label: seriesLabel(catalog, series, measures),
    points: series.points.map((p) => ({ t: Date.parse(p.bucket), v: p.value })),
  }));
}

/** Whether the series are percentiles of one measure: one ink family, told apart by dash. */
export function isFamily(response: MetricsResponse): boolean {
  const first = response.series[0];
  return (
    response.series.length > 1 &&
    first !== undefined &&
    response.series.every(
      (s) => s.measure === first.measure && s.aggregation !== "sum" && s.key.length === 0,
    )
  );
}

/**
 * One value for the time range: the sum of an additive measure, or, for a percentile, which buckets cannot be
 * added into, the newest bucket that has a value.
 */
export function reduce(series: MetricsSeries, additive: boolean): number | null {
  if (additive) return series.points.reduce((sum, p) => sum + (p.value ?? 0), 0);
  for (let i = series.points.length - 1; i >= 0; i -= 1) {
    const value = series.points[i]?.value;
    if (value !== null && value !== undefined) return value;
  }
  return null;
}

/** Whether the answer holds any data: a sum above zero, or any percentile at all. */
export function hasData(catalog: readonly SchemaMeasure[], response: MetricsResponse): boolean {
  return response.series.some((series) => {
    const value = reduce(series, isAdditive(catalog, series));
    return value !== null && value !== 0;
  });
}

export function isWholeMeta(meta: Meta): boolean {
  return !meta.partial && meta.consistency === "strong";
}

export interface KeyRow {
  key: string;
  label: string;
  other: boolean;
  values: (number | null)[];
}

/**
 * The rows of a table or a bar widget: one per key, in the order the server ranked them with `Other` last, and
 * one value per measure of the query.
 */
export function keyRows(
  catalog: readonly SchemaMeasure[],
  response: MetricsResponse,
  measures: readonly { measure: string; aggregation: string }[],
): KeyRow[] {
  const rows = new Map<string, KeyRow>();
  for (const series of response.series) {
    const label = series.other || series.key.length > 0 ? keyLabel(series) : "All";
    const key = series.other ? "\u0000other" : label;
    const row = rows.get(key) ?? {
      key,
      label,
      other: series.other,
      values: measures.map(() => null),
    };
    const column = measures.findIndex(
      (m) => m.measure === series.measure && m.aggregation === series.aggregation,
    );
    if (column >= 0) row.values[column] = reduce(series, isAdditive(catalog, series));
    rows.set(key, row);
  }
  return [...rows.values()].toSorted((a, b) => Number(a.other) - Number(b.other));
}
