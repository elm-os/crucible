"use client";

import { useQuery } from "@tanstack/react-query";
import { queryMetrics } from "@/api/client";
import type { MeasureRequest } from "@/api/generated/MeasureRequest";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import type { MetricsSeries } from "@/api/generated/MetricsSeries";
import { type Range, rangeWindow } from "@/lib/time-range";
import type { ChartSeries } from "@/primitives/chart/chart-model";

export type HomeQuery = "traces" | "cost" | "tokens" | "errors" | "latency";

const MEASURES: Record<HomeQuery, { source: string; measures: MeasureRequest[] }> = {
  traces: { source: "traces", measures: [{ measure: "traces", aggregation: "sum" }] },
  cost: {
    source: "spans",
    measures: [
      { measure: "total_cost", aggregation: "sum" },
      { measure: "count", aggregation: "sum" },
    ],
  },
  tokens: {
    source: "spans",
    measures: [
      { measure: "total_tokens", aggregation: "sum" },
      { measure: "count", aggregation: "sum" },
    ],
  },
  errors: {
    source: "spans",
    measures: [
      { measure: "count", aggregation: "sum" },
      { measure: "error_count", aggregation: "sum" },
    ],
  },
  latency: {
    source: "spans",
    measures: [
      { measure: "duration", aggregation: "p50" },
      { measure: "duration", aggregation: "p95" },
    ],
  },
};

// One query per headline: a tile and the chart that shares its numbers read the same cache entry.
export function useHomeMetrics(project: string, range: Range, which: HomeQuery) {
  return useQuery({
    queryKey: [project, "metrics", which, range],
    queryFn: ({ signal }) =>
      queryMetrics(project, { ...MEASURES[which], ...rangeWindow(range, Date.now()) }, signal),
  });
}

export function total(series: MetricsSeries | undefined): number {
  return (series?.points ?? []).reduce((sum, point) => sum + (point.value ?? 0), 0);
}

/** The value of the newest bucket that has one. */
export function latest(series: MetricsSeries | undefined): number | null {
  const points = series?.points ?? [];
  for (let i = points.length - 1; i >= 0; i -= 1) {
    const value = points[i]?.value;
    if (value !== null && value !== undefined) return value;
  }
  return null;
}

/** Whether the answer holds no spans (or, for traces, no traces) in the range. */
export function noData(response: MetricsResponse, which: HomeQuery): boolean {
  if (which === "latency") return response.series.every((s) => latest(s) === null);
  const counted = which === "traces" || which === "errors" ? 0 : 1;
  return total(response.series[counted]) === 0;
}

export function chartSeries(series: MetricsSeries, label: string): ChartSeries {
  return {
    id: `${series.measure}-${series.aggregation}`,
    label,
    points: series.points.map((point) => ({ t: Date.parse(point.bucket), v: point.value })),
  };
}

export function errorRate(response: MetricsResponse): number | null {
  const spans = total(response.series[0]);
  return spans === 0 ? null : total(response.series[1]) / spans;
}
