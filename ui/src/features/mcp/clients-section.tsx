"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { getMcpClients } from "@/api/client";
import type { McpClientList } from "@/api/generated/McpClientList";
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
  Truncated,
  useView,
  ViewSwitch,
} from "./shared";

const COLUMNS = ["Client", ...STATS_COLUMNS] as const;
const VERSION_COLUMNS = ["Client", "Version", "Calls", "Last seen"] as const;

const rows = (list: McpClientList) =>
  list.view === "summary" ? list.data.length : list.versions.length;

export function ClientsSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const [view] = useView();
  const href = useMcpHref();
  const query = useQuery({
    queryKey: [project, "mcp", "clients", view, scope, range],
    queryFn: ({ signal }) =>
      getMcpClients(project, view, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <div className="flex flex-col">
      <ViewSwitch label="Clients view" breakdown="By version" />
      <Section
        title={view === "summary" ? "Clients" : "Client versions"}
        query={query}
        skeleton={<TableSkeleton />}
        isEmpty={(list) => emptyAndWhole(rows(list), list.meta)}
        empty={<McpEmpty noun="clients" range={range} onWiden={() => setRange("30d")} />}
      >
        {(list) =>
          rows(list) === 0 ? (
            <McpIncomplete noun="clients" onRetry={() => query.refetch()} />
          ) : (
            <>
              <MetaNotice meta={list.meta} noun="clients" />
              <SourceNote sources={list.capture_sources} />
              <p data-self-reported className="mb-3 text-label text-ink-soft">
                Self-reported: each client sends its own name and version, and nothing checks them.
              </p>
              {list.view === "summary" ? (
                <Table columns={COLUMNS} wide>
                  {list.data.map((client) => (
                    <tr key={client.client_name} className="h-8 border-t border-ash-border">
                      <td className="max-w-60 truncate pe-4">
                        {client.client_name ? (
                          <Link href={href("/mcp/operations/", { client: client.client_name })}>
                            {client.client_name}
                          </Link>
                        ) : (
                          <span className="text-ink-soft">No name sent</span>
                        )}
                      </td>
                      <StatsCells stats={client.stats} />
                    </tr>
                  ))}
                </Table>
              ) : (
                <>
                  <Table columns={VERSION_COLUMNS} label="Client versions table">
                    {list.versions.map((row) => (
                      <tr
                        key={`${row.client_name}/${row.client_version}`}
                        className="h-8 border-t border-ash-border"
                      >
                        <td className="max-w-60 truncate pe-4">
                          {row.client_name || "No name sent"}
                        </td>
                        <td className="pe-4">{row.client_version || "No version sent"}</td>
                        <td className="pe-4 tabular-nums">{formatCount(row.calls)}</td>
                        <td className="whitespace-nowrap">{formatTime(row.last_seen)}</td>
                      </tr>
                    ))}
                  </Table>
                  {list.truncated ? (
                    <Truncated shown={list.versions.length} noun="versions" />
                  ) : null}
                </>
              )}
            </>
          )
        }
      </Section>
    </div>
  );
}
