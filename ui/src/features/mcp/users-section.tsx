"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { listMcpUsers } from "@/api/client";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatCount, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { IDENTITY_LABEL, userLabel } from "./format";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import { emptyAndWhole, McpEmpty, McpIncomplete, Table, TableSkeleton, Truncated } from "./shared";

const COLUMNS = [
  "User",
  "Identified by",
  "Calls",
  "Errors",
  "Sessions",
  "First seen",
  "Last seen",
] as const;

export function UsersSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const href = useMcpHref();
  const query = useQuery({
    queryKey: [project, "mcp", "users", scope, range],
    queryFn: ({ signal }) => listMcpUsers(project, rangeWindow(range, Date.now()), scope, signal),
  });
  return (
    <Section
      title="Users"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(list) => emptyAndWhole(list.data.length, list.meta)}
      empty={
        <McpEmpty
          noun="users"
          range={range}
          onWiden={() => setRange("30d")}
          widest="Users appear when an SDK's identify hook names them or a call carries a bearer token."
        />
      }
    >
      {(list) =>
        list.data.length === 0 ? (
          <McpIncomplete noun="users" onRetry={() => query.refetch()} />
        ) : (
          <>
            <MetaNotice meta={list.meta} noun="users" />
            <SourceNote sources={list.capture_sources} />
            <Table columns={COLUMNS} wide>
              {list.data.map((user) => {
                const key = user.user_id || user.subject_hash || "";
                return (
                  <tr
                    key={`${user.user_id}/${user.subject_hash}`}
                    className="h-8 border-t border-ash-border"
                  >
                    <td className="max-w-60 truncate pe-4">
                      {key ? (
                        <Link href={href("/mcp/operations/", { user: key })}>
                          {userLabel(user.user_id, user.subject_hash)}
                        </Link>
                      ) : (
                        userLabel(user.user_id, user.subject_hash)
                      )}
                    </td>
                    <td className="pe-4 whitespace-nowrap">
                      {IDENTITY_LABEL[user.identity_source]}
                    </td>
                    <td className="pe-4 tabular-nums">{formatCount(user.calls)}</td>
                    <td className="pe-4 tabular-nums">{formatCount(user.errors)}</td>
                    <td className="pe-4 tabular-nums">{formatCount(user.sessions)}</td>
                    <td className="pe-4 whitespace-nowrap">{formatTime(user.first_seen)}</td>
                    <td className="whitespace-nowrap">{formatTime(user.last_seen)}</td>
                  </tr>
                );
              })}
            </Table>
            {list.truncated ? <Truncated shown={list.data.length} noun="users" recent /> : null}
          </>
        )
      }
    </Section>
  );
}
