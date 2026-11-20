"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import type { ReactNode } from "react";
import { queryMetrics } from "@/api/client";
import type { UiError } from "@/api/errors";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import type { SchemaMeasure } from "@/api/generated/SchemaMeasure";
import type { Widget } from "@/api/generated/Widget";
import { BarListChart } from "@/components/bar-list-chart";
import { EmptyState } from "@/components/empty-state";
import { Section, SectionFrame } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { TimeSeriesChart } from "@/components/time-series-chart";
import { RANGE_PHRASE, type Range, rangeWindow } from "@/lib/time-range";
import { chartModel } from "@/primitives/chart/chart-model";
import { Markdown } from "./markdown";
import {
  chartSeries,
  formatHeadline,
  formatter,
  hasData,
  isAdditive,
  isFamily,
  isWholeMeta,
  keyRows,
  measureLabel,
  reduce,
} from "./widget-data";

export interface WidgetContext {
  project: string;
  /** The dashboard, or the editor's draft, the widget belongs to: part of the cache key. */
  owner: string;
  range: Range;
  catalog: readonly SchemaMeasure[];
  onWiden: () => void;
  /** Where a person who edits content changes the widget. */
  editHref?: Route;
}

export function widgetQueryKey(context: WidgetContext, widget: Widget) {
  return [
    context.project,
    "dashboard-widget",
    context.owner,
    widget.id,
    context.range,
    JSON.stringify(widget.query),
  ] as const;
}

// A widget's buckets are its own choice, so a range they do not fit is the widget's problem, not the
// project's limit.
function widgetErrorCopy(error: UiError): { title: string; detail: string } | null {
  if (error.code !== "query_too_broad") return null;
  return {
    title: "This widget's buckets don't fit the selected range.",
    detail: "Choose a shorter time range, or edit the widget to let it pick its buckets.",
  };
}

export function WidgetView({ context, widget }: { context: WidgetContext; widget: Widget }) {
  const { display } = widget;
  if (display.kind === "markdown" || !widget.query) {
    return (
      <SectionFrame title={display.title} className="h-full">
        <Markdown source={display.text ?? ""} />
      </SectionFrame>
    );
  }
  return <DataWidget context={context} widget={widget} />;
}

function DataWidget({ context, widget }: { context: WidgetContext; widget: Widget }) {
  const { display } = widget;
  const query = useQuery({
    queryKey: widgetQueryKey(context, widget),
    queryFn: ({ signal }) =>
      queryMetrics(
        context.project,
        { ...widget.query!, ...rangeWindow(context.range, Date.now()) },
        signal,
      ),
  });
  const title = display.title;
  return (
    <Section
      title={title}
      query={query}
      className="h-full"
      skeleton={<WidgetSkeleton kind={display.kind} />}
      errorCopy={widgetErrorCopy}
      errorActions={
        context.editHref ? (
          <Link href={context.editHref} className="text-data">
            Edit widget
          </Link>
        ) : undefined
      }
      isEmpty={(response) => !hasData(context.catalog, response) && isWholeMeta(response.meta)}
      empty={<NoData title={title} range={context.range} onWiden={context.onWiden} />}
    >
      {(response) =>
        hasData(context.catalog, response) ? (
          <>
            <MetaNotice meta={response.meta} noun="spans" />
            <WidgetBody context={context} widget={widget} response={response} />
          </>
        ) : (
          <EmptyState
            kind="incomplete"
            title="No data to show yet."
            action={
              <Button
                aria-label={`Check again: ${title.toLowerCase()}`}
                onClick={() => query.refetch()}
              >
                Check again
              </Button>
            }
          >
            Data may still be arriving, or part of it couldn&apos;t be read.
          </EmptyState>
        )
      }
    </Section>
  );
}

function NoData({ title, range, onWiden }: { title: string; range: Range; onWiden: () => void }) {
  if (range === "30d") {
    return <EmptyState kind="first-use" title="No data in the last 30 days." />;
  }
  return (
    <EmptyState
      kind="filtered"
      title={`No data in ${RANGE_PHRASE[range]}.`}
      action={
        <Button aria-label={`Show last 30 days: ${title.toLowerCase()}`} onClick={onWiden}>
          Show last 30 days
        </Button>
      }
    />
  );
}

function WidgetBody({
  context,
  widget,
  response,
}: {
  context: WidgetContext;
  widget: Widget;
  response: MetricsResponse;
}): ReactNode {
  const { catalog } = context;
  const measures = widget.query?.measures ?? [];
  const first = response.series[0];
  const unit = first?.unit ?? "count";
  const title = widget.display.title;
  switch (widget.display.kind) {
    case "stat": {
      if (!first) return null;
      const additive = isAdditive(catalog, first);
      const value = reduce(first, additive);
      return (
        <StatBody
          value={value === null ? "No data" : formatHeadline(unit, value)}
          caption={
            additive ? `in ${RANGE_PHRASE[context.range]}` : "in the newest bucket with data"
          }
          points={first.points.map((p) => ({ t: Date.parse(p.bucket), v: p.value }))}
        />
      );
    }
    case "timeseries":
      return (
        <TimeSeriesChart
          title={title}
          form={widget.display.chart ?? "line"}
          family={isFamily(response)}
          series={chartSeries(catalog, response, measures.length)}
          format={formatter(unit)}
        />
      );
    case "bar": {
      // Ranked bars: the largest first, whatever order the keys arrive in, and Other last.
      const rows = keyRows(catalog, response, measures).toSorted(
        (a, b) => Number(a.other) - Number(b.other) || (b.values[0] ?? 0) - (a.values[0] ?? 0),
      );
      return (
        <>
          {first && !isAdditive(catalog, first) ? (
            <p className="mb-2 text-label text-ink-soft">The newest bucket with data, per key.</p>
          ) : null}
          <BarListChart
            title={title}
            items={rows.map((row) => ({
              key: row.key,
              label: row.label,
              value: row.values[0] ?? null,
              other: row.other,
            }))}
            format={formatter(unit)}
          />
        </>
      );
    }
    case "table":
      return <WidgetTable context={context} widget={widget} response={response} />;
    default:
      return null;
  }
}

function StatBody({
  value,
  caption,
  points,
}: {
  value: string;
  caption: string;
  points: { t: number; v: number | null }[];
}) {
  const spark = { width: 160, height: 32, top: 2, right: 2, bottom: 2, left: 2 };
  const path =
    points.length > 1
      ? chartModel([{ id: "s", label: "", points }], spark, "line").paths[0]
      : undefined;
  return (
    <div className="flex flex-col gap-1">
      <p data-value className="text-heading-sm font-medium tabular-nums">
        {value}
      </p>
      <p className="text-label text-ink-soft">{caption}</p>
      {path ? (
        <svg
          viewBox={`0 0 ${spark.width} ${spark.height}`}
          className="mt-1 h-8 w-full"
          aria-hidden="true"
          preserveAspectRatio="none"
        >
          <path d={path} fill="none" stroke="var(--color-series-1)" strokeWidth={1.5} />
        </svg>
      ) : null}
    </div>
  );
}

function WidgetTable({
  context,
  widget,
  response,
}: {
  context: WidgetContext;
  widget: Widget;
  response: MetricsResponse;
}) {
  const measures = widget.query?.measures ?? [];
  const dimensions = widget.query?.dimensions ?? [];
  const rows = keyRows(context.catalog, response, measures);
  const unitOf = (measure: string, aggregation: string) =>
    response.series.find((s) => s.measure === measure && s.aggregation === aggregation)?.unit ??
    "count";
  const additive = (measure: string) =>
    context.catalog.find((m) => m.id === measure)?.additive ?? true;
  const keyName =
    dimensions.length === 0 ? "Keys" : dimensions.map((d) => d.replaceAll("_", " ")).join(" · ");
  return (
    <div className="relative max-h-80 overflow-auto">
      <table className="w-full text-data">
        <caption className="sr-only">{widget.display.title}</caption>
        <thead>
          <tr className="text-left text-label text-ink-soft">
            <th scope="col" className="h-8 pe-4 font-medium capitalize">
              {keyName}
            </th>
            {measures.map((m) => (
              <th
                key={`${m.measure}-${m.aggregation}`}
                scope="col"
                className="h-8 pe-4 text-end font-medium"
              >
                {measureLabel(context.catalog, m.measure)}
                {m.aggregation === "sum" ? "" : ` ${m.aggregation}`}
                {additive(m.measure) ? null : (
                  <span className="sr-only">, newest bucket with data</span>
                )}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.key} className="h-8 border-t border-ash-border">
              <th scope="row" className="max-w-48 truncate pe-4 text-left font-normal">
                {row.label}
              </th>
              {row.values.map((value, i) => {
                const m = measures[i];
                const text =
                  value === null || !m
                    ? "no data"
                    : formatter(unitOf(m.measure, m.aggregation))(value);
                return (
                  <td
                    key={`${row.key}-${m?.measure}-${m?.aggregation}`}
                    className="pe-4 text-end tabular-nums"
                  >
                    {text}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function WidgetSkeleton({ kind }: { kind: Widget["display"]["kind"] }) {
  if (kind === "stat") {
    return (
      <div className="flex flex-col gap-2">
        <div className="skeleton h-7 w-2/3" />
        <div className="skeleton h-3 w-1/2" />
      </div>
    );
  }
  if (kind === "table") {
    return (
      <div className="flex flex-col gap-3">
        <div className="skeleton h-3 w-1/3" />
        <div className="skeleton h-3 w-2/3" />
        <div className="skeleton h-3 w-1/2" />
      </div>
    );
  }
  return <div className="skeleton h-40 w-full" />;
}
