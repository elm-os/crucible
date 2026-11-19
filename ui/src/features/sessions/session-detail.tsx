"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import type { ReactNode } from "react";
import { getSessionDetail } from "@/api/client";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { Section, SectionFrame } from "@/components/section";
import { MetaNotice, TraceStatusLabel } from "@/components/status";
import { EntityTraces } from "@/features/traces/entity-traces";
import { formatCost, formatCount, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";

export function userHref(project: string, user: string): Route {
  return `/user/?${new URLSearchParams({ project, user })}` as Route;
}

export function Facts({ facts }: { facts: [string, ReactNode][] }) {
  return (
    <dl className="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
      {facts.map(([label, value]) => (
        <div key={label} className="flex min-w-0 flex-col gap-0.5">
          <dt className="text-label text-ink-soft">{label}</dt>
          <dd className="truncate text-data tabular-nums">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

export function FactsSkeleton() {
  return (
    <div className="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
      {["a", "b", "c", "d", "e", "f", "g", "h"].map((id) => (
        <div key={id} className="flex flex-col gap-1.5">
          <div className="skeleton h-3 w-1/2" />
          <div className="skeleton h-3 w-3/4" />
        </div>
      ))}
    </div>
  );
}

// A page for one entity named in the URL; without one it points back to its list.
export function MissingEntity({ title, list, href }: { title: string; list: string; href: Route }) {
  return (
    <>
      <PageHeader title={title} />
      <SectionFrame title={title}>
        <EmptyState
          kind="filtered"
          title={`No ${title.toLowerCase()} selected.`}
          action={<Link href={href}>Open the {list} list</Link>}
        >
          Open one from the {list} list to see it here.
        </EmptyState>
      </SectionFrame>
    </>
  );
}

export function SessionDetail() {
  const params = useSearchParams();
  const session = params.get("session");
  const project = params.get("project");
  if (!session) {
    const href = (project ? `/sessions?${new URLSearchParams({ project })}` : "/sessions") as Route;
    return <MissingEntity title="Session" list="sessions" href={href} />;
  }
  return (
    <>
      <PageHeader
        title="Session"
        description={<span className="font-mono break-all">{session}</span>}
      />
      <SessionSummary session={session} />
      <EntityTraces
        title="Traces in this session"
        filter={{ session_id: [session] }}
        owner="this session"
      />
    </>
  );
}

function SessionSummary({ session }: { session: string }) {
  const project = useProject();
  const [range] = useTimeRange(project);
  const query = useQuery({
    queryKey: [project, "session", session, range],
    queryFn: ({ signal }) =>
      getSessionDetail(project, session, rangeWindow(range, Date.now()), signal),
  });
  return (
    <Section
      title="Summary"
      query={query}
      skeleton={<FactsSkeleton />}
      isEmpty={(detail) => detail.session === null}
      empty={
        <EmptyState
          kind="incomplete"
          title="This session has no data to show yet."
          action={<Button onClick={() => query.refetch()}>Check again</Button>}
        >
          Its spans may still be arriving, or part of the data couldn&apos;t be read.
        </EmptyState>
      }
    >
      {(detail) =>
        detail.session ? (
          <>
            <MetaNotice meta={detail.meta} noun="spans" />
            <Facts
              facts={[
                ["Traces", formatCount(detail.session.trace_count)],
                ["Spans", formatCount(detail.session.span_count)],
                [
                  "Status",
                  <TraceStatusLabel key="status" errors={detail.session.error_count} status="ok" />,
                ],
                ["Cost", formatCost(detail.session.total_cost_usd)],
                ["Tokens", formatCount(detail.session.total_tokens)],
                ["First seen", formatTime(detail.session.first_seen)],
                ["Last seen", formatTime(detail.session.last_seen)],
                [
                  "Users",
                  detail.session.user_ids.length === 0 ? (
                    "None named"
                  ) : (
                    <span className="flex flex-wrap gap-x-2">
                      {detail.session.user_ids.map((user) => (
                        <Link key={user} href={userHref(project, user)} className="font-mono">
                          {user}
                        </Link>
                      ))}
                    </span>
                  ),
                ],
              ]}
            />
          </>
        ) : null
      }
    </Section>
  );
}
