"use client";

import { Button } from "@ui-sdk/react/button";
import {
  Pagination,
  PaginationEllipsis,
  PaginationItem,
  PaginationLink,
  PaginationList,
  PaginationNext,
  PaginationPrevious,
} from "@ui-sdk/react/pagination";
import { Progress, ProgressIndicator, ProgressLabel, ProgressTrack } from "@ui-sdk/react/progress";
import type { Route } from "next";
import Link from "next/link";
import { type ReactNode, useState } from "react";
import type { UiError } from "@/api/errors";
import type { SqlResult } from "@/api/generated/SqlResult";
import { EmptyState } from "@/components/empty-state";
import { AlertIcon, UnsetIcon } from "@/components/icons";
import { ConnectTrigger } from "@/components/shell/connect-dialog";
import { formatCount, formatDuration } from "@/lib/format";
import { currentPath, loginHref } from "@/lib/next-path";
import { formatBytes, formatCell, isNumericType, MAX_ROWS, PAGE_ROWS } from "./cells";
import { SqlError } from "./errors";

export type RunState =
  | { status: "idle" }
  | { status: "running"; what: string }
  | { status: "done"; result: SqlResult }
  | { status: "failed"; error: UiError; fromEditor: boolean }
  | { status: "cancelled" };

export function Working({ label, children }: { label: string; children?: ReactNode }) {
  return (
    <div className="flex flex-col items-start gap-2 py-4">
      <Progress value={null} className="w-full max-w-sm">
        <ProgressLabel className="text-data">{label}</ProgressLabel>
        <ProgressTrack>
          <ProgressIndicator />
        </ProgressTrack>
      </Progress>
      {children ? <p className="text-label text-ink-soft">{children}</p> : null}
    </div>
  );
}

// A refusal says what happened and how to fix it; ClickHouse's own words sit beside the fix because
// for a query error they are the precise part.
export function SqlErrorCard({
  error,
  onGoTo,
  children,
}: {
  error: UiError;
  onGoTo?: (line: number, column: number) => void;
  children?: ReactNode;
}) {
  const sql = error instanceof SqlError ? error : null;
  const line = sql?.line ?? null;
  const column = sql?.column ?? 1;
  return (
    <div
      role="alert"
      data-error={error.code ?? error.kind}
      className="flex flex-col items-start gap-2 py-4"
    >
      <p className="flex items-center gap-2 text-body font-medium">
        <AlertIcon className="shrink-0" />
        {error.title}
      </p>
      <p data-fix className="max-w-prose text-data text-ink-soft">
        {error.detail}
      </p>
      {error.code === "sql_quota_exceeded" && error.retryAfterS !== null ? (
        <p data-retry-after className="text-data text-ink-soft">
          You can run it again in {error.retryAfterS} s.
        </p>
      ) : null}
      {sql?.clickhouse ? (
        <div className="w-full">
          <p className="text-label font-medium text-ink-soft">ClickHouse said</p>
          <pre
            data-clickhouse
            className="mt-1 rounded-md border border-ash-border bg-parchment-white px-3 py-2 font-mono text-label break-words whitespace-pre-wrap"
          >
            {sql.clickhouse}
          </pre>
        </div>
      ) : null}
      {line !== null ? (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2 text-data">
          <span data-position>
            Line {line}, column {column}
          </span>
          {onGoTo ? <Button onClick={() => onGoTo(line, column)}>Go to line {line}</Button> : null}
        </div>
      ) : null}
      {error.action === "sign_in" || children || error.requestId !== null ? (
        <div className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-2">
          {error.kind === "signed_out" ? (
            <Link href={loginHref(currentPath()) as Route}>Sign in</Link>
          ) : null}
          {error.kind === "unauthenticated" ? <ConnectTrigger>Connect</ConnectTrigger> : null}
          {children}
          {error.requestId !== null ? (
            <p className="text-label text-ink-soft">
              Reference <span className="font-mono select-all">{error.requestId}</span>
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

export function ResultBody({
  run,
  onGoTo,
}: {
  run: RunState;
  onGoTo: (line: number, column: number) => void;
}) {
  switch (run.status) {
    case "idle":
      return (
        <EmptyState kind="first-use" title="No result yet.">
          Run the query, or run a saved query, to see its rows here.
        </EmptyState>
      );
    case "running":
      return (
        <div data-running>
          <Working label={run.what}>Cancel stops the query on the server.</Working>
        </div>
      );
    case "cancelled":
      return (
        <output data-cancelled className="flex items-center gap-2 py-6 text-body font-medium">
          <UnsetIcon className="shrink-0" />
          The run was cancelled.
        </output>
      );
    case "failed":
      return <SqlErrorCard error={run.error} onGoTo={run.fromEditor ? onGoTo : undefined} />;
    case "done":
      return <ResultGrid key={run.result.run_id} result={run.result} />;
  }
}

function ResultGrid({ result }: { result: SqlResult }) {
  const [page, setPage] = useState(1);
  const rows = result.rows.slice(0, MAX_ROWS);
  const numeric = result.columns.map((column) => isNumericType(column.type));
  const first = (page - 1) * PAGE_ROWS;
  const shown = rows.slice(first, first + PAGE_ROWS);
  const { stats } = result;
  return (
    <div className="flex flex-col gap-3">
      {result.truncated ? (
        <p data-notice="truncated" className="flex items-center gap-1.5 text-label text-ink-soft">
          <AlertIcon className="shrink-0" />
          Showing the first {formatCount(rows.length)} rows. Narrow the query or export it to get
          every row.
        </p>
      ) : null}
      {result.cut_cells > 0 ? (
        <p data-notice="cut" className="flex items-center gap-1.5 text-label text-ink-soft">
          <AlertIcon className="shrink-0" />
          {result.cut_cells === 1
            ? "1 value was too long and was cut."
            : `${formatCount(result.cut_cells)} values were too long and were cut.`}{" "}
          A cut value ends with …[cut]. Export the query to get whole values.
        </p>
      ) : null}
      {rows.length === 0 ? (
        <EmptyState kind="filtered" title="The query returned no rows.">
          Rows may exist outside the selected time range or the query&apos;s filters.
        </EmptyState>
      ) : (
        <>
          <section
            aria-label="Result rows"
            // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
            tabIndex={0}
            className="max-h-[32rem] overflow-auto rounded-md border border-ash-border bg-parchment-white"
          >
            <table className="w-full text-start text-data">
              <thead className="sticky top-0 bg-parchment-white">
                <tr>
                  {result.columns.map((column, index) => (
                    <th
                      key={`${index}:${column.name}`}
                      scope="col"
                      className={`border-b border-ash-border px-3 py-1.5 font-medium whitespace-nowrap ${numeric[index] ? "text-end" : "text-start"}`}
                    >
                      {column.name}
                      <span className="block font-mono text-label font-normal text-ink-soft">
                        {column.type}
                      </span>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {shown.map((row, offset) => (
                  <tr key={first + offset} className="border-t border-ash-border">
                    {result.columns.map((column, index) => (
                      <ResultCell
                        key={`${index}:${column.name}`}
                        value={row[index]}
                        numeric={numeric[index] ?? false}
                      />
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
          {rows.length > PAGE_ROWS ? (
            <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
              <p data-page-range className="text-label text-ink-soft">
                Rows {formatCount(first + 1)} to{" "}
                {formatCount(Math.min(first + PAGE_ROWS, rows.length))} of{" "}
                {formatCount(rows.length)}
              </p>
              <Pagination
                aria-label="Result pages"
                total={rows.length}
                pageSize={PAGE_ROWS}
                page={page}
                onPageChange={setPage}
              >
                <PaginationList>
                  {({ items }) => (
                    <>
                      <PaginationItem>
                        <PaginationPrevious>Previous</PaginationPrevious>
                      </PaginationItem>
                      {items.map((item) => (
                        <PaginationItem key={item}>
                          {typeof item === "number" ? (
                            <PaginationLink page={item} />
                          ) : (
                            <PaginationEllipsis />
                          )}
                        </PaginationItem>
                      ))}
                      <PaginationItem>
                        <PaginationNext>Next</PaginationNext>
                      </PaginationItem>
                    </>
                  )}
                </PaginationList>
              </Pagination>
            </div>
          ) : null}
        </>
      )}
      <p data-stats className="text-label text-ink-soft">
        {formatCount(rows.length)} {rows.length === 1 ? "row" : "rows"} returned. Read{" "}
        {formatCount(stats.rows_read)} rows ({formatBytes(stats.bytes_read)}) in{" "}
        {formatDuration(stats.elapsed_ms * 1_000)}.
      </p>
    </div>
  );
}

function ResultCell({ value, numeric }: { value: unknown; numeric: boolean }) {
  const cell = formatCell(value);
  const mono = numeric || cell.kind === "number" || cell.kind === "json";
  return (
    <td
      title={cell.text.length > 60 ? cell.text : undefined}
      className={`max-w-[28rem] truncate px-3 py-1 align-top whitespace-nowrap ${numeric ? "text-end tabular-nums" : ""} ${mono ? "font-mono text-label" : ""}`}
    >
      {cell.kind === "null" ? (
        <span data-null className="font-sans text-label text-ink-soft italic">
          null
        </span>
      ) : (
        cell.text
      )}
    </td>
  );
}
