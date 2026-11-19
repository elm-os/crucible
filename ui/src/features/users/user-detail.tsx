"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { getUser, listUserSessions } from "@/api/client";
import type { UserSessionItem } from "@/api/generated/UserSessionItem";
import type { UserSessionPage } from "@/api/generated/UserSessionPage";
import { EmptyState } from "@/components/empty-state";
import { ListSkeleton } from "@/components/list-skeleton";
import { LoadMore } from "@/components/load-more";
import { PageHeader } from "@/components/page-header";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, TraceStatusLabel } from "@/components/status";
import { Facts, FactsSkeleton, MissingEntity } from "@/features/sessions/session-detail";
import { sessionHref } from "@/features/sessions/sessions-section";
import { EntityTraces } from "@/features/traces/entity-traces";
import { formatCost, formatCount, formatTime } from "@/lib/format";
import { metaOf, rowsOf, usePagedList } from "@/lib/paged-list";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, rangeWindow, useTimeRange } from "@/lib/time-range";

const SESSION_COLUMNS = ["Latest trace", "Session", "Traces", "First trace"] as const;

export function UserDetail() {
  const params = useSearchParams();
  const user = params.get("user");
  const project = params.get("project");
  if (!user) {
    const href = (project ? `/users?${new URLSearchParams({ project })}` : "/users") as Route;
    return <MissingEntity title="User" list="users" href={href} />;
  }
  return (
    <>
      <PageHeader title="User" description={<span className="font-mono break-all">{user}</span>} />
      <UserSummary user={user} />
      <UserSessions user={user} />
      <EntityTraces title="Traces of this user" filter={{ user_id: [user] }} owner="this user" />
    </>
  );
}

function UserSummary({ user }: { user: string }) {
  const project = useProject();
  const [range] = useTimeRange(project);
  const query = useQuery({
    queryKey: [project, "user", user, range],
    queryFn: ({ signal }) => getUser(project, user, rangeWindow(range, Date.now()), signal),
  });
  return (
    <Section
      title="Summary"
      query={query}
      skeleton={<FactsSkeleton />}
      isEmpty={(detail) => detail.user === null}
      empty={
        <EmptyState
          kind="incomplete"
          title="This user has no data to show yet."
          action={<Button onClick={() => query.refetch()}>Check again</Button>}
        >
          Their spans may still be arriving, or part of the data couldn&apos;t be read.
        </EmptyState>
      }
    >
      {(detail) =>
        detail.user ? (
          <>
            <MetaNotice meta={detail.meta} noun="spans" />
            <Facts
              facts={[
                ["Traces", formatCount(detail.user.trace_count)],
                ["Sessions", formatCount(detail.user.session_count)],
                ["Spans", formatCount(detail.user.span_count)],
                [
                  "Status",
                  <TraceStatusLabel key="status" errors={detail.user.error_count} status="ok" />,
                ],
                ["Cost", formatCost(detail.user.total_cost_usd)],
                ["Tokens", formatCount(detail.user.total_tokens)],
                ["First seen", formatTime(detail.user.first_seen)],
                ["Last seen", formatTime(detail.user.last_seen)],
              ]}
            />
          </>
        ) : null
      }
    </Section>
  );
}

function UserSessions({ user }: { user: string }) {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const { query, restart } = usePagedList<UserSessionItem, UserSessionPage>(
    [project, "user-sessions", user, range],
    range,
    (window, cursor, signal) => listUserSessions(project, user, window, cursor, signal),
  );
  return (
    <Section
      title="Sessions of this user"
      query={query}
      className="mt-4"
      skeleton={<ListSkeleton />}
      isEmpty={(data) => {
        const first = data.pages[0]?.page;
        return first !== undefined && first.data.length === 0 && isWhole(first.meta);
      }}
      empty={
        <EmptyState
          kind="filtered"
          title={`No sessions of this user in ${RANGE_PHRASE[range]}.`}
          action={
            range === "30d" ? undefined : (
              <Button onClick={() => setRange("30d")}>Show last 30 days</Button>
            )
          }
        >
          A session is listed when one of the user&apos;s traces names it.
        </EmptyState>
      }
    >
      {(data) => {
        const rows = rowsOf<UserSessionItem, UserSessionPage>(data);
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
            <div className="overflow-x-auto">
              <table className="w-full min-w-[32rem] text-start text-data">
                <thead>
                  <tr className="text-label text-ink-soft">
                    {SESSION_COLUMNS.map((column) => (
                      <th key={column} scope="col" className="h-8 pe-4 text-start font-medium">
                        {column}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((row) => (
                    <tr key={row.session_id} className="h-8 border-t border-ash-border">
                      <td className="pe-4 whitespace-nowrap">{formatTime(row.last_trace_at)}</td>
                      <td className="max-w-72 truncate pe-4">
                        <Link href={sessionHref(project, row.session_id)} className="font-mono">
                          {row.session_id}
                        </Link>
                      </td>
                      <td className="pe-4 tabular-nums">{formatCount(row.trace_count)}</td>
                      <td className="whitespace-nowrap">{formatTime(row.first_trace_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
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
