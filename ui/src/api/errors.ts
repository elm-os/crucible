import type { ProblemBody } from "./types";

export type UiErrorKind =
  | "network"
  | "unknown"
  | "too_heavy"
  | "too_broad"
  | "cursor_reset"
  | "unauthenticated"
  | "signed_out"
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
    detail: "Try again in a moment.",
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
  // Without a control plane the server has no sign-in: the Connect dialog's key stands in for it.
  unauthenticated: {
    title: "Connect to a project to see this.",
    detail: "This server has no sign-in. Connect with a project id and its secret key.",
    action: "sign_in",
    autoRetries: 0,
  },
  signed_out: {
    title: "You're signed out.",
    detail: "Your session ended. Sign in again to continue.",
    action: "sign_in",
    autoRetries: 0,
  },
  forbidden: {
    title: "Your role can't see this.",
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
  // A section's request carries only the page's filters and time range, which is all a person can change.
  invalid: {
    title: "This view's filters aren't valid.",
    detail: "Clear the filters or choose another time range, then try again.",
    action: "none",
    autoRetries: 0,
  },
  // Shown once the automatic retries have run out, so it says what to do rather than what happens next.
  rate_limited: {
    title: "Too many requests.",
    detail: "Wait a minute, then try again.",
    action: "retry",
    autoRetries: 2,
  },
};

// Codes of contract 12.3 for outcomes a person can act on from the message alone. Server faults, codes that
// mean this page sent a malformed request, and codes this page doesn't know are unexpected.
const UNDERSTOOD = new Set([
  "unauthenticated",
  "invalid_credentials",
  "forbidden",
  "csrf_invalid",
  "origin_invalid",
  "not_found",
  "conflict",
  "idempotency_key_reused",
  "precondition_failed",
  "payload_too_large",
  "validation_failed",
  "token_invalid",
  "rate_limited",
  "query_too_broad",
  "query_timeout",
  "query_memory_exceeded",
  "cursor_invalid",
  "feature_disabled",
  "ssrf_blocked",
  "media_not_supported",
  "media_too_large",
  "media_hash_mismatch",
  "sql_invalid",
  "sql_forbidden",
  "sql_time_window_required",
  "sql_quota_exceeded",
  "sql_workspace_unavailable",
  "export_expired",
]);

// The request id an administrator searches the server log for. Only an unexpected failure shows it:
// one with a code outside UNDERSTOOD, or an answer that wasn't a Crucible problem at all.
function referenceOf(kind: UiErrorKind, code: string | null, requestId: string | null) {
  if (code === null) return kind === "unknown" ? requestId : null;
  return UNDERSTOOD.has(code) ? null : requestId;
}

function waitCopy(seconds: number): string {
  return `Wait ${seconds} ${seconds === 1 ? "second" : "seconds"}, then try again.`;
}

const REQUEST_ID = /^[A-Za-z0-9_-]{1,64}$/;

export class UiError extends Error {
  readonly kind: UiErrorKind;
  readonly status: number | null;
  readonly code: string | null;
  readonly title: string;
  readonly detail: string;
  readonly action: UiErrorAction;
  readonly requestId: string | null;
  /** The request id when the failure is unexpected, else null: what a page may show beside the error. */
  readonly reference: string | null;
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
        ? waitCopy(this.retryAfterS)
        : copy.detail;
    this.action = copy.action;
    this.requestId = init.requestId ?? null;
    this.reference = referenceOf(this.kind, this.code, this.requestId);
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
