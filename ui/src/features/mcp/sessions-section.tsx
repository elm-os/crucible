"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { listMcpSessions } from "@/api/client";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatCount, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { GROUP_KEY_LABEL } from "./format";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import { emptyAndWhole, McpEmpty, McpIncomplete, Table, TableSkeleton, Truncated } from "./shared";

const COLUMNS = [
  "Session",
  "Grouped by",
  "Client",
  "Calls",
  "Errors",
  "First seen",
  "Last seen",
] as const;

export function SessionsSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const href = useMcpHref();
  const query = useQuery({
    queryKey: [project, "mcp", "sessions", scope, range],
    queryFn: ({ signal }) =>
      listMcpSessions(project, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <Section
      title="Sessions"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(list) => emptyAndWhole(list.data.length, list.meta)}
      empty={<McpEmpty noun="sessions" range={range} onWiden={() => setRange("30d")} />}
    >
      {(list) =>
        list.data.length === 0 ? (
          <McpIncomplete noun="sessions" onRetry={() => query.refetch()} />
        ) : (
          <>
            <MetaNotice meta={list.meta} noun="sessions" />
            <SourceNote sources={list.capture_sources} />
            <Table columns={COLUMNS} wide>
              {list.data.map((session) => (
                <tr key={session.group_key} className="h-8 border-t border-ash-border">
                  <td className="max-w-60 truncate pe-4 font-mono">
                    <Link href={href("/mcp/operations/", { session: session.group_key })}>
                      {session.group_key}
                    </Link>
                  </td>
                  <td className="pe-4 whitespace-nowrap">
                    {GROUP_KEY_LABEL[session.group_key_kind]}
                  </td>
                  <td className="max-w-48 truncate pe-4">
                    {session.client_name || "No name sent"}
                  </td>
                  <td className="pe-4 tabular-nums">{formatCount(session.calls)}</td>
                  <td className="pe-4 tabular-nums">{formatCount(session.errors)}</td>
                  <td className="pe-4 whitespace-nowrap">{formatTime(session.first_seen)}</td>
                  <td className="whitespace-nowrap">{formatTime(session.last_seen)}</td>
                </tr>
              ))}
            </Table>
            {list.truncated ? <Truncated shown={list.data.length} noun="sessions" recent /> : null}
          </>
        )
      }
    </Section>
  );
}
