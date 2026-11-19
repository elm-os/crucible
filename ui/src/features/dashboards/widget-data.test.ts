import { describe, expect, test } from "vitest";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import type { MetricsSeries } from "@/api/generated/MetricsSeries";
import type { SchemaMeasure } from "@/api/generated/SchemaMeasure";
import {
  formatHeadline,
  formatter,
  hasData,
  isFamily,
  keyRows,
  reduce,
  seriesLabel,
} from "./widget-data";

const CATALOG: SchemaMeasure[] = [
  {
    id: "count",
    label: "Spans",
    unit: "count",
    aggregations: ["sum"],
    additive: true,
    tier: "rollup",
  },
  {
    id: "duration",
    label: "Duration",
    unit: "us",
    aggregations: ["p95"],
    additive: false,
    tier: "rollup",
  },
];

function series(fields: Partial<MetricsSeries> & { values: (number | null)[] }): MetricsSeries {
  const { values, ...rest } = fields;
  return {
    measure: "count",
    aggregation: "sum",
    unit: "count",
    key: [],
    other: false,
    points: values.map((value, i) => ({ bucket: `2026-11-17T1${i}:00:00.000000Z`, value })),
    ...rest,
  };
}

const at = (value: string) => [{ dimension: "environment", value }];

function response(all: MetricsSeries[]): MetricsResponse {
  return {
    series: all,
    granularity: "hour",
    truncated: false,
    source_tier: "rollup",
    as_of: "2026-11-17T14:00:05.000000Z",
    partial_last_bucket: false,
    saturated: false,
    meta: { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" },
  };
}

describe("widget data", () => {
  test("a sum adds its buckets; a percentile reads its newest bucket with a value", () => {
    expect(reduce(series({ values: [1, null, 4] }), true)).toBe(5);
    expect(reduce(series({ values: [7, 9, null] }), false)).toBe(9);
    expect(reduce(series({ values: [null, null] }), false)).toBeNull();
  });

  test("no data means no sum above zero and no percentile at all", () => {
    expect(hasData(CATALOG, response([series({ values: [0, 0] })]))).toBe(false);
    expect(hasData(CATALOG, response([series({ values: [0, 2] })]))).toBe(true);
    const p95 = series({
      measure: "duration",
      aggregation: "p95",
      unit: "us",
      values: [null, 1200],
    });
    expect(hasData(CATALOG, response([p95]))).toBe(true);
  });

  test("labels name the measure, its percentile and its key", () => {
    const keyed = series({ key: [{ dimension: "model", value: "gpt-4.1" }], values: [1] });
    expect(seriesLabel(CATALOG, series({ values: [1] }), 1)).toBe("Spans");
    expect(seriesLabel(CATALOG, keyed, 1)).toBe("gpt-4.1");
    expect(seriesLabel(CATALOG, keyed, 2)).toBe("Spans · gpt-4.1");
    expect(seriesLabel(CATALOG, series({ other: true, values: [1] }), 1)).toBe("Other");
    const p95 = series({ measure: "duration", aggregation: "p95", values: [1] });
    expect(seriesLabel(CATALOG, p95, 1)).toBe("Duration p95");
    expect(seriesLabel([], p95, 1)).toBe("duration p95");
  });

  test("percentiles of one measure are one family; a split or a sum is not", () => {
    const p = (aggregation: string) =>
      series({ measure: "duration", aggregation, unit: "us", values: [1] });
    expect(isFamily(response([p("p50"), p("p95")]))).toBe(true);
    expect(isFamily(response([p("p50")]))).toBe(false);
    expect(isFamily(response([series({ values: [1] }), p("p95")]))).toBe(false);
  });

  test("a table has one row per key with Other last, and one value per measure", () => {
    const rows = keyRows(
      CATALOG,
      response([
        series({ other: true, values: [1, 1] }),
        series({ key: at("prod"), values: [3, 4] }),
        series({ measure: "duration", aggregation: "p95", key: at("prod"), values: [5, null] }),
        series({ key: at(""), values: [2] }),
      ]),
      [
        { measure: "count", aggregation: "sum" },
        { measure: "duration", aggregation: "p95" },
      ],
    );
    expect(rows.map((r) => [r.label, r.values])).toEqual([
      ["prod", [7, 5]],
      ["(none)", [2, null]],
      ["Other", [2, null]],
    ]);
  });

  test("values read in their unit, and headline counts in compact form", () => {
    expect(formatter("usd")(1.5)).toBe("$1.50");
    expect(formatter("us")(2_500)).toBe("3 ms");
    expect(formatter("ppm")(12_500)).toBe("1.25%");
    expect(formatHeadline("count", 9_999)).toBe("9,999");
    expect(formatHeadline("count", 300_060)).toBe("300.1K");
    expect(formatHeadline("usd", 12_000)).toBe("$12,000.00");
  });
});
