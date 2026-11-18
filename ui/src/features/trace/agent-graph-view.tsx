"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { useId } from "react";
import { asUiError } from "@/api/errors";
import { EmptyState } from "@/components/empty-state";
import { AlertIcon } from "@/components/icons";
import { ErrorCard, useLoadingPhase } from "@/components/section";
import { formatCount } from "@/lib/format";
import {
  AgentGraph,
  AgentGraphNode,
  AgentGraphWorld,
} from "@/primitives/agent-graph/agent-graph-parts";
import type { AgentGraphModel, GraphEdge, GraphKind, GraphNode } from "./trace-tree";

const NODE_WIDTH = 156;
const NODE_HEIGHT = 60;

export const KIND_LABEL: Record<GraphKind, string> = {
  agent: "Agent",
  tool: "Tool",
  generation: "Model",
};

type Point = { x: number; y: number };

interface Layout {
  nodes: (GraphNode & Point)[];
  edges: (GraphEdge & { points: Point[] })[];
  width: number;
  height: number;
}

type Dagre = typeof import("@dagrejs/dagre");

// Nodes are listed in flow order (left to right, then top to bottom), which is the order the arrow
// keys and screen readers step through them.
function layoutGraph(dagre: Dagre, model: AgentGraphModel): Layout {
  const graph = new dagre.graphlib.Graph();
  graph.setGraph({ rankdir: "LR", nodesep: 16, ranksep: 48, marginx: 8, marginy: 8 });
  graph.setDefaultEdgeLabel(() => ({}));
  for (const node of model.nodes) {
    graph.setNode(node.id, { width: NODE_WIDTH, height: NODE_HEIGHT });
  }
  for (const edge of model.edges) graph.setEdge(edge.from, edge.to);
  dagre.layout(graph);
  const nodes = model.nodes
    .map((node) => {
      const { x, y } = graph.node(node.id);
      return { ...node, x: x - NODE_WIDTH / 2, y: y - NODE_HEIGHT / 2 };
    })
    .toSorted((a, b) => a.x - b.x || a.y - b.y);
  const edges = model.edges.map((edge) => ({
    ...edge,
    points: graph.edge(edge.from, edge.to).points ?? [],
  }));
  const { width = 0, height = 0 } = graph.graph();
  return { nodes, edges, width, height };
}

const times = (count: number) => (count === 1 ? "1 time" : `${formatCount(count)} times`);

// SVG edges are hidden from assistive tech, so each node also says what it calls and who calls it.
function callsOf(node: GraphNode, model: AgentGraphModel, names: Map<string, string>): string {
  const calls = model.edges
    .filter((edge) => edge.from === node.id)
    .map((edge) => `${names.get(edge.to)} ${times(edge.calls)}`);
  const callers = model.edges
    .filter((edge) => edge.to === node.id)
    .map((edge) => `${names.get(edge.from)} ${times(edge.calls)}`);
  return [
    calls.length > 0 ? `Calls ${calls.join(", ")}.` : "",
    callers.length > 0 ? `Called by ${callers.join(", ")}.` : "",
  ]
    .filter(Boolean)
    .join(" ");
}

export function AgentGraphView({
  model,
  paged,
  loadedSpans,
  selected,
  onSelect,
  onShowTimeline,
}: {
  model: AgentGraphModel;
  paged: boolean;
  loadedSpans: number;
  selected: string | null;
  onSelect: (node: string | null) => void;
  onShowTimeline: () => void;
}) {
  const empty = model.nodes.length === 0;
  // The layout library loads only when a graph is shown, and only once.
  const dagre = useQuery({
    queryKey: ["dagre"],
    queryFn: (): Promise<Dagre> => import("@dagrejs/dagre"),
    enabled: !empty,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
    structuralSharing: false,
    retry: false,
  });
  const phase = useLoadingPhase(!empty && dagre.isPending);
  const describedBy = useId();

  if (empty) {
    return paged ? (
      <EmptyState kind="incomplete" title="No agents, tools or model calls among the loaded spans.">
        This trace is large, so its spans load as you expand them in the timeline.
      </EmptyState>
    ) : (
      <EmptyState
        kind="filtered"
        title="This trace has no agents, tools or model calls."
        action={<Button onClick={onShowTimeline}>Show the timeline</Button>}
      >
        The graph shows spans of those types. The timeline shows every span.
      </EmptyState>
    );
  }
  if (dagre.isPending) {
    return (
      <>
        <output className="sr-only">{phase === "hidden" ? "" : "Loading agent graph"}</output>
        <div
          aria-hidden="true"
          className={`flex h-48 items-center gap-16 px-4 ${phase === "hidden" ? "invisible" : ""}`}
        >
          <div className="skeleton h-16 w-44" />
          <div className="skeleton h-16 w-44" />
        </div>
      </>
    );
  }
  if (dagre.isError) {
    return (
      <ErrorCard
        error={asUiError(dagre.error)}
        label="agent graph"
        onRetry={() => dagre.refetch()}
      />
    );
  }

  const layout = layoutGraph(dagre.data, model);
  const names = new Map(model.nodes.map((node) => [node.id, node.name]));
  const chosen = model.nodes.find((node) => node.id === selected);
  return (
    <div className="flex flex-col gap-3">
      {paged ? (
        <p className="text-label text-ink-soft">
          The graph covers the {formatCount(loadedSpans)} spans loaded so far.
        </p>
      ) : null}
      <AgentGraph
        aria-label="Agent graph"
        value={selected}
        onValueChange={(value) => onSelect(value)}
        width={layout.width}
        height={layout.height}
        className="relative h-[min(60dvh,26rem)] touch-none overflow-clip rounded-lg border border-ash-border bg-parchment-white"
      >
        <AgentGraphWorld className="absolute top-0 left-0 origin-top-left">
          <div className="relative" style={{ width: layout.width, height: layout.height }}>
            <svg
              aria-hidden="true"
              focusable="false"
              width={layout.width}
              height={layout.height}
              className="absolute inset-0 overflow-visible rtl:-scale-x-100"
            >
              <defs>
                <marker
                  id={`${describedBy}-arrow`}
                  viewBox="0 0 8 8"
                  refX="8"
                  refY="4"
                  markerWidth="7"
                  markerHeight="7"
                  orient="auto-start-reverse"
                >
                  <path d="M0 0 L8 4 L0 8 z" className="fill-ink-soft" />
                </marker>
              </defs>
              {layout.edges.map((edge) => {
                const middle = edge.points[Math.floor(edge.points.length / 2)];
                return (
                  <g key={`${edge.from}->${edge.to}`}>
                    <path
                      d={edge.points
                        .map((point, index) => `${index === 0 ? "M" : "L"}${point.x} ${point.y}`)
                        .join(" ")}
                      className="fill-none stroke-ink-soft"
                      strokeWidth="1.5"
                      markerEnd={`url(#${describedBy}-arrow)`}
                    />
                    {middle && edge.calls > 1 ? (
                      <text
                        x={middle.x}
                        y={middle.y - 6}
                        textAnchor="middle"
                        className="fill-ink-soft text-label"
                      >
                        {edge.calls}×
                      </text>
                    ) : null}
                  </g>
                );
              })}
            </svg>
            {layout.nodes.map((node, index) => (
              <AgentGraphNode
                key={node.id}
                value={node.id}
                label={`${KIND_LABEL[node.kind]} ${node.name}`}
                aria-describedby={`${describedBy}-${index}`}
                style={{
                  insetInlineStart: node.x,
                  top: node.y,
                  width: NODE_WIDTH,
                  height: NODE_HEIGHT,
                }}
                className="absolute flex cursor-default flex-col justify-center gap-0.5 rounded-md border border-control-edge bg-warm-sand px-3 select-none hover:bg-parchment-white focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-midnight-ink data-selected:shadow-[inset_0_0_0_2px_var(--color-midnight-ink)]"
              >
                {/* The spaces between flex items don't render, but they keep the node's name as words. */}
                <span className="text-label text-ink-soft">{KIND_LABEL[node.kind]}</span>{" "}
                <span className="truncate text-data font-medium">{node.name}</span>{" "}
                <span className="flex items-center gap-2 text-label text-ink-soft">
                  {formatCount(node.spans.length)} {node.spans.length === 1 ? "span" : "spans"}{" "}
                  {node.errors > 0 ? (
                    <span className="flex items-center gap-1 text-midnight-ink">
                      <AlertIcon width="12" height="12" className="shrink-0" />
                      {node.errors === 1 ? "1 error" : `${formatCount(node.errors)} errors`}
                    </span>
                  ) : null}
                </span>
              </AgentGraphNode>
            ))}
          </div>
        </AgentGraphWorld>
      </AgentGraph>
      <div hidden>
        {layout.nodes.map((node, index) => (
          <span key={node.id} id={`${describedBy}-${index}`}>
            {callsOf(node, model, names)}
          </span>
        ))}
      </div>
      {chosen ? (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2 text-data">
          <p>
            The timeline shows the {formatCount(chosen.spans.length)}{" "}
            {chosen.spans.length === 1 ? "span" : "spans"} of{" "}
            {KIND_LABEL[chosen.kind].toLowerCase()} {chosen.name}.
          </p>
          <Button onClick={onShowTimeline}>Show in timeline</Button>
        </div>
      ) : (
        <p className="text-label text-ink-soft">
          Choose a node to show its spans in the timeline. Use + and - to zoom.
        </p>
      )}
    </div>
  );
}
