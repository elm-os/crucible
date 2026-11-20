import { Tabs, TabsList, TabsPanel, TabsTab } from "@ui-sdk/react/tabs";
import type { ReactNode } from "react";
import type { Level } from "@/api/generated/Level";
import type { PayloadField } from "@/api/generated/PayloadField";
import { EmptyState } from "@/components/empty-state";
import { useLoadingPhase } from "@/components/section";
import { StatusLabel } from "@/components/status";
import { MediaUses } from "@/features/payload/media-uses";
import { PayloadViewer } from "@/features/payload/payload-viewer";
import { formatCost, formatCount, formatDuration, formatOffset, formatTime } from "@/lib/format";
import { useUrlParam } from "@/lib/url-state";
import { TYPE_LABEL } from "./span-waterfall";
import type { TraceNode, TraceTree } from "./trace-tree";

const STATUS_LABEL = { error: "Error", ok: "OK", unset: "Unset" } as const;

const LEVEL_LABEL: Record<Level, string> = {
  debug: "Debug",
  default: "Default",
  warning: "Warning",
  error: "Error",
};

// The payload tabs: the field each reads and the noun its copy uses.
const PAYLOAD_TABS: { field: PayloadField; label: string; noun: string }[] = [
  { field: "input", label: "Input", noun: "input" },
  { field: "output", label: "Output", noun: "output" },
  { field: "attributes", label: "Attributes", noun: "attributes" },
  { field: "events", label: "Events", noun: "events" },
];

export function SpanPanelBody({
  project,
  trace,
  tree,
  paged,
  requested,
  selected,
}: {
  project: string;
  trace: string;
  tree: TraceTree;
  paged: boolean;
  requested: string | null;
  selected: string | null;
}) {
  if (requested === null) {
    return (
      <EmptyState kind="filtered" title="Select a span.">
        Choose a span in the timeline to see its timing, status, model, tokens and cost.
      </EmptyState>
    );
  }
  if (selected === null) {
    return (
      <EmptyState kind="filtered" title="This span link isn't valid.">
        A span id is 16 hexadecimal characters. Choose a span in the timeline instead.
      </EmptyState>
    );
  }
  const node = tree.nodes.get(selected);
  if (!node) {
    return paged ? (
      <EmptyState kind="incomplete" title="This span isn't loaded yet.">
        This trace is large, so its spans load as you expand them. Expand the spans above it in the
        timeline to find it.
      </EmptyState>
    ) : (
      <EmptyState kind="filtered" title="This span isn't in this trace.">
        It may belong to another trace. Choose a span in the timeline instead.
      </EmptyState>
    );
  }
  return <SpanDetails project={project} trace={trace} node={node} tree={tree} />;
}

function SpanDetails({
  project,
  trace,
  node,
  tree,
}: {
  project: string;
  trace: string;
  node: TraceNode;
  tree: TraceTree;
}) {
  const { span } = node;
  const [tabParam, setTab] = useUrlParam("tab");
  const tab = PAYLOAD_TABS.some((t) => t.field === tabParam) ? tabParam : "overview";
  const payloadSpan = { traceId: trace, spanId: span.span_id, startTime: span.start_time };
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-col gap-0.5">
        <p className="text-body font-medium break-words">{span.name || "Unnamed span"}</p>
        <p className="text-label text-ink-soft">{TYPE_LABEL[span.observation_type]}</p>
      </div>
      <Tabs
        value={tab}
        onValueChange={(next) => setTab(next === "overview" || next === null ? null : next)}
        className="flex flex-col gap-3"
      >
        <TabsList aria-label="Span details" className="flex flex-wrap gap-1 text-data">
          <TabsTab value="overview">Overview</TabsTab>
          {PAYLOAD_TABS.map((t) => (
            <TabsTab key={t.field} value={t.field}>
              {t.label}
            </TabsTab>
          ))}
        </TabsList>
        <TabsPanel value="overview">
          <SpanOverview node={node} tree={tree} />
        </TabsPanel>
        {PAYLOAD_TABS.map((t) => (
          <TabsPanel key={t.field} value={t.field} className="flex flex-col gap-4">
            <PayloadViewer
              key={`${span.span_id}/${t.field}`}
              project={project}
              span={payloadSpan}
              field={t.field}
              noun={t.noun}
              ioShape={span.io_shape}
            />
            <MediaUses
              key={`${span.span_id}/${t.field}/media`}
              project={project}
              traceId={trace}
              spanId={span.span_id}
              field={t.field}
              noun={t.noun}
            />
          </TabsPanel>
        ))}
      </Tabs>
    </div>
  );
}

function SpanOverview({ node, tree }: { node: TraceNode; tree: TraceTree }) {
  const { span } = node;
  const childCount = Math.max(span.child_count ?? 0, node.children.length);
  return (
    <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1.5 text-data">
      <Field label="Status">
        <StatusLabel status={span.status_code} label={STATUS_LABEL[span.status_code]} />
      </Field>
      <Field label="Level">{LEVEL_LABEL[span.level]}</Field>
      <Field label="Started">
        {formatTime(span.start_time)}{" "}
        <span className="text-ink-soft">({formatOffset(node.startUs - tree.startUs)})</span>
      </Field>
      <Field label="Duration">{formatDuration(span.duration_us)}</Field>
      {span.model ? <Field label="Model">{span.model}</Field> : null}
      {span.total_tokens > 0 ? (
        <Field label="Tokens">
          {formatCount(span.input_tokens)} in, {formatCount(span.output_tokens)} out,{" "}
          {formatCount(span.total_tokens)} total
        </Field>
      ) : null}
      {span.total_cost_usd > 0 ? (
        <Field label="Cost">{formatCost(span.total_cost_usd)}</Field>
      ) : null}
      <Field label="Child spans">{formatCount(childCount)}</Field>
      <Field label="Span id">
        <span className="font-mono break-all">{span.span_id}</span>
      </Field>
      <Field label="Parent span">
        {span.parent_span_id ? (
          <span className="font-mono break-all">{span.parent_span_id}</span>
        ) : (
          "None, this is a root span"
        )}
      </Field>
    </dl>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-ink-soft">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </>
  );
}

export function SpanPanelSkeleton({ loading }: { loading: boolean }) {
  const phase = useLoadingPhase(loading);
  return (
    <div
      aria-hidden="true"
      className={`flex flex-col gap-3 py-1 ${phase === "hidden" ? "invisible" : ""}`}
    >
      <div className="skeleton h-4 w-1/2" />
      <div className="skeleton h-3 w-1/4" />
      {["status", "level", "started", "duration", "id"].map((row) => (
        <div key={row} className="flex gap-4">
          <div className="skeleton h-3 w-16" />
          <div className="skeleton h-3 w-2/5" />
        </div>
      ))}
    </div>
  );
}
