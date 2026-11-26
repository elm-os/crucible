import { describe, expect, test } from "vitest";
import type { Span } from "@/api/generated/Span";
import type { SpanPage } from "@/api/generated/SpanPage";
import {
  agentGraphModel,
  ancestorsOf,
  buildTraceTree,
  type ChildLoad,
  initiallyExpanded,
  moreRowParent,
  moreRowValue,
  type TraceNode,
  waterfallItems,
} from "./trace-tree";

const BASE = Date.parse("2026-11-17T14:00:00Z") * 1_000;

function at(offsetUs: number): string {
  const us = BASE + offsetUs;
  const seconds = new Date(Math.floor(us / 1_000_000) * 1_000).toISOString().slice(0, 19);
  return `${seconds}.${String(us % 1_000_000).padStart(6, "0")}Z`;
}

function span(
  id: string,
  parent: string | null,
  startUs: number,
  fields: Partial<Span> = {},
): Span {
  const duration = fields.duration_us ?? 1_000;
  return {
    span_id: id,
    parent_span_id: parent,
    name: `span ${id}`,
    observation_type: "span",
    start_time: at(startUs),
    end_time: at(startUs + duration),
    duration_us: duration,
    status_code: "unset",
    level: "default",
    model: "",
    total_cost_usd: 0,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    child_count: null,
    io_shape: null,
    input_truncated: null,
    output_truncated: null,
    saturated: false,
    ...fields,
  };
}

function page(data: Span[], hasMore = false): SpanPage {
  return {
    data,
    next_cursor: hasMore ? "next" : null,
    has_more: hasMore,
    meta: { as_of: at(0), partial: false, consistency: "strong" },
  };
}

function shape(nodes: TraceNode[]): unknown[] {
  return nodes.map((node) =>
    node.children.length > 0 ? [node.span.span_id, shape(node.children)] : node.span.span_id,
  );
}

function walk(nodes: TraceNode[], visit: (node: TraceNode, parent: TraceNode | null) => void) {
  const stack = nodes.map((node) => ({ node, parent: null as TraceNode | null }));
  for (let entry = stack.pop(); entry; entry = stack.pop()) {
    visit(entry.node, entry.parent);
    for (const child of entry.node.children) stack.push({ node: child, parent: entry.node });
  }
}

describe("the span tree", () => {
  test("nests spans under their parents in start order", () => {
    const tree = buildTraceTree([
      span("c", "a", 300),
      span("a", null, 0, { duration_us: 2_000 }),
      span("b", "a", 100),
      span("d", "b", 150),
    ]);
    expect(shape(tree.roots)).toEqual([["a", [["b", ["d"]], "c"]]]);
    expect(tree.parents.get("d")).toBe("b");
    expect(tree.parents.get("a")).toBeNull();
    expect(tree.startUs).toBe(BASE);
    expect(tree.endUs).toBe(BASE + 2_000);
  });

  test("spans that start together are ordered by span id", () => {
    const tree = buildTraceTree([span("b", null, 0), span("a", null, 0)]);
    expect(shape(tree.roots)).toEqual(["a", "b"]);
  });

  test("a span whose parent is not in the trace is shown at the top level", () => {
    const tree = buildTraceTree([span("a", null, 50), span("orphan", "gone", 0)]);
    expect(shape(tree.roots)).toEqual(["orphan", "a"]);
    expect(tree.parents.get("orphan")).toBeNull();
  });

  test("the earliest span of a parent cycle is shown at the top level", () => {
    const tree = buildTraceTree([
      span("self", "self", 0),
      span("x", "y", 20),
      span("y", "x", 10),
      span("z", "y", 30),
    ]);
    expect(shape(tree.roots)).toEqual(["self", ["y", ["x", "z"]]]);
  });

  test("a span that hangs from a cycle stays under its parent, even when it starts first", () => {
    const tree = buildTraceTree([span("early", "x", 0), span("x", "y", 20), span("y", "x", 10)]);
    expect(shape(tree.roots)).toEqual([["y", [["x", ["early"]]]]]);
  });

  test("property: every span appears once, under its own parent unless it is at the top level", () => {
    let seed = 0x5eed;
    const random = () => {
      seed = (seed + 0x6d2b79f5) | 0;
      let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4_294_967_296;
    };
    for (let run = 0; run < 300; run += 1) {
      const count = 1 + Math.floor(random() * 40);
      const ids = Array.from({ length: count }, (_, index) => `s${index}`);
      const spans = ids.map((id) => {
        const roll = random();
        let parent: string | null = null;
        if (roll < 0.75) parent = ids[Math.floor(random() * count)] ?? null;
        else if (roll < 0.85) parent = "missing";
        return span(id, parent, Math.floor(random() * 50));
      });
      const duplicates = spans.filter(() => random() < 0.1);
      const tree = buildTraceTree([...spans, ...duplicates]);

      const seen: string[] = [];
      walk(tree.roots, (node, parent) => {
        seen.push(node.span.span_id);
        if (parent !== null) expect(node.span.parent_span_id).toBe(parent.span.span_id);
        for (let index = 1; index < node.children.length; index += 1) {
          const [before, after] = [node.children[index - 1], node.children[index]];
          expect(before && after && before.startUs <= after.startUs).toBe(true);
        }
      });
      expect(seen.toSorted()).toEqual(ids.toSorted());
      for (const root of tree.roots) {
        const id = root.span.span_id;
        const parent = root.span.parent_span_id;
        // A top-level span with a loaded parent starts a cycle: its parent is in its own subtree.
        if (parent !== null && tree.nodes.has(parent)) {
          expect(parent === id || ancestorsOf(tree, parent).includes(id)).toBe(true);
        }
      }
    }
  });
});

describe("paged traces", () => {
  const root = span("root", null, 0, { child_count: 3, duration_us: 10_000 });
  const first = span("c1", "root", 100, { child_count: 2 });

  test("a span with uncounted children waits for its first page", () => {
    const tree = buildTraceTree([root, first]);
    expect(tree.nodes.get("root")?.more).toBe("unloaded");
    expect(tree.nodes.get("c1")?.more).toBe("unloaded");
  });

  test("a span whose children are all here needs nothing more", () => {
    const tree = buildTraceTree([span("root", null, 0, { child_count: 1 }), first]);
    expect(tree.nodes.get("root")?.more).toBeNull();
  });

  test("the last requested page decides: loading, failed, more, or done", () => {
    const states: [ChildLoad[], string | null][] = [
      [[{ status: "pending" }], "loading"],
      [[{ status: "success", page: page([], true) }, { status: "pending" }], "loading"],
      [[{ status: "error", error: new Error("down") }], "error"],
      [[{ status: "success", page: page([span("c2", "root", 200)], true) }], "more"],
      // The count ran ahead of the store: the last page says nothing is left.
      [[{ status: "success", page: page([span("c2", "root", 200)]) }], null],
    ];
    for (const [loads, expected] of states) {
      const tree = buildTraceTree([root, first], new Map([["root", loads]]));
      expect(tree.nodes.get("root")?.more).toBe(expected);
    }
  });

  test("loaded pages join the tree once, under their parent, in start order", () => {
    const pages = new Map<string, ChildLoad[]>([
      [
        "root",
        [
          { status: "success", page: page([first, span("c0", "root", 50)], true) },
          { status: "success", page: page([span("c2", "root", 200)]) },
        ],
      ],
      ["c1", [{ status: "success", page: page([span("g1", "c1", 120)]) }]],
    ]);
    const tree = buildTraceTree([root, first], pages);
    expect(shape(tree.roots)).toEqual([["root", ["c0", ["c1", ["g1"]], "c2"]]]);
    expect(tree.nodes.get("root")?.more).toBeNull();
    expect(tree.nodes.get("c1")?.more).toBeNull();
  });

  test("opening a paged trace expands its roots; an unpaged one expands every branch", () => {
    const tree = buildTraceTree([
      span("a", null, 0),
      span("b", "a", 1),
      span("c", "b", 2),
      span("d", null, 3),
    ]);
    expect(initiallyExpanded(tree, false).toSorted()).toEqual(["a", "b"]);
    expect(initiallyExpanded(tree, true)).toEqual(["a", "d"]);
  });
});

describe("the waterfall items", () => {
  test("times are milliseconds from the trace start, and leaves have no children", () => {
    const tree = buildTraceTree([
      span("a", null, 1_000, { duration_us: 4_000, name: "" }),
      span("b", "a", 1_500, { duration_us: 300 }),
    ]);
    expect(waterfallItems(tree)).toEqual([
      {
        value: "a",
        label: "Unnamed span",
        start: 0,
        duration: 4,
        children: [{ value: "b", label: "span b", start: 0.5, duration: 0.3 }],
      },
    ]);
  });

  test("a span with more children ends with a row that loads them", () => {
    const tree = buildTraceTree([span("a", null, 0, { child_count: 5 }), span("b", "a", 10)]);
    const [item] = waterfallItems(tree);
    expect(item?.children?.map((child) => child.value)).toEqual(["b", moreRowValue("a")]);
    expect(item?.children?.[1]?.label).toBe("Loading child spans");
    expect(moreRowParent(moreRowValue("a"))).toBe("a");
    expect(moreRowParent("a")).toBeNull();
  });

  test("a filter keeps its spans with their ancestors and drops the loading rows", () => {
    const tree = buildTraceTree([
      span("a", null, 0, { child_count: 9 }),
      span("b", "a", 1),
      span("c", "b", 2),
      span("d", "a", 3),
    ]);
    const items = waterfallItems(tree, new Set(["c"]));
    expect(items).toEqual([
      expect.objectContaining({
        value: "a",
        children: [
          expect.objectContaining({
            value: "b",
            children: [expect.objectContaining({ value: "c" })],
          }),
        ],
      }),
    ]);
    expect(JSON.stringify(items)).not.toContain('"d"');
    expect(JSON.stringify(items)).not.toContain("/more");
  });
});

test("the agent graph links each agent, tool and model to the nearest one above it", () => {
  const tree = buildTraceTree([
    span("a", null, 0, { observation_type: "agent", name: "planner" }),
    span("chain", "a", 1, { observation_type: "chain", name: "step" }),
    span("t1", "chain", 2, { observation_type: "tool", name: "search", level: "error" }),
    span("t2", "a", 3, { observation_type: "tool", name: "search" }),
    span("g", "a", 4, { observation_type: "generation", name: "chat", model: "gpt-x" }),
    span("sub", "a", 5, { observation_type: "agent", name: "planner" }),
    span("loose", null, 6, { observation_type: "tool", name: "" }),
  ]);
  const graph = agentGraphModel(tree);
  expect(graph.nodes.map(({ id, spans, errors }) => [id, spans, errors])).toEqual([
    ["agent:planner", ["a", "sub"], 0],
    ["tool:search", ["t1", "t2"], 1],
    ["generation:gpt-x", ["g"], 0],
    ["tool:Unnamed tool", ["loose"], 0],
  ]);
  expect(graph.edges).toEqual([
    { from: "agent:planner", to: "tool:search", calls: 2 },
    { from: "agent:planner", to: "generation:gpt-x", calls: 1 },
  ]);
});
