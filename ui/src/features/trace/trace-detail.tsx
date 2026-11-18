"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { getTrace } from "@/api/client";
import type { Span } from "@/api/generated/Span";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { Section, SectionFrame } from "@/components/section";
import { MetaNotice, StatusLabel } from "@/components/status";
import { formatDuration, formatOffset, parseMicros } from "@/lib/format";
import { useProject } from "@/lib/project";

const COLUMNS = ["Name", "Start", "Duration", "Status"] as const;
const SKELETON_ROWS = Array.from({ length: 8 }, (_, row) => `skeleton-row-${row}`);
const STATUS_LABEL = { error: "Error", ok: "OK", unset: "Unset" } as const;

export function TraceDetail() {
  const params = useSearchParams();
  const project = useProject();
  const trace = params.get("trace");
  const span = params.get("span");
  const fromUrl = params.get("project");
  const tracesHref = (
    fromUrl ? `/traces?${new URLSearchParams({ project: fromUrl })}` : "/traces"
  ) as Route;

  if (!trace) {
    return (
      <>
        <PageHeader title="Trace" />
        <SectionFrame title="Trace">
          <EmptyState
            kind="filtered"
            title="No trace selected."
            action={<Link href={tracesHref}>Open the traces list</Link>}
          >
            Open a trace from the list to see its spans here.
          </EmptyState>
        </SectionFrame>
      </>
    );
  }

  return (
    <>
      <PageHeader
        title="Trace"
        description={<span className="font-mono break-all">{trace}</span>}
      />
      <div className="grid gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <SpansSection project={project} trace={trace} />
        <SectionFrame title="Span">
          <EmptyState kind="planned" title="Span details will render here.">
            {span ? (
              <>
                Selected span <span className="font-mono break-all text-midnight-ink">{span}</span>
              </>
            ) : (
              "Selecting a span will show its input, output and attributes."
            )}
          </EmptyState>
        </SectionFrame>
      </div>
    </>
  );
}

function SpansSection({ project, trace }: { project: string; trace: string }) {
  const query = useQuery({
    queryKey: [project, "trace", trace],
    queryFn: ({ signal }) => getTrace(project, trace, signal),
  });
  return (
    <Section
      title="Spans"
      query={query}
      skeleton={<SpansSkeleton />}
      isEmpty={(detail) => detail.spans.length === 0}
      empty={
        <EmptyState
          kind="incomplete"
          title="This trace has no spans to show yet."
          action={<Button onClick={() => query.refetch()}>Check again</Button>}
        >
          Its spans may still be arriving, or part of the data couldn&apos;t be read.
        </EmptyState>
      }
    >
      {(detail) => (
        <>
          <MetaNotice meta={detail.meta} noun="spans" />
          <SpansTable spans={detail.spans} />
          {detail.paged ? (
            <p className="mt-3 text-label text-ink-soft">
              Showing the root spans and their children, {detail.spans.length} of{" "}
              {detail.span_count}. Deeper spans will load with the span tree.
            </p>
          ) : null}
        </>
      )}
    </Section>
  );
}

function SpansTable({ spans }: { spans: Span[] }) {
  const starts = spans.map((span) => parseMicros(span.start_time));
  const known = starts.filter((start): start is number => start !== null);
  const traceStart = known.length > 0 ? Math.min(...known) : null;
  return (
    <div className="overflow-x-auto">
      <table className="w-full min-w-[32rem] text-start text-data">
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
          {spans.map((span, index) => {
            const start = starts[index];
            return (
              <tr key={span.span_id} className="h-8 border-t border-ash-border">
                <td className="max-w-80 truncate pe-4">{span.name || "Unnamed span"}</td>
                <td className="pe-4 whitespace-nowrap tabular-nums">
                  {start !== null && traceStart !== null ? formatOffset(start - traceStart) : ""}
                </td>
                <td className="pe-4 whitespace-nowrap tabular-nums">
                  {formatDuration(span.duration_us)}
                </td>
                <td className="whitespace-nowrap">
                  <StatusLabel status={span.status_code} label={STATUS_LABEL[span.status_code]} />
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function SpansSkeleton() {
  return (
    <div>
      <div className="h-8" />
      {SKELETON_ROWS.map((id) => (
        <div key={id} className="flex h-8 items-center gap-4 border-t border-ash-border">
          <div className="skeleton h-3 w-2/5" />
          <div className="skeleton h-3 w-1/12" />
          <div className="skeleton h-3 w-1/12" />
        </div>
      ))}
    </div>
  );
}
