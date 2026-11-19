"use client";

import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { listSpans } from "@/api/client";
import type { SpanListItem } from "@/api/generated/SpanListItem";
import type { SpanListPage } from "@/api/generated/SpanListPage";
import { EmptyState } from "@/components/empty-state";
import { FilterBar } from "@/components/filter-bar";
import { ListSkeleton } from "@/components/list-skeleton";
import { LoadMore } from "@/components/load-more";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, StatusLabel } from "@/components/status";
import { FILTER_FIELDS, filterKey, hasFilters, useListFilters } from "@/lib/filters";
import { formatCost, formatDuration, formatTime } from "@/lib/format";
import { metaOf, rowsOf, usePagedList } from "@/lib/paged-list";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, useTimeRange } from "@/lib/time-range";

const COLUMNS = ["Started", "Name", "Type", "Duration", "Model", "Cost", "Status"] as const;

function spanHref(project: string, row: SpanListItem): Route {
  const params = new URLSearchParams({ project, trace: row.trace_id, span: row.span_id });
  return `/trace/?${params}` as Route;
}

export function SpansSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [filters, setFilters] = useListFilters();
  const filtered = hasFilters(filters);
  const { query, restart } = usePagedList<SpanListItem, SpanListPage>(
    [project, "spans", filterKey(filters), range],
    range,
    (window, cursor, signal) => listSpans(project, window, filters, cursor, signal),
  );
  return (
    <>
      <FilterBar filters={filters} onChange={setFilters} fields={FILTER_FIELDS} noun="spans" />
      <Section
        title="Spans"
        query={query}
        skeleton={<ListSkeleton />}
        isEmpty={(data) => {
          const first = data.pages[0]?.page;
          return first !== undefined && first.data.length === 0 && isWhole(first.meta);
        }}
        empty={
          filtered ? (
            <EmptyState
              kind="filtered"
              title={`No spans match these filters in ${RANGE_PHRASE[range]}.`}
              action={<Button onClick={() => setFilters({})}>Clear filters</Button>}
            >
              Remove a filter or choose a longer time range.
            </EmptyState>
          ) : range === "30d" ? (
            <EmptyState kind="first-use" title="No spans in the last 30 days.">
              Spans will appear here once an app sends traces to this Crucible server over OTLP.
            </EmptyState>
          ) : (
            <EmptyState
              kind="filtered"
              title={`No spans in ${RANGE_PHRASE[range]}.`}
              action={<Button onClick={() => setRange("30d")}>Show last 30 days</Button>}
            >
              Older spans may be outside this time range.
            </EmptyState>
          )
        }
      >
        {(data) => {
          const rows = rowsOf<SpanListItem, SpanListPage>(data);
          const meta = metaOf(data);
          if (rows.length === 0) {
            return (
              <EmptyState
                kind="incomplete"
                title="No spans to show yet."
                action={<Button onClick={() => query.refetch()}>Check again</Button>}
              >
                Spans may still be arriving, or part of the data couldn&apos;t be read.
              </EmptyState>
            );
          }
          return (
            <>
              {meta ? <MetaNotice meta={meta} noun="spans" /> : null}
              <div className="overflow-x-auto">
                <table className="w-full min-w-[48rem] text-start text-data">
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
                      <tr
                        key={`${row.trace_id}/${row.span_id}`}
                        className="h-8 border-t border-ash-border"
                      >
                        <td className="pe-4 whitespace-nowrap">{formatTime(row.start_time)}</td>
                        <td className="max-w-72 truncate pe-4">
                          <Link href={spanHref(project, row)}>{row.name || "Unnamed span"}</Link>
                        </td>
                        <td className="pe-4">{row.observation_type}</td>
                        <td className="pe-4 whitespace-nowrap tabular-nums">
                          {formatDuration(row.duration_us)}
                        </td>
                        <td className="max-w-40 truncate pe-4">{row.model || "None"}</td>
                        <td className="pe-4 whitespace-nowrap tabular-nums">
                          {formatCost(row.total_cost_usd)}
                        </td>
                        <td className="whitespace-nowrap">
                          {row.status_code === "error" || row.level === "error" ? (
                            <StatusLabel status="error" label="Error" />
                          ) : (
                            <StatusLabel status="ok" label="OK" />
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <LoadMore
                noun="spans"
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
