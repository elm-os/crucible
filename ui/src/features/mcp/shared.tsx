"use client";

import { Button } from "@ui-sdk/react/button";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import type { ReactNode } from "react";
import type { McpClassCount } from "@/api/generated/McpClassCount";
import type { McpStats } from "@/api/generated/McpStats";
import type { McpView } from "@/api/generated/McpView";
import type { Meta } from "@/api/generated/Meta";
import { EmptyState } from "@/components/empty-state";
import { isWhole } from "@/components/status";
import { formatCount } from "@/lib/format";
import { RANGE_PHRASE, type Range } from "@/lib/time-range";
import { useUrlParam } from "@/lib/url-state";
import { ERROR_LABEL, formatLatency, formatRate } from "./format";

// An answer counts as empty only when it is whole; otherwise rows may exist that were not read.
export function emptyAndWhole(rows: number, meta: Meta): boolean {
  return rows === 0 && isWhole(meta);
}

// Nothing in the range: offer the widest one; at the widest, `widest` explains, or the page invites the
// first MCP data.
export function McpEmpty({
  noun,
  range,
  onWiden,
  widest,
}: {
  noun: string;
  range: Range;
  onWiden: () => void;
  widest?: ReactNode;
}) {
  if (range === "30d" && widest) {
    return (
      <EmptyState kind="filtered" title={`No MCP ${noun} in the last 30 days.`}>
        {widest}
      </EmptyState>
    );
  }
  if (range === "30d") {
    return (
      <EmptyState kind="first-use" title={`No MCP ${noun} in the last 30 days.`}>
        Wrap your MCP server with a Crucible SDK, or run it behind the Crucible proxy, and its calls
        will appear here. A server that already sends OpenTelemetry spans with the MCP attributes
        appears too.
      </EmptyState>
    );
  }
  return (
    <EmptyState
      kind="filtered"
      title={`No MCP ${noun} in ${RANGE_PHRASE[range]}.`}
      action={<Button onClick={onWiden}>Show last 30 days</Button>}
    >
      Older calls may be outside this time range, or on the other side.
    </EmptyState>
  );
}

export function McpIncomplete({ noun, onRetry }: { noun: string; onRetry: () => void }) {
  return (
    <EmptyState
      kind="incomplete"
      title={`No MCP ${noun} to show yet.`}
      action={<Button onClick={onRetry}>Check again</Button>}
    >
      Calls may still be arriving, or part of the data couldn&apos;t be read.
    </EmptyState>
  );
}

const SKELETON_ROWS = Array.from({ length: 8 }, (_, row) => `skeleton-row-${row}`);

export function TableSkeleton() {
  return (
    <div>
      <div className="h-8" />
      {SKELETON_ROWS.map((id) => (
        <div key={id} className="flex h-8 items-center gap-4 border-t border-ash-border">
          <div className="skeleton h-3 w-1/5" />
          <div className="skeleton h-3 w-1/6" />
          <div className="skeleton h-3 w-1/12" />
          <div className="skeleton h-3 w-1/12" />
        </div>
      ))}
    </div>
  );
}

// A table that scrolls sideways inside its frame. One whose rows hold no links takes a `label`, which makes
// the scroller a named region that the keyboard can reach and scroll.
export function Table({
  columns,
  children,
  wide = false,
  label,
}: {
  columns: readonly string[];
  children: ReactNode;
  wide?: boolean;
  label?: string;
}) {
  return (
    <div
      role={label ? "region" : undefined}
      aria-label={label}
      // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
      tabIndex={label ? 0 : undefined}
      className="relative overflow-x-auto"
    >
      <table className={`w-full text-start text-data ${minWidth(columns.length, wide)}`}>
        <thead>
          <tr className="text-label text-ink-soft">
            {columns.map((column) => (
              <th key={column} scope="col" className="h-8 pe-4 text-start font-medium">
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}

// The calls, error rate, caller faults and latency of a row, as table cells.
export function StatsCells({ stats }: { stats: McpStats }) {
  return (
    <>
      <td className="pe-4 tabular-nums">{formatCount(stats.calls)}</td>
      <td className="pe-4 whitespace-nowrap tabular-nums">{formatRate(stats.error_rate)}</td>
      <td className="pe-4 tabular-nums">{formatCount(stats.caller_faults)}</td>
      <td className="pe-4 whitespace-nowrap tabular-nums">{formatLatency(stats.latency.p50_us)}</td>
      <td className="pe-4 whitespace-nowrap tabular-nums">{formatLatency(stats.latency.p90_us)}</td>
      <td className="pe-4 whitespace-nowrap tabular-nums">{formatLatency(stats.latency.p99_us)}</td>
    </>
  );
}

export const STATS_COLUMNS = ["Calls", "Error rate", "Caller faults", "p50", "p90", "p99"] as const;

// Summary or breakdown, kept in `view=`.
export function useView(): [McpView, (next: McpView) => void] {
  const [view, setView] = useUrlParam("view");
  return [
    view === "breakdown" ? "breakdown" : "summary",
    (next) => setView(next === "summary" ? null : next),
  ];
}

export function ViewSwitch({ label, breakdown }: { label: string; breakdown: string }) {
  const [view, setView] = useView();
  return (
    <SegmentedControl
      aria-label={label}
      value={view}
      onValueChange={(next) => setView(next === "breakdown" ? "breakdown" : "summary")}
      className="mb-4 self-start"
    >
      <SegmentedControlIndicator />
      <SegmentedControlItem value="summary" className="text-data">
        Summary
      </SegmentedControlItem>
      <SegmentedControlItem value="breakdown" className="text-data">
        {breakdown}
      </SegmentedControlItem>
    </SegmentedControl>
  );
}

// Calls per error class as bars of one ink, each with its count in words beside it.
export function ClassBars({ classes }: { classes: McpClassCount[] }) {
  const most = Math.max(1, ...classes.map((c) => c.calls));
  return (
    <ul aria-label="Calls per error class" className="flex flex-col gap-2">
      {classes.map((c) => (
        <li
          key={c.error_class}
          className="grid grid-cols-[9rem_1fr_4rem] items-center gap-3 text-data"
        >
          <span>{ERROR_LABEL[c.error_class]}</span>
          <svg
            viewBox="0 0 100 8"
            preserveAspectRatio="none"
            className="h-2 w-full"
            aria-hidden="true"
          >
            <rect width={(c.calls / most) * 100} height="8" fill="var(--color-series-1)" />
          </svg>
          <span className="text-end tabular-nums">{formatCount(c.calls)}</span>
        </li>
      ))}
    </ul>
  );
}

// Says which rows a capped list kept: those with the most calls, or those seen most recently.
export function Truncated({
  shown,
  noun,
  recent = false,
}: {
  shown: number;
  noun: string;
  recent?: boolean;
}) {
  const kept = recent
    ? `the ${formatCount(shown)} ${noun} seen most recently`
    : `the ${formatCount(shown)} ${noun} with the most calls`;
  return (
    <p className="mt-3 text-label text-ink-soft">
      {`Showing ${kept}. Narrow the time range or the server to see the rest.`}
    </p>
  );
}

// Four short columns fit a phone; more keep their width and scroll inside the frame.
function minWidth(columns: number, wide: boolean): string {
  if (columns <= 4) return "";
  return wide ? "min-w-[60rem]" : "min-w-[42rem]";
}
