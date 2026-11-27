import { fromHttpFailure, type HttpFailure, UiError } from "@/api/errors";

interface Copy {
  title: string;
  /** Shown when the server sent no fix sentence, or always when `own` is set. */
  fix: string;
  /** The page's fix replaces the server's, whose sentence names internals. */
  own?: boolean;
}

// The workspace writes `detail` as the sentence that says how to fix the request (contract 12.3), so
// for these codes the page shows it under a title it owns. Every other failure keeps the shared copy.
const COPY: Record<string, Copy> = {
  sql_invalid: {
    title: "The query has an error.",
    fix: "Correct the query text, then run it again.",
  },
  sql_forbidden: {
    title: "The workspace doesn't allow this query.",
    fix: "Read only the relations listed under Schema, with one SELECT or WITH statement.",
  },
  sql_time_window_required: {
    title: "The query needs the time window.",
    fix: "Filter the relation's time column with {from:DateTime64(6)} and {to:DateTime64(6)}.",
  },
  sql_quota_exceeded: {
    title: "This project has reached its SQL limit for now.",
    fix: "Wait, then run the query again.",
  },
  sql_workspace_unavailable: {
    title: "The SQL workspace is unavailable.",
    fix: "Try again shortly.",
  },
  query_too_broad: {
    title: "The query would read too much.",
    fix: "Choose a shorter time range or add a filter.",
  },
  query_timeout: {
    title: "The query took too long.",
    fix: "Choose a shorter time range, filter earlier or aggregate less.",
  },
  query_memory_exceeded: {
    title: "The query needed too much memory.",
    fix: "Choose a shorter time range, select fewer columns or group by fewer keys.",
  },
  validation_failed: {
    title: "The request needs a correction.",
    fix: "Check the parameter values and the form fields, then try again.",
  },
  forbidden: {
    title: "Your role can't do this.",
    fix: "Running ad hoc SQL needs the member role on this project. Viewers can run the project's saved queries.",
  },
  conflict: {
    title: "The run was cancelled.",
    fix: "Run the query again when you are ready.",
  },
  precondition_failed: {
    title: "This saved query changed since you opened it.",
    fix: "Reload it, then apply your change again.",
  },
  control_plane_unavailable: {
    title: "Saved queries and history are unavailable right now.",
    fix: "Crucible can't reach its database. Try again shortly.",
    own: true,
  },
};

// Operator guidance for each reason the workspace gives for being off.
const UNAVAILABLE: Record<string, Copy> = {
  isolation_probe_failed: {
    title: "The SQL workspace is switched off.",
    fix: "Its isolation self-check failed on this ClickHouse server, so Crucible runs no queries here. An operator should read the health page, run crucible migrate and check the server's access configuration.",
  },
  not_configured: {
    title: "The SQL workspace is not set up on this server.",
    fix: "It needs ClickHouse with access management and the Postgres control plane. An operator configures both and runs crucible migrate.",
  },
  probe_pending: {
    title: "The SQL workspace is starting.",
    fix: "Its isolation self-check is still running. Check again in a moment.",
  },
  probe_unavailable: {
    title: "The SQL workspace can't reach ClickHouse.",
    fix: "Its isolation self-check could not run. An operator should check that ClickHouse is up and run crucible migrate.",
  },
};

export interface WorkspaceNotice {
  reason: string | null;
  title: string;
  detail: string;
}

export function workspaceNotice(reason: string | null, serverFix: string | null): WorkspaceNotice {
  const known = reason === null ? undefined : UNAVAILABLE[reason];
  const fallback = COPY.sql_workspace_unavailable as Copy;
  return {
    reason,
    title: (known ?? fallback).title,
    detail: known?.fix ?? serverFix ?? fallback.fix,
  };
}

export class SqlError extends UiError {
  override readonly title: string;
  override readonly detail: string;
  /** ClickHouse's own description of a query error. */
  readonly clickhouse: string | null;
  readonly line: number | null;
  readonly column: number | null;
  readonly reason: string | null;

  constructor(base: UiError, failure: HttpFailure, copy: Copy) {
    super(base);
    const problem = failure.problem;
    const serverFix = text(problem?.detail);
    this.reason = text(problem?.reason);
    this.line = position(problem?.line);
    this.column = position(problem?.column);
    this.clickhouse = clickhouseMessage(problem?.errors);
    if (base.code === "sql_workspace_unavailable") {
      const notice = workspaceNotice(this.reason, serverFix);
      this.title = notice.title;
      this.detail = notice.detail;
    } else {
      this.title = copy.title;
      this.detail = (copy.own ? null : serverFix) ?? copy.fix;
    }
  }
}

// A failure of a workspace route: an `SqlError` for the codes the workspace explains, otherwise the
// shared error with the shared copy.
export function sqlErrorFrom(failure: HttpFailure): UiError {
  const base = fromHttpFailure(failure);
  const copy = base.code === null ? undefined : COPY[base.code];
  return copy === undefined ? base : new SqlError(base, failure, copy);
}

export function unavailableNotice(error: unknown): WorkspaceNotice | null {
  return error instanceof SqlError && error.code === "sql_workspace_unavailable"
    ? { reason: error.reason, title: error.title, detail: error.detail }
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

function position(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 ? value : null;
}

function clickhouseMessage(errors: unknown): string | null {
  if (!Array.isArray(errors)) return null;
  const first: unknown = errors[0];
  if (typeof first !== "object" || first === null) return null;
  const entry = first as { code?: unknown; message?: unknown };
  return entry.code === "clickhouse" ? text(entry.message) : null;
}
