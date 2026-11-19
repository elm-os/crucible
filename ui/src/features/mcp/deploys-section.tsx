"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { listMcpDeploys } from "@/api/client";
import type { McpDeployRow } from "@/api/generated/McpDeployRow";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import { emptyAndWhole, McpEmpty, McpIncomplete, Table, TableSkeleton } from "./shared";

const COLUMNS = ["Server", "Deploy", "Version", "First seen", "Last seen", "Compare"] as const;

export function DeploysSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const href = useMcpHref();
  const query = useQuery({
    queryKey: [project, "mcp", "deploys", scope, range],
    queryFn: ({ signal }) => listMcpDeploys(project, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <Section
      title="Deploys"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(list) => emptyAndWhole(list.data.length, list.meta)}
      empty={<McpEmpty noun="deploys" range={range} onWiden={() => setRange("30d")} />}
    >
      {(list) =>
        list.data.length === 0 ? (
          <McpIncomplete noun="deploys" onRetry={() => query.refetch()} />
        ) : (
          <>
            <MetaNotice meta={list.meta} noun="deploys" />
            <SourceNote sources={list.capture_sources} />
            <p className="mb-3 text-label text-ink-soft">
              Deploys are recorded from server-side operations, whichever side is shown.
            </p>
            <Table columns={COLUMNS} label="Deploys table">
              {list.data.map((deploy, at) => {
                const previous = previousDeploy(list.data, at);
                return (
                  <tr
                    key={`${deploy.server_name}/${deploy.deploy_id}/${deploy.server_version}/${deploy.capture_source}`}
                    className="h-8 border-t border-ash-border"
                  >
                    <td className="max-w-48 truncate pe-4">{deploy.server_name}</td>
                    <td className="pe-4 font-mono">{deploy.deploy_id || "Unknown"}</td>
                    <td className="pe-4">{deploy.server_version || "Unknown"}</td>
                    <td className="pe-4 whitespace-nowrap">{formatTime(deploy.first_seen)}</td>
                    <td className="pe-4 whitespace-nowrap">{formatTime(deploy.last_seen)}</td>
                    <td className="whitespace-nowrap">
                      {previous ? (
                        <Link
                          href={href("/mcp/compare/", {
                            server: deploy.server_name,
                            baseline: previous.deploy_id,
                            candidate: deploy.deploy_id,
                          })}
                        >
                          {`Compare with ${previous.deploy_id}`}
                        </Link>
                      ) : (
                        <span className="text-ink-soft">
                          {deploy.deploy_id ? "First in range" : "No deploy id"}
                        </span>
                      )}
                    </td>
                  </tr>
                );
              })}
            </Table>
          </>
        )
      }
    </Section>
  );
}

// The deploy a row compares against: the next older row of the same server with another, known id.
function previousDeploy(rows: McpDeployRow[], at: number): McpDeployRow | undefined {
  const deploy = rows[at];
  if (!deploy?.deploy_id) return undefined;
  return rows
    .slice(at + 1)
    .find(
      (other) =>
        other.server_name === deploy.server_name &&
        other.deploy_id !== "" &&
        other.deploy_id !== deploy.deploy_id,
    );
}
