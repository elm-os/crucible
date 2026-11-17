"use client";

import type { Route } from "next";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";

export function TraceDetail() {
  const params = useSearchParams();
  const trace = params.get("trace");
  const span = params.get("span");
  const project = params.get("project");
  const tracesHref = (project ? `/traces?${new URLSearchParams({ project })}` : "/traces") as Route;

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
        <SectionFrame title="Timeline">
          <EmptyState kind="planned" title="The span tree and timeline will render here.">
            Spans will load from the trace detail endpoint and render as a waterfall.
          </EmptyState>
        </SectionFrame>
        <SectionFrame title="Span">
          {span ? (
            <EmptyState kind="planned" title="Span details will render here.">
              Selected span <span className="font-mono break-all text-midnight-ink">{span}</span>
            </EmptyState>
          ) : (
            <EmptyState kind="filtered" title="Select a span.">
              Choose a span in the timeline to see its input, output and attributes.
            </EmptyState>
          )}
        </SectionFrame>
      </div>
    </>
  );
}
