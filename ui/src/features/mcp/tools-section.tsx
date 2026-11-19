"use client";

import { useQuery } from "@tanstack/react-query";
import { Tabs, TabsList, TabsPanel, TabsTab } from "@ui-sdk/react/tabs";
import Link from "next/link";
import { listMcpTools } from "@/api/client";
import type { McpToolRow } from "@/api/generated/McpToolRow";
import { EmptyState } from "@/components/empty-state";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatCount } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import {
  emptyAndWhole,
  McpEmpty,
  McpIncomplete,
  STATS_COLUMNS,
  StatsCells,
  Table,
  TableSkeleton,
} from "./shared";

const COLUMNS = ["Tool", "Server", ...STATS_COLUMNS, "Response bytes"] as const;
const UNUSED_COLUMNS = ["Tool", "Server", "Schema hash"] as const;

export function ToolsSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const query = useQuery({
    queryKey: [project, "mcp", "tools", scope, range],
    queryFn: ({ signal }) => listMcpTools(project, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <Section
      title="Tools"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(list) => emptyAndWhole(list.data.length, list.meta)}
      empty={<McpEmpty noun="tools" range={range} onWiden={() => setRange("30d")} />}
    >
      {(list) =>
        list.data.length === 0 ? (
          <McpIncomplete noun="tools" onRetry={() => query.refetch()} />
        ) : (
          <>
            <MetaNotice meta={list.meta} noun="tools" />
            <SourceNote sources={list.capture_sources} />
            <ToolTabs rows={list.data} />
          </>
        )
      }
    </Section>
  );
}

function ToolTabs({ rows }: { rows: McpToolRow[] }) {
  const called = rows.filter((row) => !row.unused);
  const unused = rows.filter((row) => row.unused);
  const href = useMcpHref();
  return (
    <Tabs defaultValue="called" className="flex flex-col gap-3">
      <TabsList aria-label="Tools" className="flex gap-1 text-data">
        <TabsTab value="called">Called ({called.length})</TabsTab>
        <TabsTab value="unused">Unused ({unused.length})</TabsTab>
      </TabsList>
      <TabsPanel value="called">
        {called.length === 0 ? (
          <EmptyState kind="filtered" title="No tool was called in this range.">
            The listed tools are under Unused.
          </EmptyState>
        ) : (
          <Table columns={COLUMNS} wide>
            {called.map((tool) => (
              <tr
                key={`${tool.server_name}/${tool.tool_name}`}
                className="h-8 border-t border-ash-border"
              >
                <td className="max-w-60 truncate pe-4">
                  <Link
                    href={href("/mcp/tool/", { server: tool.server_name, name: tool.tool_name })}
                  >
                    {tool.tool_name}
                  </Link>
                </td>
                <td className="max-w-48 truncate pe-4">{tool.server_name}</td>
                <StatsCells stats={tool.stats} />
                <td className="tabular-nums">{formatCount(tool.response_bytes)}</td>
              </tr>
            ))}
          </Table>
        )}
      </TabsPanel>
      <TabsPanel value="unused">
        {unused.length === 0 ? (
          <EmptyState kind="filtered" title="Every listed tool was called in this range." />
        ) : (
          <Table columns={UNUSED_COLUMNS} label="Unused tools">
            {unused.map((tool) => (
              <tr
                key={`${tool.server_name}/${tool.tool_name}`}
                className="h-8 border-t border-ash-border"
              >
                <td className="max-w-60 truncate pe-4">{tool.tool_name}</td>
                <td className="max-w-48 truncate pe-4">{tool.server_name}</td>
                <td className="font-mono">{tool.schema_hash ?? "Not listed"}</td>
              </tr>
            ))}
          </Table>
        )}
      </TabsPanel>
    </Tabs>
  );
}
