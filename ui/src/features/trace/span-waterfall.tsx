"use client";

import {
  createWaterfallController,
  visibleWaterfallRows,
  Waterfall,
  WaterfallAxis,
  WaterfallBar,
  WaterfallCell,
  WaterfallColumnHeader,
  type WaterfallData,
  type WaterfallDataRow,
  WaterfallHeaderRow,
  WaterfallIndicator,
  WaterfallRow,
  WaterfallRowHeader,
} from "@ui-sdk/react/waterfall";
import { useEffect, useRef, useState } from "react";
import type { ObservationType } from "@/api/generated/ObservationType";
import { AlertIcon, ChevronRightIcon } from "@/components/icons";
import { formatDuration } from "@/lib/format";
import { moreRowParent, type TraceTree } from "./trace-tree";

// Rows have one fixed height, so only the rows in view (plus a margin) are rendered. UI SDK keys every
// cell of a row by the row's value, so a row has one cell of its own: the duration.
const ROW_HEIGHT = 32;
const OVERSCAN = 8;
const COLUMNS =
  "grid grid-cols-[minmax(9rem,3fr)_minmax(5rem,2fr)_4rem] gap-3 sm:grid-cols-[minmax(12rem,2fr)_minmax(8rem,3fr)_4.5rem]";
const ROW = `${COLUMNS} absolute inset-x-0 h-8 items-center border-b border-ash-border px-2`;

export const TYPE_LABEL: Record<ObservationType, string> = {
  span: "Span",
  generation: "Generation",
  event: "Event",
  embedding: "Embedding",
  agent: "Agent",
  tool: "Tool",
  chain: "Chain",
  retriever: "Retriever",
  evaluator: "Evaluator",
  guardrail: "Guardrail",
};

const formatMs = (ms: number) => (ms === 0 ? "0 ms" : formatDuration(ms * 1_000));

export function SpanWaterfall({
  tree,
  items,
  expanded,
  onExpandedChange,
  selected,
  onSelect,
  onMore,
}: {
  tree: TraceTree;
  items: WaterfallData;
  expanded: string[];
  onExpandedChange: (expanded: string[]) => void;
  selected: string | null;
  onSelect: (span: string | null) => void;
  onMore: (parent: string) => void;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [view, setView] = useState({ width: 640, height: 640 });
  const rows = visibleWaterfallRows(items, expanded);
  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const last = Math.min(rows.length, Math.ceil((scrollTop + view.height) / ROW_HEIGHT) + OVERSCAN);
  const length = (tree.endUs - tree.startUs) / 1_000;
  const range: [number, number] = [0, length > 0 ? length : 1];

  // The header row is sticky, so a row is in view only below it.
  const reveal = (index: number) => {
    const el = scroller.current;
    if (!el) return;
    const top = (index + 1) * ROW_HEIGHT;
    if (top < el.scrollTop + ROW_HEIGHT) el.scrollTop = top - ROW_HEIGHT;
    else if (top + ROW_HEIGHT > el.scrollTop + el.clientHeight) {
      el.scrollTop = top + ROW_HEIGHT - el.clientHeight;
    }
    setScrollTop(el.scrollTop);
  };

  // When the row that loads more children leaves after its last page, focus would fall to the page;
  // it moves to the first span that row loaded instead.
  const [controller] = useState(createWaterfallController);
  const loading = useRef<{ parent: string; before: Set<string> } | null>(null);
  useEffect(() => {
    const pending = loading.current;
    const node = pending ? tree.nodes.get(pending.parent) : undefined;
    if (!pending || node?.more === "loading") return;
    loading.current = null;
    if (!node || node.more !== null || document.activeElement !== document.body) return;
    const next = node.children.find((child) => !pending.before.has(child.span.span_id));
    if (next) controller.focus(next.span.span_id);
  });

  // A deep link, or a selection made elsewhere, brings its row into view once.
  const revealed = useRef<string | null>(null);
  useEffect(() => {
    if (selected === null || revealed.current === selected) return;
    const index = rows.findIndex((row) => row.item.value === selected);
    if (index < 0) return;
    revealed.current = selected;
    reveal(index);
  });

  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const observer = new ResizeObserver(() =>
      setView({ width: el.clientWidth, height: el.clientHeight }),
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  return (
    <div
      ref={scroller}
      onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
      className="-mx-2 max-h-[min(70dvh,40rem)] overflow-auto"
    >
      <Waterfall
        aria-label="Spans"
        aria-rowcount={rows.length + 1}
        items={items}
        controller={controller}
        range={range}
        expanded={expanded}
        onExpandedChange={(next) => onExpandedChange(next)}
        value={selected}
        onValueChange={(value, details) => {
          const parent = value === null ? null : moreRowParent(value);
          if (parent !== null) {
            details.cancel();
            const children = tree.nodes.get(parent)?.children ?? [];
            loading.current = { parent, before: new Set(children.map((c) => c.span.span_id)) };
            onMore(parent);
            return;
          }
          revealed.current = value;
          onSelect(value);
        }}
        onHighlight={(_value, { index }) => reveal(index)}
        format={formatMs}
        className="relative block min-w-72 text-data"
        style={{ height: (rows.length + 1) * ROW_HEIGHT }}
      >
        <WaterfallHeaderRow
          aria-rowindex={1}
          className={`${COLUMNS} sticky top-0 z-10 h-8 items-center border-b border-ash-border bg-warm-sand px-2 text-label text-ink-soft`}
        >
          <WaterfallColumnHeader>Span</WaterfallColumnHeader>
          <WaterfallColumnHeader aria-label="Timeline">
            <WaterfallAxis maxTicks={view.width < 480 ? 2 : 4} />
          </WaterfallColumnHeader>
          <WaterfallColumnHeader className="text-end">Duration</WaterfallColumnHeader>
        </WaterfallHeaderRow>
        {rows.slice(first, last).map((row) => (
          <SpanRow key={row.item.value} row={row} tree={tree} />
        ))}
      </Waterfall>
    </div>
  );
}

function SpanRow({ row, tree }: { row: WaterfallDataRow; tree: TraceTree }) {
  const { item, index } = row;
  const style = { top: (index + 1) * ROW_HEIGHT };
  const parent = moreRowParent(item.value);
  if (parent !== null) {
    const state = tree.nodes.get(parent)?.more ?? undefined;
    return (
      <WaterfallRow
        value={item.value}
        aria-rowindex={index + 2}
        aria-busy={state === "loading" || state === "unloaded" ? true : undefined}
        data-more={state}
        style={style}
        className={ROW}
      >
        <WaterfallRowHeader className="col-span-full flex min-w-0 items-center gap-1.5 text-ink-soft">
          <span aria-hidden="true" className="w-4 shrink-0" />
          {state === "error" ? <AlertIcon className="shrink-0" /> : null}
          <span className="truncate">{item.label}</span>
        </WaterfallRowHeader>
      </WaterfallRow>
    );
  }
  const node = tree.nodes.get(item.value);
  if (!node) return null;
  const { span } = node;
  const failed = span.level === "error" || span.status_code === "error";
  return (
    <WaterfallRow value={item.value} aria-rowindex={index + 2} style={style} className={ROW}>
      <WaterfallRowHeader className="flex min-w-0 items-center gap-1.5">
        {item.children ? (
          <WaterfallIndicator className="flex size-4 shrink-0 items-center justify-center text-ink-soft">
            <ChevronRightIcon width="14" height="14" />
          </WaterfallIndicator>
        ) : (
          <span aria-hidden="true" className="w-4 shrink-0" />
        )}
        <span className="shrink-0 text-label text-ink-soft max-sm:hidden">
          {TYPE_LABEL[span.observation_type]}
        </span>
        <span className="truncate">{item.label}</span>
        {failed ? (
          <span className="flex shrink-0 items-center gap-1 text-label">
            <AlertIcon className="shrink-0" />
            Error
          </span>
        ) : null}
      </WaterfallRowHeader>
      <WaterfallBar />
      <WaterfallCell className="text-end tabular-nums">
        {formatDuration(span.duration_us)}
      </WaterfallCell>
    </WaterfallRow>
  );
}
