"use client";

import type { UseQueryResult } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { SqlRun } from "@/api/generated/SqlRun";
import type { SqlRunList } from "@/api/generated/SqlRunList";
import type { SqlRunOutcome } from "@/api/generated/SqlRunOutcome";
import { EmptyState } from "@/components/empty-state";
import { AlertIcon, CheckIcon, TodoIcon, UnsetIcon } from "@/components/icons";
import { Section } from "@/components/section";
import { formatCount, formatDuration, formatTime } from "@/lib/format";
import { firstLine } from "./cells";

const OUTCOMES: Record<SqlRunOutcome, { label: string; Icon: typeof CheckIcon }> = {
  succeeded: { label: "Succeeded", Icon: CheckIcon },
  failed: { label: "Failed", Icon: AlertIcon },
  cancelled: { label: "Cancelled", Icon: UnsetIcon },
  running: { label: "Running", Icon: TodoIcon },
};

const KINDS: Record<SqlRun["kind"], string> = {
  ad_hoc: "Ad hoc",
  saved: "Saved query",
  export: "Export",
};

const COLUMNS = ["Started", "Outcome", "Kind", "Rows", "Elapsed", "Query", "Load"] as const;
const SKELETON_ROWS = ["a", "b", "c", "d", "e"] as const;

export function RunHistory({
  query,
  onLoad,
}: {
  query: UseQueryResult<SqlRunList>;
  onLoad: (run: SqlRun) => void;
}) {
  return (
    <Section
      title="History"
      query={query}
      skeleton={
        <div>
          <div className="h-8" />
          {SKELETON_ROWS.map((row) => (
            <div key={row} className="flex h-8 items-center gap-4 border-t border-ash-border">
              <div className="skeleton h-3 w-1/6" />
              <div className="skeleton h-3 w-1/12" />
              <div className="skeleton h-3 w-2/5" />
            </div>
          ))}
        </div>
      }
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No runs yet.">
          Each query you run or export is listed here, newest first.
        </EmptyState>
      }
    >
      {(list) => (
        <div className="relative overflow-x-auto">
          <table className="w-full min-w-[44rem] text-start text-data">
            <thead>
              <tr className="text-label text-ink-soft">
                {COLUMNS.map((column) => (
                  <th key={column} scope="col" className="h-8 pe-4 text-start font-medium">
                    {column === "Load" ? <span className="sr-only">Load</span> : column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {list.data.map((run) => (
                <tr key={run.id} className="border-t border-ash-border">
                  <td className="py-1 pe-4 whitespace-nowrap">{formatTime(run.started_at)}</td>
                  <td className="py-1 pe-4 whitespace-nowrap">
                    <Outcome outcome={run.outcome} />
                  </td>
                  <td className="py-1 pe-4 whitespace-nowrap">{KINDS[run.kind]}</td>
                  <td className="py-1 pe-4 whitespace-nowrap tabular-nums">
                    {formatCount(run.result_rows)}
                    {run.truncated ? " (cut)" : ""}
                  </td>
                  <td className="py-1 pe-4 whitespace-nowrap tabular-nums">
                    {formatDuration(run.elapsed_ms * 1_000)}
                  </td>
                  <td className="max-w-80 truncate py-1 pe-4 font-mono text-label">
                    {firstLine(run.sql)}
                  </td>
                  <td className="py-1 whitespace-nowrap">
                    <Button onClick={() => onLoad(run)}>
                      Load into editor
                      <span className="sr-only"> the run of {formatTime(run.started_at)}</span>
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  );
}

function Outcome({ outcome }: { outcome: SqlRunOutcome }) {
  const { label, Icon } = OUTCOMES[outcome] ?? OUTCOMES.failed;
  return (
    <span data-outcome={outcome} className="flex items-center gap-1.5">
      <Icon className="shrink-0" />
      {label}
    </span>
  );
}
