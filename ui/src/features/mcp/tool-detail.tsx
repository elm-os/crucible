"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { getMcpTool } from "@/api/client";
import type { McpToolDetail } from "@/api/generated/McpToolDetail";
import { EmptyState } from "@/components/empty-state";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { TimeSeriesChart } from "@/components/time-series-chart";
import type { ChartSeries } from "@/primitives/chart/chart-model";
import { formatCount, formatDuration, parseMicros } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { useUrlParam } from "@/lib/url-state";
import { formatLatency, formatRate } from "./format";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import { ClassBars, emptyAndWhole, McpEmpty, Table, TableSkeleton } from "./shared";

function ms(iso: string): number {
  return Math.floor((parseMicros(iso) ?? 0) / 1_000);
}

function useTool() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const [name] = useUrlParam("name");
  const query = useQuery({
    queryKey: [project, "mcp", "tool", name, scope, range],
    queryFn: ({ signal }) =>
      getMcpTool(project, name ?? "", rangeWindow(range, Date.now()), scope, signal),
    enabled: Boolean(name),
  });
  return { name, query, range, setRange };
}

const noCalls = (tool: McpToolDetail) =>
  emptyAndWhole(tool.stats.calls + tool.stats.caller_faults, tool.meta);

// The tool's summary, its calls and latency over time with deploy markers, its error classes and its
// top clients: one request, shown in independent frames.
export function ToolDetail() {
  const { name, query, range, setRange } = useTool();
  const href = useMcpHref();
  if (!name) {
    return (
      <EmptyState kind="filtered" title="No tool is selected.">
        Open a tool from the <Link href={href("/mcp/tools/")}>tools list</Link>.
      </EmptyState>
    );
  }
  const empty = (
    <McpEmpty
      noun="calls of this tool"
      range={range}
      onWiden={() => setRange("30d")}
      widest="The tool may be listed and never called, or called only on the other side."
    />
  );
  return (
    <div className="flex flex-col gap-4">
      <Section
        title="Summary"
        query={query}
        skeleton={<TableSkeleton />}
        isEmpty={noCalls}
        empty={empty}
      >
        {(tool) => (
          <>
            <MetaNotice meta={tool.meta} noun="calls" />
            <SourceNote sources={tool.capture_sources} />
            <dl className="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-3 lg:grid-cols-6">
              <Tile label="Calls" value={formatCount(tool.stats.calls)} />
              <Tile label="Error rate" value={formatRate(tool.stats.error_rate)} />
              <Tile label="Caller faults" value={formatCount(tool.stats.caller_faults)} />
              <Tile label="p50" value={formatLatency(tool.stats.latency.p50_us)} />
              <Tile label="p90" value={formatLatency(tool.stats.latency.p90_us)} />
              <Tile label="p99" value={formatLatency(tool.stats.latency.p99_us)} />
            </dl>
            <p className="mt-3 text-label">
              <Link href={href("/mcp/operations/", { target: name })}>
                Recent operations of this tool
              </Link>
            </p>
          </>
        )}
      </Section>
      <Section
        title="Calls over time"
        query={query}
        skeleton={<div className="skeleton h-48 w-full" />}
        isEmpty={noCalls}
        empty={empty}
      >
        {(tool) => (
          <TimeSeriesChart
            title="Calls over time"
            form="bar"
            format={(v) => formatCount(Math.round(v))}
            series={callSeries(tool)}
            markers={markers(tool)}
          />
        )}
      </Section>
      <Section
        title="Latency percentiles"
        query={query}
        skeleton={<div className="skeleton h-48 w-full" />}
        isEmpty={noCalls}
        empty={empty}
      >
        {(tool) => (
          <TimeSeriesChart
            title="Latency percentiles"
            family
            format={(v) => formatDuration(v)}
            series={latencySeries(tool)}
            markers={markers(tool)}
          />
        )}
      </Section>
      <div className="grid gap-4 lg:grid-cols-2">
        <Section
          title="Error classes"
          query={query}
          skeleton={<TableSkeleton />}
          isEmpty={(tool) => tool.error_classes.length === 0 && emptyAndWhole(0, tool.meta)}
          empty={<EmptyState kind="filtered" title="No errors in this range." />}
        >
          {(tool) => <ClassBars classes={tool.error_classes} />}
        </Section>
        <Section
          title="Top clients"
          query={query}
          skeleton={<TableSkeleton />}
          isEmpty={(tool) => emptyAndWhole(tool.clients.length, tool.meta)}
          empty={<EmptyState kind="filtered" title="No client called this tool in this range." />}
        >
          {(tool) => (
            <>
              <p className="mb-2 text-label text-ink-soft">Client names are self-reported.</p>
              <Table columns={["Client", "Calls", "Error rate", "p50"]}>
                {tool.clients.map((client) => (
                  <tr key={client.client_name} className="h-8 border-t border-ash-border">
                    <td className="max-w-48 truncate pe-4">
                      {client.client_name ? (
                        <Link
                          href={href("/mcp/operations/", {
                            target: name,
                            client: client.client_name,
                          })}
                        >
                          {client.client_name}
                        </Link>
                      ) : (
                        "No name sent"
                      )}
                    </td>
                    <td className="pe-4 tabular-nums">{formatCount(client.stats.calls)}</td>
                    <td className="pe-4 tabular-nums">{formatRate(client.stats.error_rate)}</td>
                    <td className="tabular-nums">{formatLatency(client.stats.latency.p50_us)}</td>
                  </tr>
                ))}
              </Table>
            </>
          )}
        </Section>
      </div>
    </div>
  );
}

function Tile({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col gap-0.5">
      <dt className="text-label text-ink-soft">{label}</dt>
      <dd className="text-body font-medium tabular-nums">{value}</dd>
    </div>
  );
}

function callSeries(tool: McpToolDetail): ChartSeries[] {
  return [
    {
      id: "calls",
      label: "Calls",
      points: tool.series.map((p) => ({ t: ms(p.bucket), v: p.calls })),
    },
    {
      id: "errors",
      label: "Errors",
      points: tool.series.map((p) => ({ t: ms(p.bucket), v: p.errors })),
    },
  ];
}

function latencySeries(tool: McpToolDetail): ChartSeries[] {
  const of = (pick: (p: McpToolDetail["series"][number]) => number | null) =>
    tool.series.map((p) => ({ t: ms(p.bucket), v: pick(p) }));
  return [
    { id: "p50", label: "p50", points: of((p) => p.latency.p50_us) },
    { id: "p90", label: "p90", points: of((p) => p.latency.p90_us) },
    { id: "p99", label: "p99", points: of((p) => p.latency.p99_us) },
  ];
}

function markers(tool: McpToolDetail): { t: number; label: string }[] {
  return tool.deploys.map((d) => ({
    t: ms(d.first_seen),
    label: `Deploy ${d.deploy_id || "unknown"} of ${d.server_name}`,
  }));
}
