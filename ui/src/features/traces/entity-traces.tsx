"use client";

import { Button } from "@ui-sdk/react/button";
import { listTraces } from "@/api/client";
import type { TraceListItem } from "@/api/generated/TraceListItem";
import type { TraceListPage } from "@/api/generated/TraceListPage";
import type { ListFilters } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { ListSkeleton } from "@/components/list-skeleton";
import { LoadMore } from "@/components/load-more";
import { Section } from "@/components/section";
import { isWhole, MetaNotice } from "@/components/status";
import { filterKey } from "@/lib/filters";
import { metaOf, rowsOf, usePagedList } from "@/lib/paged-list";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, useTimeRange } from "@/lib/time-range";
import { TracesTable } from "./traces-section";

// The traces of one session or user: the trace list under that filter, which tests each trace's head span.
export function EntityTraces({
  title,
  filter,
  owner,
}: {
  title: string;
  filter: ListFilters;
  owner: string;
}) {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const { query, restart } = usePagedList<TraceListItem, TraceListPage>(
    [project, "traces", filterKey(filter), range, "paged"],
    range,
    (window, cursor, signal) => listTraces(project, window, filter, signal, cursor ?? undefined),
  );
  return (
    <Section
      title={title}
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
          title={`No traces of ${owner} in ${RANGE_PHRASE[range]}.`}
          action={
            range === "30d" ? undefined : (
              <Button onClick={() => setRange("30d")}>Show last 30 days</Button>
            )
          }
        >
          A trace belongs here when its root span names {owner}.
        </EmptyState>
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
  );
}
