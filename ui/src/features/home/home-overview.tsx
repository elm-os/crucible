"use client";

import type { UseQueryResult } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { ReactNode } from "react";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import { EmptyState } from "@/components/empty-state";
import { Section, SectionFrame } from "@/components/section";
import { isWhole, MetaNotice } from "@/components/status";
import { TimeSeriesChart } from "@/components/time-series-chart";
import { formatCost, formatCount, formatDuration } from "@/lib/format";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, type Range, useTimeRange } from "@/lib/time-range";
import { type ChartSeries, chartModel } from "@/primitives/chart/chart-model";
import {
  chartSeries,
  errorRate,
  type HomeQuery,
  latest,
  noData,
  total,
  useHomeMetrics,
} from "./home-metrics";
import { placement, useSetupSteps } from "./home-setup";
import { SetupCard } from "./setup-card";

const percent = new Intl.NumberFormat("en-US", { style: "percent", maximumFractionDigits: 1 });

export function HomeOverview() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const traces = useHomeMetrics(project, range, "traces");
  const cost = useHomeMetrics(project, range, "cost");
  const tokens = useHomeMetrics(project, range, "tokens");
  const errors = useHomeMetrics(project, range, "errors");
  const latency = useHomeMetrics(project, range, "latency");
  const setup = useSetupSteps(project);
  const setupAt = placement(setup);
  const widen = () => setRange("30d");

  return (
    <div className="flex flex-col gap-6">
      {setupAt === "first" ? <SetupCard steps={setup} /> : null}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-5">
        <Tile title="Traces" which="traces" query={traces} range={range} onWiden={widen}>
          {(response) => (
            <Headline
              value={formatCount(total(response.series[0]))}
              caption={`in ${RANGE_PHRASE[range]}`}
              series={response.series[0] && chartSeries(response.series[0], "Traces")}
            />
          )}
        </Tile>
        <Tile title="Error rate" which="errors" query={errors} range={range} onWiden={widen}>
          {(response) => (
            <Headline
              value={percent.format(errorRate(response) ?? 0)}
              caption={`${formatCount(total(response.series[1]))} of ${formatCount(total(response.series[0]))} spans`}
              series={response.series[1] && chartSeries(response.series[1], "Errors")}
            />
          )}
        </Tile>
        <Tile title="Latency" which="latency" query={latency} range={range} onWiden={widen}>
          {(response) => (
            <Headline
              value={`${formatDuration(latest(response.series[0]) ?? 0)} p50`}
              caption={`${formatDuration(latest(response.series[1]) ?? 0)} p95, in the newest bucket with spans`}
              series={response.series[1] && chartSeries(response.series[1], "p95")}
            />
          )}
        </Tile>
        <Tile title="Cost" which="cost" query={cost} range={range} onWiden={widen}>
          {(response) => (
            <Headline
              value={formatCost(total(response.series[0]))}
              caption={`in ${RANGE_PHRASE[range]}`}
              series={response.series[0] && chartSeries(response.series[0], "Cost")}
            />
          )}
        </Tile>
        <Tile title="Tokens" which="tokens" query={tokens} range={range} onWiden={widen}>
          {(response) => (
            <Headline
              value={formatCount(total(response.series[0]))}
              caption={`in ${RANGE_PHRASE[range]}`}
              series={response.series[0] && chartSeries(response.series[0], "Tokens")}
            />
          )}
        </Tile>
      </div>
      <div className="grid gap-4 lg:grid-cols-2">
        <ChartSection
          title="Traces over time"
          which="traces"
          query={traces}
          range={range}
          onWiden={widen}
        >
          {(response) => (
            <TimeSeriesChart
              title="Traces per bucket"
              form="bar"
              series={response.series.map((s) => chartSeries(s, "Traces"))}
              format={formatCount}
            />
          )}
        </ChartSection>
        <ChartSection
          title="Spans and errors"
          which="errors"
          query={errors}
          range={range}
          onWiden={widen}
        >
          {(response) => (
            <TimeSeriesChart
              title="Spans and errors per bucket"
              series={response.series.map((s, i) => chartSeries(s, i === 0 ? "Spans" : "Errors"))}
              format={formatCount}
            />
          )}
        </ChartSection>
        <ChartSection
          title="Latency percentiles"
          which="latency"
          query={latency}
          range={range}
          onWiden={widen}
        >
          {(response) => (
            <TimeSeriesChart
              title="Span latency percentiles per bucket"
              family
              series={response.series.map((s) => chartSeries(s, s.aggregation))}
              format={(us) => formatDuration(us)}
            />
          )}
        </ChartSection>
      </div>
      {setupAt === "last" ? <SetupCard steps={setup} /> : null}
    </div>
  );
}

interface MetricsSectionProps {
  title: string;
  which: HomeQuery;
  query: UseQueryResult<MetricsResponse>;
  range: Range;
  onWiden: () => void;
  children: (response: MetricsResponse) => ReactNode;
}

// Every tile and chart is its own section: one failing query never blanks the others.
function Tile(props: MetricsSectionProps) {
  return <MetricsSection {...props} skeleton={<TileSkeleton />} compact />;
}

function ChartSection(props: MetricsSectionProps) {
  return <MetricsSection {...props} skeleton={<ChartSkeleton />} compact={false} />;
}

function MetricsSection({
  title,
  which,
  query,
  range,
  onWiden,
  children,
  skeleton,
  compact,
}: MetricsSectionProps & { skeleton: ReactNode; compact: boolean }) {
  return (
    <Section
      title={title}
      query={query}
      skeleton={skeleton}
      fill={compact}
      // No data counts as empty only when the answer is whole; otherwise spans may exist unseen.
      isEmpty={(response) => noData(response, which) && isWhole(response.meta)}
      empty={<NoData title={title} range={range} onWiden={onWiden} compact={compact} />}
    >
      {(response) =>
        noData(response, which) ? (
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
            {compact ? null : "Spans may still be arriving, or part of the data couldn't be read."}
          </EmptyState>
        ) : (
          <>
            <MetaNotice meta={response.meta} noun="spans" />
            {children(response)}
          </>
        )
      }
    </Section>
  );
}

function NoData({
  title,
  range,
  onWiden,
  compact,
}: {
  title: string;
  range: Range;
  onWiden: () => void;
  compact: boolean;
}) {
  if (range === "30d") {
    return (
      <EmptyState kind="first-use" title="No data in the last 30 days.">
        {compact
          ? null
          : "Numbers appear here once an app sends traces to this Crucible server over OTLP."}
      </EmptyState>
    );
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

const SPARK = { width: 160, height: 32, top: 2, right: 2, bottom: 2, left: 2 };

function Headline({
  value,
  caption,
  series,
}: {
  value: string;
  caption: string;
  series: ChartSeries | undefined;
}) {
  const path = series ? chartModel([series], SPARK, "line").paths[0] : undefined;
  return (
    <div className="flex grow flex-col gap-1">
      <p data-value className="text-heading-sm font-medium tabular-nums">
        {value}
      </p>
      <p className="mb-1 text-label text-ink-soft">{caption}</p>
      {/* At the tile's bottom, so a caption on two lines leaves every sparkline in the row level. */}
      {path ? (
        <svg
          viewBox={`0 0 ${SPARK.width} ${SPARK.height}`}
          className="mt-auto h-8 w-full"
          aria-hidden="true"
          preserveAspectRatio="none"
        >
          <path d={path} fill="none" stroke="var(--color-series-1)" strokeWidth={1.5} />
        </svg>
      ) : null}
    </div>
  );
}

/** What the page shows before it can read the URL: every frame, holding its place. */
export function HomeSkeleton() {
  return (
    <div className="flex flex-col gap-6">
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-5">
        {["Traces", "Error rate", "Latency", "Cost", "Tokens"].map((title) => (
          <SectionFrame key={title} title={title} busy>
            <div aria-hidden="true" className="invisible">
              <TileSkeleton />
            </div>
          </SectionFrame>
        ))}
      </div>
    </div>
  );
}

function TileSkeleton() {
  return (
    <div className="flex flex-col gap-2">
      <div className="skeleton h-7 w-2/3" />
      <div className="skeleton h-3 w-1/2" />
      <div className="skeleton h-8 w-full" />
    </div>
  );
}

function ChartSkeleton() {
  return <div className="skeleton h-52 w-full" />;
}
