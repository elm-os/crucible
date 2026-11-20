import type { CaptureClass } from "@/api/generated/CaptureClass";
import type { ErrorClass } from "@/api/generated/ErrorClass";
import type { CaptureSource } from "@/api/generated/CaptureSource";
import type { GroupKeyKind } from "@/api/generated/GroupKeyKind";
import type { IdentitySource } from "@/api/generated/IdentitySource";
import type { McpOperationRow } from "@/api/generated/McpOperationRow";
import type { McpServerSource } from "@/api/generated/McpServerSource";
import type { StatusCode } from "@/api/generated/StatusCode";
import type { Transport } from "@/api/generated/Transport";
import { formatDuration } from "@/lib/format";

export function formatRate(rate: number | null): string {
  if (rate === null) return "No calls";
  if (rate === 0) return "0%";
  if (rate < 0.001) return "<0.1%";
  return `${(rate * 100).toFixed(rate < 0.1 ? 1 : 0)}%`;
}

export function formatLatency(us: number | null): string {
  return us === null ? "No calls" : formatDuration(us);
}

export const CLASS_LABEL: Record<CaptureClass, string> = {
  sdk: "SDK",
  proxy: "proxy",
  otlp: "OTLP",
};

export const ERROR_LABEL: Record<ErrorClass, string> = {
  none: "No error",
  protocol: "Protocol",
  validation: "Invalid arguments",
  not_found: "Not found",
  tool_execution: "Tool failed",
  auth: "Auth",
  rate_limit: "Rate limited",
  timeout: "Timeout",
  transport: "Transport",
  cancelled: "Cancelled",
  internal: "Internal",
};

// Which capture source each server was read from, in a sentence: "files from the SDK, db from OTLP".
export function sourcesSentence(sources: McpServerSource[]): string | null {
  const read = sources.filter((s) => s.chosen !== null);
  if (read.length === 0) return null;
  return read
    .map(
      (s) =>
        `${s.server_name} from ${s.chosen === "otlp" ? "" : "the "}${CLASS_LABEL[s.chosen ?? "sdk"]}`,
    )
    .join(", ");
}

export const IDENTITY_LABEL: Record<IdentitySource, string> = {
  none: "Unknown",
  identify_hook: "Identify hook",
  token_claims: "Token claims",
  token_verified: "Verified token",
};

export const GROUP_KEY_LABEL: Record<GroupKeyKind, string> = {
  legacy_session: "Session id",
  handle: "Session handle",
  trace: "Trace",
  process: "Process",
};

export const SOURCE_LABEL: Record<CaptureSource, string> = {
  sdk: "SDK",
  proxy_stdio: "Proxy (stdio)",
  proxy_http: "Proxy (HTTP)",
  otlp: "OTLP",
};

export const TRANSPORT_LABEL: Record<Transport, string> = {
  unknown: "Unknown",
  stdio: "stdio",
  streamable_http: "Streamable HTTP",
  sse: "SSE",
};

// The operation's outcome as a status icon and words; a caller fault says so.
export function operationStatus(row: McpOperationRow): { status: StatusCode; label: string } {
  if (row.status === "ok") return { status: "ok", label: "OK" };
  if (row.status === "cancelled") return { status: "unset", label: "Cancelled" };
  const label = ERROR_LABEL[row.error_class];
  return { status: "error", label: row.caller_fault ? `${label} (caller)` : label };
}

// A user by id when the identify hook named one, else by the start of the token subject's hash.
export function userLabel(userId: string, subjectHash: string | null): string {
  if (userId) return userId;
  if (subjectHash) return `Subject ${subjectHash.slice(0, 12)}`;
  return "Anonymous";
}
