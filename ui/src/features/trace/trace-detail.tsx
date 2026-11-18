"use client";

import { type UseQueryResult, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Drawer,
  DrawerBackdrop,
  DrawerClose,
  DrawerContent,
  DrawerTitle,
  DrawerTrigger,
} from "@ui-sdk/react/drawer";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import type { Route } from "next";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { type ReactNode, useState } from "react";
import { getTrace } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { TraceDetail as TraceDetailBody } from "@/api/generated/TraceDetail";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { Section, SectionFrame, useLoadingPhase } from "@/components/section";
import { MetaNotice, TraceStatusLabel } from "@/components/status";
import { formatCost, formatCount, formatDuration, formatTime } from "@/lib/format";
import { parseSpanId, parseTraceId } from "@/lib/ids";
import { useProject } from "@/lib/project";
import { useUrlParam } from "@/lib/url-state";
import { TraceFactsSection } from "./trace-facts";
import {
  childPagesKey,
  type PageRequests,
  useChildPages,
  withFirstPage,
  withNextPage,
  withRestart,
} from "./child-pages";
import { AgentGraphView, KIND_LABEL } from "./agent-graph-view";
import { SpanPanelBody, SpanPanelSkeleton } from "./span-panel";
import { SpanWaterfall } from "./span-waterfall";
import {
  agentGraphModel,
  ancestorsOf,
  buildTraceTree,
  initiallyExpanded,
  type TraceTree,
  traceTotals,
  waterfallItems,
} from "./trace-tree";

const SKELETON_ROWS = Array.from({ length: 8 }, (_, row) => `skeleton-row-${row}`);

// The span panel sits beside the timeline from this width; below it, it opens as a sheet.
const WIDE = "(min-width: 64rem)";
const isWide = () => window.matchMedia?.(WIDE).matches !== false;

export function TraceDetail() {
  const params = useSearchParams();
  const project = useProject();
  const requested = params.get("trace");
  const trace = parseTraceId(requested);
  const fromUrl = params.get("project");
  const tracesHref = (
    fromUrl ? `/traces?${new URLSearchParams({ project: fromUrl })}` : "/traces"
  ) as Route;

  if (trace === null) {
    return (
      <>
        <PageHeader title="Trace" />
        <SectionFrame title="Trace">
          {requested ? (
            <EmptyState
              kind="filtered"
              title="This trace link isn't valid."
              action={<Link href={tracesHref}>Open the traces list</Link>}
            >
              A trace id is 32 hexadecimal characters. Open the trace from the list instead.
            </EmptyState>
          ) : (
            <EmptyState
              kind="filtered"
              title="No trace selected."
              action={<Link href={tracesHref}>Open the traces list</Link>}
            >
              Open a trace from the list to see its spans here.
            </EmptyState>
          )}
        </SectionFrame>
      </>
    );
  }

  return <TraceView key={`${project}/${trace}`} project={project} trace={trace} />;
}

function TraceView({ project, trace }: { project: string; trace: string }) {
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: [project, "trace", trace],
    queryFn: ({ signal }) => getTrace(project, trace, signal),
  });
  const [requests, setRequests] = useState<PageRequests>(new Map());
  const children = useChildPages(project, trace, requests);
  const [spanParam, setSpanParam] = useUrlParam("span");
  const [viewParam, setView] = useUrlParam("view");
  const [expandedState, setExpanded] = useState<string[] | null>(null);
  const [sheetOpen, setSheetOpen] = useState(false);
  const [graphNode, setGraphNode] = useState<string | null>(null);

  const detail = query.data;
  const tree = detail ? buildTraceTree(detail.spans, children.pages) : null;
  const expanded = expandedState ?? (tree && detail ? initiallyExpanded(tree, detail.paged) : []);
  const selected = parseSpanId(spanParam);
  const view = viewParam === "graph" ? "graph" : "timeline";
  const graph = tree ? agentGraphModel(tree) : null;
  const filter = graph?.nodes.find((node) => node.id === graphNode) ?? null;

  // Requests are state, not derived, so a page stays loaded after its span closes; a requested span is
  // no longer unloaded, so this settles in one extra render.
  const wanted = tree ? expanded.filter((id) => tree.nodes.get(id)?.more === "unloaded") : [];
  if (wanted.length > 0) setRequests(wanted.reduce(withFirstPage, requests));

  const loadMore = (parent: string) => {
    const node = tree?.nodes.get(parent);
    const last = children.pages.get(parent)?.at(-1);
    if (node?.more === "error") {
      // A cursor the server no longer accepts restarts that span's children from their first page.
      if (last?.status === "error" && asUiError(last.error).kind === "cursor_reset") {
        queryClient.removeQueries({ queryKey: childPagesKey(project, trace, parent) });
        setRequests(withRestart(requests, parent));
      } else {
        children.retry(parent);
      }
      return;
    }
    const cursor = last?.status === "success" ? last.page.next_cursor : null;
    if (node?.more === "more" && cursor) setRequests(withNextPage(requests, parent, cursor));
  };

  const select = (span: string | null) => {
    setSpanParam(span);
    if (span !== null && !isWide()) setSheetOpen(true);
  };

  // A chosen graph node's spans must show in the timeline, so the spans above them open.
  const chooseGraphNode = (id: string | null) => {
    setGraphNode(id);
    const node = graph?.nodes.find((candidate) => candidate.id === id);
    if (!node || !tree) return;
    const open = new Set(expanded);
    for (const span of node.spans)
      for (const ancestor of ancestorsOf(tree, span)) open.add(ancestor);
    setExpanded([...open]);
  };

  const panel =
    tree && detail ? (
      <SpanPanelBody tree={tree} paged={detail.paged} requested={spanParam} selected={selected} />
    ) : null;

  return (
    <>
      <PageHeader
        title="Trace"
        description={<span className="font-mono break-all">{trace}</span>}
      />
      <TraceSummary query={query} tree={tree} />
      {detail && detail.spans.length > 0 ? (
        <SegmentedControl
          aria-label="View"
          value={view}
          onValueChange={(next) => setView(next === "graph" ? "graph" : null)}
          className="mb-3"
        >
          <SegmentedControlIndicator />
          <SegmentedControlItem value="timeline" className="text-data">
            Timeline
          </SegmentedControlItem>
          <SegmentedControlItem value="graph" className="text-data">
            Graph
          </SegmentedControlItem>
        </SegmentedControl>
      ) : null}
      <div className="grid gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <Section
          title={view === "graph" ? "Agent graph" : "Spans"}
          className="min-w-0 lg:self-start"
          query={query}
          skeleton={<SpansSkeleton />}
          isEmpty={(body) => body.spans.length === 0}
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
          {(body) => {
            if (!tree || !graph) return null;
            if (view === "graph") {
              return (
                <AgentGraphView
                  model={graph}
                  paged={body.paged}
                  loadedSpans={tree.nodes.size}
                  selected={filter?.id ?? null}
                  onSelect={chooseGraphNode}
                  onShowTimeline={() => setView(null)}
                />
              );
            }
            return (
              <>
                {filter ? (
                  <div
                    data-filter
                    className="mb-3 flex flex-wrap items-center gap-x-4 gap-y-2 text-data"
                  >
                    <p>
                      Showing the {formatCount(filter.spans.length)}{" "}
                      {filter.spans.length === 1 ? "span" : "spans"} of{" "}
                      {KIND_LABEL[filter.kind].toLowerCase()} {filter.name}, with the spans above
                      them.
                    </p>
                    <Button onClick={() => setGraphNode(null)}>Show all spans</Button>
                  </div>
                ) : null}
                <SpanWaterfall
                  tree={tree}
                  items={waterfallItems(tree, filter ? new Set(filter.spans) : null)}
                  expanded={expanded}
                  onExpandedChange={setExpanded}
                  selected={selected !== null && tree.nodes.has(selected) ? selected : null}
                  onSelect={select}
                  onMore={loadMore}
                />
              </>
            );
          }}
        </Section>
        {query.isPending || panel ? (
          <SectionFrame
            title="Span"
            busy={query.isPending}
            className="max-lg:hidden lg:sticky lg:top-4 lg:self-start"
          >
            {panel ?? <SpanPanelSkeleton loading={query.isPending} />}
          </SectionFrame>
        ) : null}
      </div>
      <div className="mt-6">
        <TraceFactsSection project={project} trace={trace} query={query} />
      </div>
      {panel && detail && detail.spans.length > 0 ? (
        <Drawer side="bottom" open={sheetOpen} onOpenChange={setSheetOpen}>
          {spanParam !== null ? (
            <DrawerTrigger className="mt-4 lg:hidden">Show span details</DrawerTrigger>
          ) : null}
          <DrawerBackdrop className="bg-midnight-ink/20" />
          <DrawerContent className="flex max-h-[85dvh] flex-col gap-3 rounded-t-xl border-t border-ash-border bg-parchment-white px-4 pt-3 pb-[max(1rem,env(safe-area-inset-bottom))]">
            <div className="flex items-center justify-between gap-3">
              <DrawerTitle className="text-label font-medium text-ink-soft">Span</DrawerTitle>
              <DrawerClose>Close</DrawerClose>
            </div>
            {/* A drag scrolls this area first; at its top, the same drag moves the sheet. */}
            <div className="min-h-0 flex-1 overflow-y-auto">{panel}</div>
          </DrawerContent>
        </Drawer>
      ) : null}
    </>
  );
}

function TraceSummary({
  query,
  tree,
}: {
  query: UseQueryResult<TraceDetailBody>;
  tree: TraceTree | null;
}) {
  const phase = useLoadingPhase(query.isPending);
  if (query.isPending) {
    return (
      <div
        aria-hidden="true"
        data-loading="summary"
        className={`mb-6 flex flex-col gap-2 ${phase === "hidden" ? "invisible" : ""}`}
      >
        <div className="skeleton h-5 w-1/3" />
        <div className="skeleton h-3 w-2/3" />
      </div>
    );
  }
  const detail = query.data;
  // Without a trace, the spans section shows the error or the empty state for both.
  if (!detail || !tree || detail.spans.length === 0) return null;
  const totals = traceTotals(tree);
  return (
    <div className="mb-6 flex flex-col gap-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <p className="text-subheading font-medium break-all">{totals.name}</p>
        <TraceStatusLabel errors={totals.errors} status={totals.root?.status_code ?? "unset"} />
      </div>
      <dl className="flex flex-wrap gap-x-6 gap-y-1 text-data">
        {totals.start ? <Fact label="Started">{formatTime(totals.start)}</Fact> : null}
        <Fact label="Duration">{formatDuration(totals.durationUs)}</Fact>
        <Fact label="Cost">{formatCost(totals.costUsd)}</Fact>
        <Fact label="Tokens">{formatCount(totals.tokens)}</Fact>
        <Fact label="Spans">{formatCount(detail.span_count)}</Fact>
      </dl>
      {detail.paged ? (
        <p className="text-label text-ink-soft">
          This trace is large. Duration, cost and tokens cover the {formatCount(tree.nodes.size)}{" "}
          spans loaded so far; expanding a span loads its children.
        </p>
      ) : null}
      <MetaNotice meta={detail.meta} noun="spans" />
    </div>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex gap-1.5">
      <dt className="text-ink-soft">{label}</dt>
      <dd className="tabular-nums">{children}</dd>
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
          <div className="skeleton h-3 w-1/4" />
          <div className="skeleton h-3 w-1/12" />
        </div>
      ))}
    </div>
  );
}
