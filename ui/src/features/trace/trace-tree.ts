import type { WaterfallData, WaterfallDataItem } from "@ui-sdk/react/waterfall";
import type { ObservationType } from "@/api/generated/ObservationType";
import type { Span } from "@/api/generated/Span";
import type { SpanPage } from "@/api/generated/SpanPage";
import { formatCount, parseMicros } from "@/lib/format";

// One requested page of a span's children, as far as it has loaded.
export type ChildLoad =
  | { status: "pending" }
  | { status: "error"; error: unknown }
  | { status: "success"; page: SpanPage };

export type ChildPages = ReadonlyMap<string, readonly ChildLoad[]>;

// What follows a span's loaded children on a paged trace: a row that loads the rest.
export type MoreState = "unloaded" | "loading" | "error" | "more";

export interface TraceNode {
  span: Span;
  startUs: number;
  children: TraceNode[];
  more: MoreState | null;
}

export interface TraceTree {
  roots: TraceNode[];
  nodes: ReadonlyMap<string, TraceNode>;
  // The tree parent of each span: its own parent, or null for a span shown at the top level.
  parents: ReadonlyMap<string, string | null>;
  startUs: number;
  endUs: number;
}

const NO_PAGES: ChildPages = new Map();

function byStart(a: TraceNode, b: TraceNode): number {
  if (a.startUs !== b.startUs) return a.startUs - b.startUs;
  if (a.span.span_id === b.span.span_id) return 0;
  return a.span.span_id < b.span.span_id ? -1 : 1;
}

// Builds the span tree from the trace detail and every loaded page of children. A span whose parent is
// not loaded is shown at the top level, and so is the earliest span of any parent cycle, so every span
// appears exactly once.
export function buildTraceTree(spans: readonly Span[], pages: ChildPages = NO_PAGES): TraceTree {
  const merged = new Map<string, Span>();
  for (const span of spans) merged.set(span.span_id, span);
  for (const loads of pages.values()) {
    for (const load of loads) {
      if (load.status === "success")
        for (const span of load.page.data) merged.set(span.span_id, span);
    }
  }

  const nodes = new Map<string, TraceNode>();
  for (const span of merged.values()) {
    nodes.set(span.span_id, {
      span,
      startUs: parseMicros(span.start_time) ?? 0,
      children: [],
      more: null,
    });
  }

  const childrenOf = new Map<string, TraceNode[]>();
  const tops: TraceNode[] = [];
  for (const node of nodes.values()) {
    const parent = node.span.parent_span_id;
    if (parent === null || !nodes.has(parent)) {
      tops.push(node);
      continue;
    }
    const siblings = childrenOf.get(parent);
    if (siblings) siblings.push(node);
    else childrenOf.set(parent, [node]);
  }

  const parents = new Map<string, string | null>();
  const roots: TraceNode[] = [];
  const attach = (top: TraceNode) => {
    parents.set(top.span.span_id, null);
    roots.push(top);
    const stack = [top];
    for (let node = stack.pop(); node !== undefined; node = stack.pop()) {
      const id = node.span.span_id;
      const children = (childrenOf.get(id) ?? [])
        .filter((child) => !parents.has(child.span.span_id))
        .toSorted(byStart);
      for (const child of children) parents.set(child.span.span_id, id);
      node.children = children;
      stack.push(...children);
    }
  };
  for (const top of tops.toSorted(byStart)) attach(top);
  // What is left hangs from a parent cycle: every span has a loaded parent and no top-level ancestor.
  const unreached = [...nodes.values()].filter((node) => !parents.has(node.span.span_id));
  for (const node of unreached.toSorted(byStart)) {
    if (!parents.has(node.span.span_id)) attach(cycleStart(node, nodes));
  }

  let startUs = Number.POSITIVE_INFINITY;
  let endUs = Number.NEGATIVE_INFINITY;
  for (const node of nodes.values()) {
    node.more = moreState(node, pages.get(node.span.span_id));
    startUs = Math.min(startUs, node.startUs);
    endUs = Math.max(endUs, node.startUs + node.span.duration_us);
  }
  if (nodes.size === 0) {
    startUs = 0;
    endUs = 0;
  }
  return { roots: roots.toSorted(byStart), nodes, parents, startUs, endUs };
}

// Walks up from a span with no top-level ancestor to the cycle above it, and returns the cycle's
// earliest span.
function cycleStart(from: TraceNode, nodes: ReadonlyMap<string, TraceNode>): TraceNode {
  const path: TraceNode[] = [];
  const seen = new Set<string>();
  let node: TraceNode | undefined = from;
  while (node !== undefined && !seen.has(node.span.span_id)) {
    seen.add(node.span.span_id);
    path.push(node);
    const parent: string | null = node.span.parent_span_id;
    node = parent === null ? undefined : nodes.get(parent);
  }
  const cycle = node === undefined ? path : path.slice(path.indexOf(node));
  return cycle.toSorted(byStart)[0] ?? from;
}

// `child_count` is set on paged traces only. It can run ahead of the store, so once a page has loaded,
// the page's `has_more` decides whether more remain.
function moreState(node: TraceNode, loads: readonly ChildLoad[] | undefined): MoreState | null {
  const last = loads?.at(-1);
  if (last === undefined) {
    const count = node.span.child_count;
    return count !== null && count > node.children.length ? "unloaded" : null;
  }
  if (last.status === "pending") return "loading";
  if (last.status === "error") return "error";
  return last.page.has_more ? "more" : null;
}

const MORE_SUFFIX = "/more";

export const MORE_LABEL: Record<MoreState, string> = {
  unloaded: "Loading child spans",
  loading: "Loading child spans",
  error: "Couldn't load child spans. Select to retry.",
  more: "Show more child spans",
};

function moreLabel(node: TraceNode, more: MoreState): string {
  const remaining = (node.span.child_count ?? 0) - node.children.length;
  if (more !== "more" || remaining <= 0) return MORE_LABEL[more];
  return `Show more child spans (${formatCount(remaining)} not shown)`;
}

// The row after a span's children that loads more of them. Span ids are hex, so this never collides.
export function moreRowValue(parent: string): string {
  return `${parent}${MORE_SUFFIX}`;
}

export function moreRowParent(value: string): string | null {
  return value.endsWith(MORE_SUFFIX) ? value.slice(0, -MORE_SUFFIX.length) : null;
}

export function spanLabel(span: Span): string {
  return span.name || "Unnamed span";
}

// The waterfall's items, in milliseconds from the trace start. With `keep`, only those spans and their
// ancestors remain, and the rows that load more children are left out.
export function waterfallItems(
  tree: TraceTree,
  keep: ReadonlySet<string> | null = null,
): WaterfallData {
  const toItem = (node: TraceNode): WaterfallDataItem | null => {
    const id = node.span.span_id;
    const children = node.children
      .map(toItem)
      .filter((item): item is WaterfallDataItem => item !== null);
    if (keep !== null && !keep.has(id) && children.length === 0) return null;
    const start = (node.startUs - tree.startUs) / 1_000;
    if (keep === null && node.more !== null) {
      children.push({
        value: moreRowValue(id),
        label: moreLabel(node, node.more),
        start,
        duration: 0,
      });
    }
    const item: WaterfallDataItem = {
      value: id,
      label: spanLabel(node.span),
      start,
      duration: node.span.duration_us / 1_000,
    };
    return children.length > 0 ? { ...item, children } : item;
  };
  return tree.roots.map(toItem).filter((item): item is WaterfallDataItem => item !== null);
}

// Spans with children, which an unpaged trace opens at first; a paged trace opens its root spans.
export function initiallyExpanded(tree: TraceTree, paged: boolean): string[] {
  if (paged) return tree.roots.map((root) => root.span.span_id);
  const ids: string[] = [];
  for (const node of tree.nodes.values()) if (node.children.length > 0) ids.push(node.span.span_id);
  return ids;
}

export function ancestorsOf(tree: TraceTree, id: string): string[] {
  const chain: string[] = [];
  for (let parent = tree.parents.get(id); parent; parent = tree.parents.get(parent)) {
    chain.push(parent);
  }
  return chain;
}

export interface TraceTotals {
  name: string;
  start: string | null;
  durationUs: number;
  costUsd: number;
  tokens: number;
  errors: number;
  root: Span | null;
}

export function traceTotals(tree: TraceTree): TraceTotals {
  let costUsd = 0;
  let tokens = 0;
  let errors = 0;
  let first: TraceNode | null = null;
  for (const node of tree.nodes.values()) {
    const { span } = node;
    costUsd += span.total_cost_usd;
    tokens += span.total_tokens;
    if (span.level === "error") errors += 1;
    if (first === null || byStart(node, first) < 0) first = node;
  }
  const root = tree.roots.find((node) => node.span.parent_span_id === null) ?? tree.roots[0];
  return {
    name: root?.span.name || "Unnamed trace",
    start: first?.span.start_time ?? null,
    durationUs: tree.endUs - tree.startUs,
    costUsd,
    tokens,
    errors,
    root: root?.span ?? null,
  };
}

export type GraphKind = "agent" | "tool" | "generation";

export interface GraphNode {
  id: string;
  kind: GraphKind;
  name: string;
  spans: string[];
  errors: number;
}

export interface GraphEdge {
  from: string;
  to: string;
  calls: number;
}

export interface AgentGraphModel {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

const GRAPH_KIND: Partial<Record<ObservationType, GraphKind>> = {
  agent: "agent",
  tool: "tool",
  generation: "generation",
};

const UNNAMED: Record<GraphKind, string> = {
  agent: "Unnamed agent",
  tool: "Unnamed tool",
  generation: "Unnamed model",
};

// Agents, tools and models as nodes, one per name, and an edge from the nearest agent, tool or model
// above a span to the span's own node, counting calls. Spans of other types pass their caller through.
export function agentGraphModel(tree: TraceTree): AgentGraphModel {
  const nodes = new Map<string, GraphNode>();
  const edges = new Map<string, GraphEdge>();
  const stack: { node: TraceNode; caller: string | null }[] = tree.roots
    .map((node) => ({ node, caller: null }))
    .toReversed();
  for (let entry = stack.pop(); entry !== undefined; entry = stack.pop()) {
    const { node, caller } = entry;
    const { span } = node;
    const kind = GRAPH_KIND[span.observation_type];
    let here = caller;
    if (kind !== undefined) {
      const name = (kind === "generation" ? span.model || span.name : span.name) || UNNAMED[kind];
      const id = `${kind}:${name}`;
      const graphNode = nodes.get(id) ?? { id, kind, name, spans: [], errors: 0 };
      graphNode.spans.push(span.span_id);
      if (span.level === "error") graphNode.errors += 1;
      nodes.set(id, graphNode);
      if (caller !== null && caller !== id) {
        const key = `${caller}\u001f${id}`;
        const edge = edges.get(key) ?? { from: caller, to: id, calls: 0 };
        edge.calls += 1;
        edges.set(key, edge);
      }
      here = id;
    }
    for (let index = node.children.length - 1; index >= 0; index -= 1) {
      const child = node.children[index];
      if (child) stack.push({ node: child, caller: here });
    }
  }
  return { nodes: [...nodes.values()], edges: [...edges.values()] };
}
