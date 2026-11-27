"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { useEffect, useRef, useState } from "react";
import { listNewTraces, listTraces } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { TraceListItem } from "@/api/generated/TraceListItem";
import type { TraceListPage } from "@/api/generated/TraceListPage";
import type { ListFilters, TimeWindow } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { FilterBar, type FilterConflict } from "@/components/filter-bar";
import { AlertIcon } from "@/components/icons";
import { LoadMore } from "@/components/load-more";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, TraceStatusLabel } from "@/components/status";
import { FILTER_FIELDS, filterKey, hasFilters, useListFilters } from "@/lib/filters";
import { formatCost, formatDuration, formatTime } from "@/lib/format";
import { metaOf, rowsOf, usePagedList } from "@/lib/paged-list";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, type Range, useTimeRange } from "@/lib/time-range";

const COLUMNS = ["Started", "Name", "Duration", "Spans", "Cost", "Status"] as const;
const SKELETON_ROWS = Array.from({ length: 12 }, (_, row) => `skeleton-row-${row}`);
const POLL_MS = 5_000;

// The trace list evaluates span conditions on one table, so these two cannot be combined there.
const TRACE_CONFLICTS: readonly FilterConflict[] = [
  [
    "metadata",
    "observation_type",
    "Type and Metadata can't be combined on the trace list. The spans list filters by both.",
  ],
];

export function TracesSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [filters, setFilters] = useListFilters();
  const filtered = hasFilters(filters);
  const { query, restart } = usePagedList<TraceListItem, TraceListPage>(
    [project, "traces", filterKey(filters), range],
    range,
    (window, cursor, signal) => listTraces(project, window, filters, signal, cursor ?? undefined),
  );
  // Live tail counts from the first page: the newest traces and the window they were read in.
  const first = query.data?.pages[0];
  return (
    <>
      <FilterBar
        filters={filters}
        onChange={setFilters}
        fields={FILTER_FIELDS}
        conflicts={TRACE_CONFLICTS}
        noun="traces"
      />
      {/* Outside the section, so it also counts traces arriving into an empty list. */}
      {first?.page.head_cursor ? (
        <NewTraces
          key={first.page.head_cursor}
          project={project}
          filters={filters}
          window={first.window}
          head={first.page.head_cursor}
          onShow={() => query.refetch()}
        />
      ) : null}
      <Section
        title="Recent traces"
        query={query}
        skeleton={<TracesSkeleton />}
        // An empty page means "no traces" only when it is whole; otherwise traces may exist unseen.
        isEmpty={(data) => {
          const page = data.pages[0]?.page;
          return page !== undefined && page.data.length === 0 && isWhole(page.meta);
        }}
        empty={
          filtered ? (
            <EmptyState
              kind="filtered"
              title={`No traces match these filters in ${RANGE_PHRASE[range]}.`}
              action={<Button onClick={() => setFilters({})}>Clear filters</Button>}
            >
              Remove a filter or choose a longer time range.
            </EmptyState>
          ) : (
            <TracesEmpty range={range} onWiden={() => setRange("30d")} />
          )
        }
      >
        {(data) => {
          const rows = rowsOf<TraceListItem, TraceListPage>(data);
          const meta = metaOf(data);
          if (rows.length === 0) {
            return (
              <EmptyState
                kind="incomplete"
                title="No traces to show yet."
                action={<Button onClick={() => query.refetch()}>Check again</Button>}
              >
                Traces may still be arriving, or part of the data couldn&apos;t be read.
              </EmptyState>
            );
          }
          return (
            <>
              {meta ? <MetaNotice meta={meta} noun="traces" /> : null}
              <TracesTable rows={rows} project={project} />
              <LoadMore
                noun="traces"
                hasMore={query.hasNextPage}
                loading={query.isFetchingNextPage}
                error={query.isFetchNextPageError ? query.error : null}
                onLoad={() => query.fetchNextPage()}
                onRestart={restart}
              />
            </>
          );
        }}
      </Section>
    </>
  );
}

// True once the wall clock passes `to`.
function useEnded(to: string): boolean {
  const end = Date.parse(to);
  const [ended, setEnded] = useState(() => Date.now() >= end);
  useEffect(() => {
    const wait = end - Date.now();
    if (wait <= 0) return;
    const timer = setTimeout(() => setEnded(true), wait);
    return () => clearTimeout(timer);
  }, [end]);
  return ended;
}

// Live tail: asks every 5 s how many traces arrived after the page was read, and offers to show them.
// It can only count traces that start inside the page's window, so when the window ends it refreshes
// the list (which reads a new window) if nothing new is waiting, and otherwise stops at the count.
function NewTraces({
  project,
  filters,
  window,
  head,
  onShow,
}: {
  project: string;
  filters: ListFilters;
  window: TimeWindow;
  head: string;
  onShow: () => void;
}) {
  const ended = useEnded(window.to);
  const tail = useQuery({
    queryKey: [project, "traces-new", filterKey(filters), head],
    queryFn: ({ signal }) => listNewTraces(project, window, filters, head, signal),
    enabled: !ended,
    refetchInterval: POLL_MS,
    retry: false,
  });
  const count = tail.data?.new_count ?? 0;
  const restart = tail.isError && asUiError(tail.error).kind === "cursor_reset";
  const refresh = restart || (ended && count === 0);
  const refreshed = useRef(false);
  useEffect(() => {
    if (refresh && !refreshed.current) {
      refreshed.current = true;
      onShow();
    }
  }, [refresh, onShow]);
  if (count === 0) {
    // A failed check says so where the count would show; the next one runs on its own. The list keeps
    // its own error state, so this line stays quiet and carries no code.
    return tail.isError && !restart ? (
      <p
        data-live-tail="failed"
        className="mb-3 flex items-center gap-1.5 text-label text-ink-soft"
      >
        <AlertIcon className="shrink-0" />
        Couldn&apos;t check for new traces. Checking again every {POLL_MS / 1000} seconds.
      </p>
    ) : null;
  }
  const more = count >= 100 && tail.data?.has_more;
  const label = `Show ${more ? "100+" : count} new ${count === 1 ? "trace" : "traces"}`;
  return (
    <div data-live-tail className="mb-3 flex items-center gap-3">
      <output className="text-label text-ink-soft">New traces arrived.</output>
      <Button onClick={onShow}>{label}</Button>
    </div>
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

export function TracesTable({ rows, project }: { rows: TraceListItem[]; project: string }) {
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
                <TraceStatusLabel errors={row.error_count} status={row.status_code} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function TracesSkeleton() {
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
