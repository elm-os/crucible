"use client";

import { useInfiniteQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldLabel } from "@ui-sdk/react/field";
import { Switch, SwitchThumb } from "@ui-sdk/react/switch";
import Link from "next/link";
import { useState } from "react";
import { type McpOperationFilter, type McpScope, listMcpOperations } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { McpOperationPage } from "@/api/generated/McpOperationPage";
import { ErrorCard, Section } from "@/components/section";
import { MetaNotice, StatusLabel } from "@/components/status";
import { formatDuration, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { type Range, rangeWindow, useTimeRange } from "@/lib/time-range";
import { useUrlParam } from "@/lib/url-state";
import { operationStatus } from "./format";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import { emptyAndWhole, FilterChip, McpEmpty, McpIncomplete, Table, TableSkeleton } from "./shared";

const COLUMNS = ["Started", "Method", "Target", "Server", "Client", "Duration", "Status"] as const;

const FILTERS = [
  { key: "target", label: "Target" },
  { key: "client", label: "Client" },
  { key: "session", label: "Session" },
  { key: "user", label: "User" },
] as const;

function useFilter(): {
  filter: McpOperationFilter;
  setErrorsOnly: (on: boolean) => void;
  clear: (key: (typeof FILTERS)[number]["key"]) => void;
} {
  const [target, setTarget] = useUrlParam("target");
  const [client, setClient] = useUrlParam("client");
  const [session, setSession] = useUrlParam("session");
  const [user, setUser] = useUrlParam("user");
  const [errors, setErrors] = useUrlParam("errors");
  const setters = { target: setTarget, client: setClient, session: setSession, user: setUser };
  return {
    filter: {
      target: target || undefined,
      client: client || undefined,
      session: session || undefined,
      user: user || undefined,
      errorsOnly: errors === "1",
    },
    setErrorsOnly: (on) => setErrors(on ? "1" : null),
    clear: (key) => setters[key](null),
  };
}

export function OperationsSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const { filter, setErrorsOnly, clear } = useFilter();
  const active = FILTERS.filter((f) => filter[f.key]);
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <Field className="flex flex-row items-center gap-2">
          <Switch checked={filter.errorsOnly ?? false} onCheckedChange={setErrorsOnly}>
            <SwitchThumb />
          </Switch>
          <FieldLabel className="text-data">Failed calls only</FieldLabel>
        </Field>
        {active.map((f) => (
          <FilterChip
            key={f.key}
            label={f.label}
            value={filter[f.key] ?? ""}
            onClear={() => clear(f.key)}
          />
        ))}
      </div>
      {/* A list's pages share one window, since their cursors are bound to it; any change starts a new list. */}
      <OperationsList
        key={JSON.stringify([project, range, scope, filter])}
        project={project}
        range={range}
        onWiden={() => setRange("30d")}
        scope={scope}
        filter={filter}
      />
    </div>
  );
}

function OperationsList({
  project,
  range,
  onWiden,
  scope,
  filter,
}: {
  project: string;
  range: Range;
  onWiden: () => void;
  scope: McpScope;
  filter: McpOperationFilter;
}) {
  const href = useMcpHref();
  const [timeWindow] = useState(() => rangeWindow(range, Date.now()));
  const query = useInfiniteQuery({
    queryKey: [project, "mcp", "operations", scope, filter, timeWindow.from, timeWindow.to],
    queryFn: ({ pageParam, signal }) =>
      listMcpOperations(project, timeWindow, scope, filter, pageParam, signal),
    initialPageParam: null as string | null,
    getNextPageParam: (last: McpOperationPage) => (last.has_more ? last.next_cursor : null),
  });
  const pages = query.data?.pages ?? [];
  const rows = pages.flatMap((page) => page.data);
  const first = pages[0];
  return (
    <Section
      title="Operations"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={() =>
        rows.length === 0 &&
        !query.hasNextPage &&
        first !== undefined &&
        emptyAndWhole(0, first.meta)
      }
      empty={<McpEmpty noun="operations" range={range} onWiden={onWiden} />}
    >
      {() => (
        <>
          {first ? <MetaNotice meta={first.meta} noun="operations" /> : null}
          {first ? <SourceNote sources={first.capture_sources} /> : null}
          {rows.length === 0 ? (
            <McpIncomplete noun="operations" onRetry={() => query.refetch()} />
          ) : (
            <Table columns={COLUMNS} wide>
              {rows.map((row) => {
                const status = operationStatus(row);
                return (
                  <tr
                    key={`${row.trace_id}/${row.span_id}`}
                    className="h-8 border-t border-ash-border"
                  >
                    <td className="pe-4 whitespace-nowrap">
                      <Link
                        href={href("/mcp/operation/", {
                          trace: row.trace_id,
                          span: row.span_id,
                        })}
                        aria-label={`${row.method} ${row.target_name} at ${formatTime(row.start_time)}`}
                      >
                        {formatTime(row.start_time)}
                      </Link>
                    </td>
                    <td className="pe-4 font-mono whitespace-nowrap">{row.method}</td>
                    <td className="max-w-48 truncate pe-4">{row.target_name || "None"}</td>
                    <td className="max-w-40 truncate pe-4">{row.server_name}</td>
                    <td className="max-w-40 truncate pe-4">{row.client_name || "No name sent"}</td>
                    <td className="pe-4 whitespace-nowrap tabular-nums">
                      {formatDuration(row.duration_us)}
                    </td>
                    <td className="whitespace-nowrap">
                      <StatusLabel status={status.status} label={status.label} />
                    </td>
                  </tr>
                );
              })}
            </Table>
          )}
          {query.isFetchNextPageError ? (
            <ErrorCard
              error={asUiError(query.error)}
              label="more operations"
              onRetry={() => query.fetchNextPage()}
            />
          ) : query.hasNextPage ? (
            <div className="mt-3">
              <Button
                onClick={() => query.fetchNextPage()}
                disabled={query.isFetchingNextPage}
                aria-busy={query.isFetchingNextPage || undefined}
              >
                {query.isFetchingNextPage ? "Loading more operations" : "Load more operations"}
              </Button>
            </div>
          ) : null}
        </>
      )}
    </Section>
  );
}
