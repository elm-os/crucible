"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { listMcpServers } from "@/api/client";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatCount, formatTime } from "@/lib/format";
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

const COLUMNS = ["Server", "Version", ...STATS_COLUMNS, "Clients", "Last seen"] as const;

export function ServersSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const href = useMcpHref();
  const query = useQuery({
    queryKey: [project, "mcp", "servers", scope, range],
    queryFn: ({ signal }) => listMcpServers(project, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <Section
      title="Servers"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(list) => emptyAndWhole(list.data.length, list.meta)}
      empty={<McpEmpty noun="servers" range={range} onWiden={() => setRange("30d")} />}
    >
      {(list) =>
        list.data.length === 0 ? (
          <McpIncomplete noun="servers" onRetry={() => query.refetch()} />
        ) : (
          <>
            <MetaNotice meta={list.meta} noun="servers" />
            <SourceNote sources={list.capture_sources} />
            <Table columns={COLUMNS} wide>
              {list.data.map((server) => (
                <tr key={server.server_name} className="h-8 border-t border-ash-border">
                  <td className="max-w-60 truncate pe-4">
                    <Link href={href("/mcp/tools/", { server: server.server_name })}>
                      {server.server_name || "Unnamed server"}
                    </Link>
                  </td>
                  <td className="pe-4 whitespace-nowrap">{server.version ?? "Unknown"}</td>
                  <StatsCells stats={server.stats} />
                  <td className="pe-4 tabular-nums">{formatCount(server.clients)}</td>
                  <td className="whitespace-nowrap">
                    {server.stats.last_seen ? formatTime(server.stats.last_seen) : "Not seen"}
                  </td>
                </tr>
              ))}
            </Table>
          </>
        )
      }
    </Section>
  );
}
