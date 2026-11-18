"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { listTraces } from "@/api/client";
import type { TraceListItem } from "@/api/generated/TraceListItem";
import { EmptyState } from "@/components/empty-state";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, StatusLabel } from "@/components/status";
import { formatCost, formatDuration, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, type Range, rangeWindow, useTimeRange } from "@/lib/time-range";

const COLUMNS = ["Started", "Name", "Duration", "Spans", "Cost", "Status"] as const;
const SKELETON_ROWS = Array.from({ length: 12 }, (_, row) => `skeleton-row-${row}`);

export function TracesSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const query = useQuery({
    queryKey: [project, "traces", {}, range],
    queryFn: ({ signal }) => listTraces(project, rangeWindow(range, Date.now()), signal),
  });
  return (
    <Section
      title="Recent traces"
      query={query}
      skeleton={<TracesSkeleton />}
      // An empty page means "no traces" only when it is whole; otherwise traces may exist unseen.
      isEmpty={(page) => page.data.length === 0 && isWhole(page.meta)}
      empty={<TracesEmpty range={range} onWiden={() => setRange("30d")} />}
    >
      {(page) =>
        page.data.length === 0 ? (
          <EmptyState
            kind="incomplete"
            title="No traces to show yet."
            action={<Button onClick={() => query.refetch()}>Check again</Button>}
          >
            Traces may still be arriving, or part of the data couldn&apos;t be read.
          </EmptyState>
        ) : (
          <>
            <MetaNotice meta={page.meta} noun="traces" />
            <TracesTable rows={page.data} project={project} />
            {page.has_more ? (
              <p className="mt-3 text-label text-ink-soft">
                Showing the newest {page.data.length} traces. Paging will arrive with the data grid.
              </p>
            ) : null}
          </>
        )
      }
    </Section>
  );
}

function TracesEmpty({ range, onWiden }: { range: Range; onWiden: () => void }) {
  if (range === "30d") {
    return (
      <EmptyState kind="first-use" title="No traces in the last 30 days.">
        Traces will appear here once an app sends them to this Crucible server over OTLP.
      </EmptyState>
    );
  }
  return (
    <EmptyState
      kind="filtered"
      title={`No traces in ${RANGE_PHRASE[range]}.`}
      action={<Button onClick={onWiden}>Show last 30 days</Button>}
    >
      Older traces may be outside this time range.
    </EmptyState>
  );
}

function traceHref(project: string, row: TraceListItem): Route {
  const params = new URLSearchParams({ project, trace: row.trace_id });
  return `/trace/?${params}` as Route;
}

function TraceStatus({ row }: { row: TraceListItem }) {
  const errors = row.error_count;
  if (errors > 0) {
    return <StatusLabel status="error" label={errors === 1 ? "1 error" : `${errors} errors`} />;
  }
  if (row.status_code === "error") return <StatusLabel status="error" label="Error" />;
  return <StatusLabel status="ok" label="OK" />;
}

function TracesTable({ rows, project }: { rows: TraceListItem[]; project: string }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full min-w-[42rem] text-start text-data">
        <thead>
          <tr className="text-label text-ink-soft">
            {COLUMNS.map((column) => (
              <th key={column} scope="col" className="h-8 pe-4 text-start font-medium">
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.trace_id} className="h-8 border-t border-ash-border">
              <td className="pe-4 whitespace-nowrap">{formatTime(row.start_time)}</td>
              <td className="max-w-80 truncate pe-4">
                <Link href={traceHref(project, row)}>
                  {row.trace_name || row.name || "Unnamed trace"}
                </Link>
              </td>
              <td className="pe-4 whitespace-nowrap tabular-nums">
                {formatDuration(row.duration_us)}
              </td>
              <td className="pe-4 tabular-nums">{row.span_count}</td>
              <td className="pe-4 whitespace-nowrap tabular-nums">
                {formatCost(row.total_cost_usd)}
              </td>
              <td className="whitespace-nowrap">
                <TraceStatus row={row} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function TracesSkeleton() {
  return (
    <div>
      <div className="h-8" />
      {SKELETON_ROWS.map((id) => (
        <div key={id} className="flex h-8 items-center gap-4 border-t border-ash-border">
          <div className="skeleton h-3 w-1/6" />
          <div className="skeleton h-3 w-2/5" />
          <div className="skeleton h-3 w-1/12" />
          <div className="skeleton h-3 w-1/12" />
        </div>
      ))}
    </div>
  );
}
