import type { MeasureRequest } from "@/api/generated/MeasureRequest";
import type { SchemaMeasure } from "@/api/generated/SchemaMeasure";
import type { WidgetKind } from "@/api/generated/WidgetKind";
import type { WidgetLayout } from "@/api/generated/WidgetLayout";

// The rules the server checks on every dashboard change (crates/crucible-api/src/dashboards.rs). The editor
// keeps its draft inside them, so a save is refused only when someone else changed the dashboard.

export const GRID_COLUMNS = 12;
export const GRID_ROWS = 600;
export const MAX_SIZE = 12;
export const MAX_WIDGETS = 50;
export const MAX_CHART_SERIES = 4;
export const MAX_SERIES_KEYS = 20;
export const MAX_TITLE = 120;
export const MAX_NAME = 120;
export const MAX_DESCRIPTION = 1_000;
export const MAX_TEXT = 10_000;

export const KINDS: readonly WidgetKind[] = ["timeseries", "stat", "table", "bar", "markdown"];

export const KIND_LABEL: Record<WidgetKind, string> = {
  timeseries: "Time series",
  stat: "Stat",
  table: "Table",
  bar: "Bar",
  markdown: "Text",
};

export function minSize(kind: WidgetKind): { w: number; h: number } {
  return kind === "stat" || kind === "markdown" ? { w: 2, h: 1 } : { w: 3, h: 2 };
}

/** The size a new widget of a kind starts at. */
export function startSize(kind: WidgetKind): { w: number; h: number } {
  if (kind === "stat") return { w: 3, h: 2 };
  if (kind === "markdown") return { w: 4, h: 2 };
  if (kind === "table") return { w: 6, h: 4 };
  return { w: 6, h: 3 };
}

export function fits(kind: WidgetKind, layout: WidgetLayout): boolean {
  const min = minSize(kind);
  return (
    layout.w >= min.w &&
    layout.w <= MAX_SIZE &&
    layout.h >= min.h &&
    layout.h <= MAX_SIZE &&
    layout.x >= 0 &&
    layout.y >= 0 &&
    layout.x + layout.w <= GRID_COLUMNS &&
    layout.y + layout.h <= GRID_ROWS
  );
}

/** Whether two rectangles share a cell. An empty rectangle shares none. */
export function overlaps(a: WidgetLayout, b: WidgetLayout): boolean {
  const meet = (a0: number, aLen: number, b0: number, bLen: number) =>
    Math.max(a0, b0) < Math.min(a0 + aLen, b0 + bLen);
  return meet(a.x, a.w, b.x, b.w) && meet(a.y, a.h, b.y, b.h);
}

export interface Placed {
  id: string;
  kind: WidgetKind;
  layout: WidgetLayout;
}

/** Whether `layout` is a place `id` may take: it fits its kind and overlaps none of the others. */
export function canPlace(
  widgets: readonly Placed[],
  id: string,
  kind: WidgetKind,
  layout: WidgetLayout,
): boolean {
  return fits(kind, layout) && widgets.every((w) => w.id === id || !overlaps(w.layout, layout));
}

/** The first free place of a size, row by row from the top, or null when the grid is full. */
export function firstFree(
  widgets: readonly Placed[],
  size: { w: number; h: number },
): WidgetLayout | null {
  for (let y = 0; y + size.h <= GRID_ROWS; y += 1) {
    for (let x = 0; x + size.w <= GRID_COLUMNS; x += 1) {
      const layout = { x, y, ...size };
      if (widgets.every((w) => !overlaps(w.layout, layout))) return layout;
    }
  }
  return null;
}

/** The rows a layout reaches down to. */
export function gridHeight(widgets: readonly Placed[]): number {
  return widgets.reduce((rows, w) => Math.max(rows, w.layout.y + w.layout.h), 0);
}

/** Reading order: by row, then by column. */
export function byReadingOrder<T extends { id: string; layout: WidgetLayout }>(a: T, b: T): number {
  return a.layout.y - b.layout.y || a.layout.x - b.layout.x || a.id.localeCompare(b.id);
}

export interface QueryShape {
  measures: readonly MeasureRequest[];
  dimensions: readonly string[];
  topN: number | null;
}

/** The series a chart of the query draws: each measure with each key, and `other` for an additive measure. */
export function chartSeries(shape: QueryShape, catalog: readonly SchemaMeasure[]): number {
  if (shape.dimensions.length === 0) return shape.measures.length;
  const keys = shape.topN ?? MAX_SERIES_KEYS;
  return shape.measures.reduce((sum, m) => {
    const additive = catalog.find((c) => c.id === m.measure)?.additive ?? true;
    return sum + keys + (additive ? 1 : 0);
  }, 0);
}

/** What the kind takes of a query: the measures and dimensions it allows, and the most keys of a chart. */
export function kindShape(kind: WidgetKind): {
  measures: [number, number];
  dimensions: [number, number];
} {
  if (kind === "stat") return { measures: [1, 1], dimensions: [0, 0] };
  if (kind === "bar") return { measures: [1, 1], dimensions: [1, 2] };
  return { measures: [1, 4], dimensions: [0, 2] };
}

/** Why the query does not suit the kind, in the page's words, or null when it does. */
export function shapeProblem(
  kind: WidgetKind,
  shape: QueryShape,
  catalog: readonly SchemaMeasure[],
): string | null {
  const { measures, dimensions } = kindShape(kind);
  const m = shape.measures.length;
  const d = shape.dimensions.length;
  if (m < measures[0] || m > measures[1]) {
    return measures[1] === 1 ? "Choose one measure." : "Choose one to four measures.";
  }
  if (d < dimensions[0]) return "Group by at least one dimension.";
  if (d > dimensions[1]) return kind === "stat" ? "A stat shows one number: remove the grouping." : "Group by at most two dimensions.";
  if (kind === "timeseries") {
    const series = chartSeries(shape, catalog);
    if (series > MAX_CHART_SERIES) {
      return `A chart draws at most ${MAX_CHART_SERIES} series; this one would draw ${series}. Show fewer keys.`;
    }
  }
  return null;
}

/** The most keys a chart of these measures can show with its `other` series, or null without a grouping. */
export function maxChartKeys(
  measures: readonly MeasureRequest[],
  catalog: readonly SchemaMeasure[],
): number {
  const perKey = measures.length;
  const other = measures.filter((m) => catalog.find((c) => c.id === m.measure)?.additive ?? true).length;
  return Math.max(0, Math.floor((MAX_CHART_SERIES - other) / Math.max(1, perKey)));
}
