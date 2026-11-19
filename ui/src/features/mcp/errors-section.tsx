"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { getMcpErrors } from "@/api/client";
import type { McpErrors } from "@/api/generated/McpErrors";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatCount, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { ERROR_LABEL, formatRate } from "./format";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import {
  ClassBars,
  emptyAndWhole,
  McpEmpty,
  McpIncomplete,
  Table,
  TableSkeleton,
  Truncated,
  useView,
  ViewSwitch,
} from "./shared";

const TARGET_COLUMNS = ["Method", "Target", "Error class", "Calls"] as const;
const GROUP_COLUMNS = [
  "Server",
  "Target",
  "Error class",
  "Code",
  "Message",
  "Calls",
  "Last seen",
] as const;

const failures = (errors: McpErrors) =>
  errors.view === "summary"
    ? errors.classes.reduce((sum, c) => sum + c.calls, 0)
    : errors.breakdown.length;

export function ErrorsSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const [view] = useView();
  const query = useQuery({
    queryKey: [project, "mcp", "errors", view, scope, range],
    queryFn: ({ signal }) =>
      getMcpErrors(project, view, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <div className="flex flex-col">
      <ViewSwitch label="Errors view" breakdown="By message" />
      <Section
        title={view === "summary" ? "Error classes" : "Failed calls by message"}
        query={query}
        skeleton={<TableSkeleton />}
        isEmpty={(errors) => emptyAndWhole(failures(errors), errors.meta)}
        empty={
          <McpEmpty
            noun="errors"
            range={range}
            onWiden={() => setRange("30d")}
            widest="Every call in this range succeeded. Calls the caller caused count here only with Include caller faults on."
          />
        }
      >
        {(errors) =>
          failures(errors) === 0 ? (
            <McpIncomplete noun="errors" onRetry={() => query.refetch()} />
          ) : (
            <>
              <MetaNotice meta={errors.meta} noun="errors" />
              <SourceNote sources={errors.capture_sources} />
              {errors.view === "summary" ? (
                <Summary errors={errors} />
              ) : (
                <Breakdown errors={errors} />
              )}
            </>
          )
        }
      </Section>
    </div>
  );
}

function Summary({ errors }: { errors: McpErrors }) {
  const href = useMcpHref();
  return (
    <div className="flex flex-col gap-6">
      {errors.stats ? (
        <p className="text-data">
          {`${formatCount(errors.stats.errors)} of ${formatCount(errors.stats.calls)} calls failed (${formatRate(errors.stats.error_rate)}). `}
          {`${formatCount(errors.stats.caller_faults)} more were caller faults.`}
        </p>
      ) : null}
      <ClassBars classes={errors.classes} />
      <Table columns={TARGET_COLUMNS}>
        {errors.targets.map((row) => (
          <tr
            key={`${row.method}/${row.target_name}/${row.error_class}`}
            className="h-8 border-t border-ash-border"
          >
            <td className="pe-4 font-mono">{row.method}</td>
            <td className="max-w-60 truncate pe-4">
              {row.target_name ? (
                <Link href={href("/mcp/operations/", { target: row.target_name, errors: "1" })}>
                  {row.target_name}
                </Link>
              ) : (
                <span className="text-ink-soft">None</span>
              )}
            </td>
            <td className="pe-4">{ERROR_LABEL[row.error_class]}</td>
            <td className="tabular-nums">{formatCount(row.calls)}</td>
          </tr>
        ))}
      </Table>
    </div>
  );
}

function Breakdown({ errors }: { errors: McpErrors }) {
  return (
    <>
      <Table columns={GROUP_COLUMNS} wide label="Failed calls table">
        {errors.breakdown.map((row) => (
          <tr
            key={`${row.server_name}/${row.method}/${row.target_name}/${row.error_class}/${row.error_subclass}/${row.jsonrpc_code}/${row.error_message}`}
            className="h-8 border-t border-ash-border"
          >
            <td className="max-w-40 truncate pe-4">{row.server_name}</td>
            <td className="max-w-48 truncate pe-4">{row.target_name || row.method}</td>
            <td className="pe-4 whitespace-nowrap">{ERROR_LABEL[row.error_class]}</td>
            <td className="pe-4 tabular-nums">{row.jsonrpc_code ?? "None"}</td>
            <td className="max-w-80 truncate pe-4" title={row.error_message}>
              {row.error_message || "No message"}
            </td>
            <td className="pe-4 tabular-nums">{formatCount(row.calls)}</td>
            <td className="whitespace-nowrap">{formatTime(row.last_seen)}</td>
          </tr>
        ))}
      </Table>
      {errors.truncated ? <Truncated shown={errors.breakdown.length} noun="groups" /> : null}
    </>
  );
}
