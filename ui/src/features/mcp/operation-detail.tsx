"use client";

import { useQuery } from "@tanstack/react-query";
import type { Route } from "next";
import Link from "next/link";
import type { ReactNode } from "react";
import { getMcpOperation } from "@/api/client";
import type { McpContentPreview } from "@/api/generated/McpContentPreview";
import type { McpOperation } from "@/api/generated/McpOperation";
import type { McpOperationRow } from "@/api/generated/McpOperationRow";
import { EmptyState } from "@/components/empty-state";
import { Section } from "@/components/section";
import { isWhole, MetaNotice, StatusLabel } from "@/components/status";
import { formatBytes } from "@/features/payload/payload-viewer";
import { formatDuration, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { useUrlParam } from "@/lib/url-state";
import {
  ERROR_LABEL,
  GROUP_KEY_LABEL,
  IDENTITY_LABEL,
  operationStatus,
  SOURCE_LABEL,
  TRANSPORT_LABEL,
  userLabel,
} from "./format";
import { useMcpHref, useMcpScope } from "./scope";
import { TableSkeleton } from "./shared";

// One operation: what it was, who called it, its captured content, the other side's row of the same
// call, and the multi-round exchange it belongs to.
export function OperationDetail() {
  const project = useProject();
  const [range] = useTimeRange(project);
  const [scope] = useMcpScope();
  const href = useMcpHref();
  const [trace] = useUrlParam("trace");
  const [span] = useUrlParam("span");
  const query = useQuery({
    queryKey: [project, "mcp", "operation", trace, span, scope, range],
    queryFn: ({ signal }) =>
      getMcpOperation(
        project,
        trace ?? "",
        span ?? "",
        rangeWindow(range, Date.now()),
        scope,
        signal,
      ),
    enabled: Boolean(trace && span),
  });
  if (!trace || !span) {
    return (
      <EmptyState kind="filtered" title="No operation is selected.">
        Open an operation from the <Link href={href("/mcp/operations/")}>operations list</Link>.
      </EmptyState>
    );
  }
  return (
    <Section
      title="Operation"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(detail) => detail.operation === null && isWhole(detail.meta)}
      empty={
        <EmptyState kind="filtered" title="This operation isn't in the selected time range.">
          It may be older, or on the other side. Widen the time range or switch the side.
        </EmptyState>
      }
    >
      {(detail) =>
        detail.operation === null ? (
          <EmptyState kind="incomplete" title="This operation isn't available yet.">
            It may still be arriving, or part of the data couldn&apos;t be read.
          </EmptyState>
        ) : (
          <>
            <MetaNotice meta={detail.meta} noun="details" />
            <Body project={project} op={detail.operation} />
          </>
        )
      }
    </Section>
  );
}

function traceHref(project: string, row: McpOperationRow): Route {
  const params = new URLSearchParams({ project, trace: row.trace_id, span: row.span_id });
  return `/trace/?${params}` as Route;
}

function Body({ project, op }: { project: string; op: McpOperation }) {
  const row = op.operation;
  const status = operationStatus(row);
  const href = useMcpHref();
  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1">
        <h2 className="text-body font-medium break-all">
          <span className="font-mono">{row.method}</span>
          {row.target_name ? ` ${row.target_name}` : null}
        </h2>
        <StatusLabel status={status.status} label={status.label} />
        <Link href={traceHref(project, row)} className="text-data">
          Open in the trace
        </Link>
      </div>
      <dl className="grid grid-cols-1 gap-x-6 gap-y-3 text-data sm:grid-cols-2 lg:grid-cols-3">
        <Field label="Started">{formatTime(row.start_time)}</Field>
        <Field label="Duration">{formatDuration(row.duration_us)}</Field>
        <Field label="Side">{op.side === "client" ? "Client side" : "Server side"}</Field>
        <Field label="Server">{`${row.server_name} ${row.server_version}`.trim()}</Field>
        <Field label="Deploy">{row.deploy_id || "Unknown"}</Field>
        <Field label="Captured by">{SOURCE_LABEL[row.capture_source]}</Field>
        <Field label="Transport">{TRANSPORT_LABEL[row.transport]}</Field>
        <Field label="Protocol version">{op.protocol_version || "Unknown"}</Field>
        <Field label="JSON-RPC id">{op.jsonrpc_id || "None"}</Field>
        <Field label="Client (self-reported)">
          {`${row.client_name || "No name sent"} ${row.client_version}`.trim()}
        </Field>
        <Field label="User">
          {`${userLabel(row.user_id, row.user_subject_hash)} (${IDENTITY_LABEL[row.identity_source]})`}
        </Field>
        <Field label="Session">
          <span className="font-mono break-all">{row.group_key || "None"}</span>
          {row.group_key ? ` (${GROUP_KEY_LABEL[row.group_key_kind]})` : null}
        </Field>
        {row.status === "ok" ? null : (
          <>
            <Field label="Error class">{ERROR_LABEL[row.error_class]}</Field>
            <Field label="Caller fault">{row.caller_fault ? "Yes" : "No"}</Field>
            <Field label="JSON-RPC code">{row.jsonrpc_code ?? "None"}</Field>
            <Field label="Message">{row.error_message || "No message"}</Field>
          </>
        )}
        <Field label="Request size">
          {row.request_bytes ? formatBytes(row.request_bytes) : "Unknown"}
        </Field>
        <Field label="Response size">
          {row.response_bytes ? formatBytes(row.response_bytes) : "Unknown"}
        </Field>
        {op.http_status === null ? null : <Field label="HTTP status">{op.http_status}</Field>}
        {op.tenant ? <Field label="Tenant">{op.tenant}</Field> : null}
      </dl>
      <div className="grid gap-4 lg:grid-cols-2">
        <Content title="Arguments" content={op.args} captured={row.content_captured} />
        <Content title="Result" content={op.result} captured={row.content_captured} />
      </div>
      <div className="flex flex-col gap-2">
        <h3 className="text-data font-medium">Other side</h3>
        {op.other_side ? (
          <p className="text-data">
            <Link
              href={href("/mcp/operation/", {
                trace: op.other_side.trace_id,
                span: op.other_side.span_id,
                side: op.side === "client" ? "server" : "client",
              })}
            >
              {`${op.side === "client" ? "Server" : "Client"} side, ${formatDuration(op.other_side.duration_us)}`}
            </Link>
            {`, captured by ${SOURCE_LABEL[op.other_side.capture_source]}, ${operationStatus(op.other_side).label}.`}
          </p>
        ) : (
          <p className="text-data text-ink-soft">
            The other side of this call wasn&apos;t captured.
          </p>
        )}
      </div>
      {op.chain.length > 0 ? (
        <div className="flex flex-col gap-2">
          <h3 className="text-data font-medium">Multi-round exchange</h3>
          <ol className="flex flex-col gap-1 text-data">
            {op.chain.map((link) => (
              <li
                key={link.span_id}
                aria-current={link.span_id === row.span_id ? "step" : undefined}
                className="aria-[current=step]:font-medium"
              >
                {`Round ${link.round}: ${link.method} ${link.target_name}, ${formatTime(link.start_time)}`}
              </li>
            ))}
          </ol>
        </div>
      ) : null}
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-0.5">
      <dt className="text-label text-ink-soft">{label}</dt>
      <dd className="break-words">{children}</dd>
    </div>
  );
}

function Content({
  title,
  content,
  captured,
}: {
  title: string;
  content: McpContentPreview | null;
  captured: boolean;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-2">
      <h3 className="text-data font-medium">{title}</h3>
      {content ? (
        <>
          <pre className="max-h-80 overflow-auto rounded-md border border-ash-border bg-parchment-white p-3 font-mono text-label whitespace-pre-wrap break-all">
            {content.preview}
          </pre>
          {content.truncated ? (
            <p className="text-label text-ink-soft">
              {`Showing the start of ${formatBytes(content.bytes)}. Open in the trace to read all of it.`}
            </p>
          ) : null}
        </>
      ) : (
        <p className="text-data text-ink-soft">
          {captured
            ? "The captured content has expired."
            : "Content capture was off for this call."}
        </p>
      )}
    </div>
  );
}
