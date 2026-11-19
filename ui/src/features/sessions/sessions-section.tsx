"use client";

import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { listSessions } from "@/api/client";
import type { SessionItem } from "@/api/generated/SessionItem";
import type { SessionPage } from "@/api/generated/SessionPage";
import { EmptyState } from "@/components/empty-state";
import { ListSkeleton } from "@/components/list-skeleton";
import { LoadMore } from "@/components/load-more";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, TraceStatusLabel } from "@/components/status";
import { formatCost, formatCount, formatTime } from "@/lib/format";
import { metaOf, rowsOf, usePagedList } from "@/lib/paged-list";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, useTimeRange } from "@/lib/time-range";

const COLUMNS = ["Last seen", "Session", "Traces", "Spans", "Cost", "Tokens", "Status"] as const;

export function sessionHref(project: string, session: string): Route {
  return `/session/?${new URLSearchParams({ project, session })}` as Route;
}

export function SessionsSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const { query, restart } = usePagedList<SessionItem, SessionPage>(
    [project, "sessions", range],
    range,
    (window, cursor, signal) => listSessions(project, window, cursor, signal),
  );
  return (
    <Section
      title="Sessions"
      query={query}
      skeleton={<ListSkeleton />}
      isEmpty={(data) => {
        const first = data.pages[0]?.page;
        return first !== undefined && first.data.length === 0 && isWhole(first.meta);
      }}
      empty={
        range === "30d" ? (
          <EmptyState kind="first-use" title="No sessions in the last 30 days.">
            Sessions appear here once your spans carry a session id. Traces with the same session id
            are grouped into one session.
          </EmptyState>
        ) : (
          <EmptyState
            kind="filtered"
            title={`No sessions in ${RANGE_PHRASE[range]}.`}
            action={<Button onClick={() => setRange("30d")}>Show last 30 days</Button>}
          >
            Older sessions may be outside this time range.
          </EmptyState>
        )
      }
    >
      {(data) => {
        const rows = rowsOf<SessionItem, SessionPage>(data);
        const meta = metaOf(data);
        if (rows.length === 0) {
          return (
            <EmptyState
              kind="incomplete"
              title="No sessions to show yet."
              action={<Button onClick={() => query.refetch()}>Check again</Button>}
            >
              Sessions may still be arriving, or part of the data couldn&apos;t be read.
            </EmptyState>
          );
        }
        return (
          <>
            {meta ? <MetaNotice meta={meta} noun="sessions" /> : null}
            <SessionsTable rows={rows} project={project} />
            <LoadMore
              noun="sessions"
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
  );
}

function SessionsTable({ rows, project }: { rows: SessionItem[]; project: string }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full min-w-[44rem] text-start text-data">
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
            <tr key={row.session_id} className="h-8 border-t border-ash-border">
              <td className="pe-4 whitespace-nowrap">{formatTime(row.last_seen)}</td>
              <td className="max-w-72 truncate pe-4">
                <Link href={sessionHref(project, row.session_id)} className="font-mono">
                  {row.session_id}
                </Link>
              </td>
              <td className="pe-4 tabular-nums">{formatCount(row.trace_count)}</td>
              <td className="pe-4 tabular-nums">{formatCount(row.span_count)}</td>
              <td className="pe-4 whitespace-nowrap tabular-nums">
                {formatCost(row.total_cost_usd)}
              </td>
              <td className="pe-4 tabular-nums">{formatCount(row.total_tokens)}</td>
              <td className="whitespace-nowrap">
                <TraceStatusLabel errors={row.error_count} status="ok" />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
