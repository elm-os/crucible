import type { ReactNode } from "react";
import type { Level } from "@/api/generated/Level";
import { EmptyState } from "@/components/empty-state";
import { useLoadingPhase } from "@/components/section";
import { StatusLabel } from "@/components/status";
import { formatCost, formatCount, formatDuration, formatOffset, formatTime } from "@/lib/format";
import { TYPE_LABEL } from "./span-waterfall";
import type { TraceNode, TraceTree } from "./trace-tree";

export const STATUS_LABEL = { error: "Error", ok: "OK", unset: "Unset" } as const;

const LEVEL_LABEL: Record<Level, string> = {
  debug: "Debug",
  default: "Default",
  warning: "Warning",
  error: "Error",
};

// What the span panel shows for the `span` in the URL: the span, or why there is none.
export function SpanPanelBody({
  tree,
  paged,
  requested,
  selected,
}: {
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
  return <SpanDetails node={node} tree={tree} />;
}

function SpanDetails({ node, tree }: { node: TraceNode; tree: TraceTree }) {
  const { span } = node;
  const childCount = Math.max(span.child_count ?? 0, node.children.length);
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-col gap-0.5">
        <p className="text-body font-medium break-words">{span.name || "Unnamed span"}</p>
        <p className="text-label text-ink-soft">{TYPE_LABEL[span.observation_type]}</p>
      </div>
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
      <EmptyState kind="planned" title="Input, output, attributes and events">
        Previews of the span&apos;s input and output, its attributes and its events will show here
        once the span detail API ships.
      </EmptyState>
    </div>
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
