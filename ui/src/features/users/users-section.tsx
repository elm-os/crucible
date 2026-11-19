"use client";

import { Button } from "@ui-sdk/react/button";
import Link from "next/link";
import { listUsers } from "@/api/client";
import type { UserItem } from "@/api/generated/UserItem";
import type { UserPage } from "@/api/generated/UserPage";
import { EmptyState } from "@/components/empty-state";
import { ListSkeleton } from "@/components/list-skeleton";
import { LoadMore } from "@/components/load-more";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, TraceStatusLabel } from "@/components/status";
import { userHref } from "@/features/sessions/session-detail";
import { formatCost, formatCount, formatTime } from "@/lib/format";
import { metaOf, rowsOf, usePagedList } from "@/lib/paged-list";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, useTimeRange } from "@/lib/time-range";

const COLUMNS = [
  "Last seen",
  "User",
  "Traces",
  "Sessions",
  "Spans",
  "Cost",
  "Tokens",
  "Status",
] as const;

export function UsersSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const { query, restart } = usePagedList<UserItem, UserPage>(
    [project, "users", range],
    range,
    (window, cursor, signal) => listUsers(project, window, cursor, signal),
  );
  return (
    <Section
      title="Users"
      query={query}
      skeleton={<ListSkeleton />}
      isEmpty={(data) => {
        const first = data.pages[0]?.page;
        return first !== undefined && first.data.length === 0 && isWhole(first.meta);
      }}
      empty={
        range === "30d" ? (
          <EmptyState kind="first-use" title="No users in the last 30 days.">
            Users appear here once your spans carry a user id: the person or account your agent
            served.
          </EmptyState>
        ) : (
          <EmptyState
            kind="filtered"
            title={`No users in ${RANGE_PHRASE[range]}.`}
            action={<Button onClick={() => setRange("30d")}>Show last 30 days</Button>}
          >
            Older users may be outside this time range.
          </EmptyState>
        )
      }
    >
      {(data) => {
        const rows = rowsOf<UserItem, UserPage>(data);
        const meta = metaOf(data);
        if (rows.length === 0) {
          return (
            <EmptyState
              kind="incomplete"
              title="No users to show yet."
              action={<Button onClick={() => query.refetch()}>Check again</Button>}
            >
              Users may still be arriving, or part of the data couldn&apos;t be read.
            </EmptyState>
          );
        }
        return (
          <>
            {meta ? <MetaNotice meta={meta} noun="users" /> : null}
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
                    <tr key={row.user_id} className="h-8 border-t border-ash-border">
                      <td className="pe-4 whitespace-nowrap">{formatTime(row.last_seen)}</td>
                      <td className="max-w-64 truncate pe-4">
                        <Link href={userHref(project, row.user_id)} className="font-mono">
                          {row.user_id}
                        </Link>
                      </td>
                      <td className="pe-4 tabular-nums">{formatCount(row.trace_count)}</td>
                      <td className="pe-4 tabular-nums">{formatCount(row.session_count)}</td>
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
            <LoadMore
              noun="users"
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
