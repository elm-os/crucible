import type { ProblemBody } from "./types";

export type UiErrorKind =
  | "network"
  | "unknown"
  | "too_heavy"
  | "too_broad"
  | "cursor_reset"
  | "unauthenticated"
  | "forbidden"
  | "not_found"
  | "invalid"
  | "rate_limited";

export type UiErrorAction = "retry" | "narrow_range" | "reset_list" | "sign_in" | "none";

export interface FieldError {
  pointer: string;
  code: string;
}

interface Copy {
  title: string;
  detail: string;
  action: UiErrorAction;
  autoRetries: number;
}

// Every message a person sees comes from this table; server `title`, `detail` and field messages
// are never shown.
const COPY: Record<UiErrorKind, Copy> = {
  network: {
    title: "Couldn't reach Crucible.",
    detail: "Check your connection, then try again.",
    action: "retry",
    autoRetries: 2,
  },
  unknown: {
    title: "This section couldn't load.",
    detail: "Nothing was changed. Try again in a moment.",
    action: "retry",
    autoRetries: 1,
  },
  too_heavy: {
    title: "This query was too heavy for the selected range.",
    detail: "Choose a shorter time range or remove a filter.",
    action: "narrow_range",
    autoRetries: 0,
  },
  too_broad: {
    title: "The range is longer than this project allows.",
    detail: "Choose a shorter time range.",
    action: "narrow_range",
    autoRetries: 0,
  },
  cursor_reset: {
    title: "List refreshed.",
    detail: "The list changed while you were paging, so it starts from the top again.",
    action: "reset_list",
    autoRetries: 0,
  },
  // Milestone 1 has no sessions: the Connect dialog's key stands in for signing in.
  unauthenticated: {
    title: "Connect to a project to see this.",
    detail: "This development preview has no sign-in yet. Connect with a project id and its key.",
    action: "sign_in",
    autoRetries: 0,
  },
  forbidden: {
    title: "Your role can't do this.",
    detail: "Ask a project admin for access.",
    action: "none",
    autoRetries: 0,
  },
  not_found: {
    title: "This isn't in this project.",
    detail: "It may have been deleted, or it expired under the retention policy.",
    action: "none",
    autoRetries: 0,
  },
  invalid: {
    title: "Some fields need attention.",
    detail: "Check the highlighted fields, then try again.",
    action: "none",
    autoRetries: 0,
  },
  rate_limited: {
    title: "Too many requests.",
    detail: "Crucible will retry shortly.",
    action: "retry",
    autoRetries: 2,
  },
};

const REQUEST_ID = /^[A-Za-z0-9_-]{1,64}$/;

export class UiError extends Error {
  readonly kind: UiErrorKind;
  readonly status: number | null;
  readonly code: string | null;
  readonly title: string;
  readonly detail: string;
  readonly action: UiErrorAction;
  readonly requestId: string | null;
  readonly retryAfterS: number | null;
  readonly fieldErrors: readonly FieldError[];

  constructor(init: {
    kind: UiErrorKind;
    status?: number | null;
    code?: string | null;
    requestId?: string | null;
    retryAfterS?: number | null;
    fieldErrors?: readonly FieldError[];
  }) {
    const copy = COPY[init.kind];
    super(copy.title);
    this.name = "UiError";
    this.kind = init.kind;
    this.status = init.status ?? null;
    this.code = init.code ?? null;
    this.title = copy.title;
    this.retryAfterS = init.retryAfterS ?? null;
    this.detail =
      init.kind === "rate_limited" && this.retryAfterS !== null
        ? `Retrying in ${this.retryAfterS} s.`
        : copy.detail;
    this.action = copy.action;
    this.requestId = init.requestId ?? null;
    this.fieldErrors = init.fieldErrors ?? [];
  }
}

export interface HttpFailure {
  status: number;
  /** The parsed problem+json body, or null when the response was not one. */
  problem: ProblemBody | null;
  requestIdHeader: string | null;
  retryAfterHeader: string | null;
}

export function networkError(): UiError {
  return new UiError({ kind: "network" });
}

export function fromHttpFailure(failure: HttpFailure): UiError {
  const { status, problem } = failure;
  const code = typeof problem?.code === "string" ? problem.code : null;
  return new UiError({
    kind: classify(status, problem !== null, code),
    status,
    code,
    requestId: validRequestId(problem?.request_id) ?? validRequestId(failure.requestIdHeader),
    retryAfterS: retryAfter(failure.retryAfterHeader, problem),
    fieldErrors: fieldErrors(problem),
  });
}

export function asUiError(error: unknown): UiError {
  return error instanceof UiError ? error : new UiError({ kind: "unknown" });
}

export function shouldRetry(failureCount: number, error: unknown): boolean {
  return error instanceof UiError && failureCount < COPY[error.kind].autoRetries;
}

export function retryDelayMs(attempt: number, error: unknown): number {
  if (error instanceof UiError && error.retryAfterS !== null) return error.retryAfterS * 1000;
  return Math.min(1000 * 2 ** attempt, 8000);
}

// Statuses whose meaning holds whoever answered come first. Entity and role copy needs a Crucible
// problem body, because a proxy or static host answering 403 or 404 says nothing about the project.
function classify(status: number, isProblem: boolean, code: string | null): UiErrorKind {
  if (code === "query_timeout" || code === "query_memory_exceeded") return "too_heavy";
  if (code === "query_too_broad") return "too_broad";
  if (code === "cursor_invalid") return "cursor_reset";
  if (status === 401) return "unauthenticated";
  if (status === 429) return "rate_limited";
  if (!isProblem) return "unknown";
  if (status === 403 && (code === null || code === "forbidden")) return "forbidden";
  if (status === 404 && (code === null || code === "not_found")) return "not_found";
  if (status === 409 || status === 422) return "invalid";
  return "unknown";
}

function validRequestId(value: unknown): string | null {
  return typeof value === "string" && REQUEST_ID.test(value) ? value : null;
}

function retryAfter(header: string | null, problem: ProblemBody | null): number | null {
  if (header !== null && /^\d+$/.test(header.trim())) return Number(header.trim());
  const fromBody = problem?.retry_after_s;
  return typeof fromBody === "number" && Number.isFinite(fromBody) && fromBody >= 0
    ? fromBody
    : null;
}

function fieldErrors(problem: ProblemBody | null): FieldError[] {
  if (!Array.isArray(problem?.errors)) return [];
  return problem.errors.flatMap((entry) =>
    typeof entry?.pointer === "string" && typeof entry.code === "string"
      ? [{ pointer: entry.pointer, code: entry.code }]
      : [],
  );
}
