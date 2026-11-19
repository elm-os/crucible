import type { AlertComparison } from "@/api/generated/AlertComparison";
import type { AlertNoDataPolicy } from "@/api/generated/AlertNoDataPolicy";
import type { AlertRule } from "@/api/generated/AlertRule";
import type { AlertStatus } from "@/api/generated/AlertStatus";
import type { ChannelKind } from "@/api/generated/ChannelKind";
import type { SeriesKey } from "@/api/generated/SeriesKey";
import { formatCost, formatCount, formatDuration } from "@/lib/format";

export type Unit = "count" | "tokens" | "us" | "nusd";

export interface MeasureOption {
  value: string;
  label: string;
  unit: Unit;
  aggregations: readonly string[];
}

const SUM = ["sum"] as const;
const PERCENTILES = ["p50", "p90", "p95", "p99"] as const;

// The measures of the span rollup, which is what alert rules read (contract 12.6). The server checks the
// rule again, so a measure it no longer has comes back as a field error.
export const MEASURES: MeasureOption[] = [
  { value: "error_count", label: "Errors", unit: "count", aggregations: SUM },
  { value: "count", label: "Spans", unit: "count", aggregations: SUM },
  { value: "duration", label: "Duration", unit: "us", aggregations: PERCENTILES },
  { value: "ttft", label: "Time to first token", unit: "us", aggregations: PERCENTILES },
  { value: "total_cost", label: "Total cost", unit: "nusd", aggregations: SUM },
  { value: "total_tokens", label: "Total tokens", unit: "tokens", aggregations: SUM },
  { value: "input_tokens", label: "Input tokens", unit: "tokens", aggregations: SUM },
  { value: "output_tokens", label: "Output tokens", unit: "tokens", aggregations: SUM },
];

export const NO_DIMENSION = "none";

export const DIMENSIONS: { value: string; label: string }[] = [
  { value: NO_DIMENSION, label: "None" },
  { value: "environment", label: "Environment" },
  { value: "model", label: "Model" },
  { value: "observation_type", label: "Span type" },
  { value: "level", label: "Level" },
  { value: "release", label: "Release" },
];

export const COMPARISONS: { value: AlertComparison; label: string }[] = [
  { value: "gt", label: "is above" },
  { value: "gte", label: "is at or above" },
  { value: "lt", label: "is below" },
  { value: "lte", label: "is at or below" },
];

export const NO_DATA: { value: AlertNoDataPolicy; label: string }[] = [
  { value: "keep", label: "Keep the current state" },
  { value: "ok", label: "Treat it as OK" },
  { value: "no_data", label: "Notify that there is no data" },
];

export const AGGREGATION_LABEL: Readonly<Record<string, string>> = {
  sum: "Sum",
  p50: "Median (p50)",
  p90: "p90",
  p95: "p95",
  p99: "p99",
};

export const STATUS_LABEL: Readonly<Record<AlertStatus, string>> = {
  ok: "OK",
  pending: "Pending",
  firing: "Firing",
  resolved: "Resolved",
  no_data: "No data",
};

export const KIND_LABEL: Readonly<Record<ChannelKind, string>> = {
  slack: "Slack",
  email: "Email",
  webhook: "Webhook",
};

export function measureOf(id: string): MeasureOption {
  return MEASURES.find((measure) => measure.value === id) ?? MEASURES[0];
}

export function isUnit(unit: string): unit is Unit {
  return unit === "count" || unit === "tokens" || unit === "us" || unit === "nusd";
}

// A stored value as people read it: counts as they are, microseconds as a duration, nano-USD as dollars.
export function formatValue(unit: string, value: number): string {
  if (unit === "us") return formatDuration(value);
  if (unit === "nusd") return formatCost(value / 1e9);
  return formatCount(value);
}

// What the threshold field asks for, per unit.
export const THRESHOLD_UNIT: Readonly<Record<Unit, string>> = {
  count: "",
  tokens: "tokens",
  us: "milliseconds",
  nusd: "US dollars",
};

// The threshold as typed, in the stored unit: a whole count, milliseconds as microseconds, dollars as
// nano-USD. Commas and a leading dollar sign are forgiven. `null` when it is not a number.
export function thresholdToStored(unit: Unit, text: string): number | null {
  const cleaned = text.trim().replace(/^\$/, "").replaceAll(",", "");
  if (cleaned === "" || !/^-?\d*\.?\d*$/.test(cleaned)) return null;
  const typed = Number(cleaned);
  if (!Number.isFinite(typed)) return null;
  const stored = unit === "us" ? typed * 1_000 : unit === "nusd" ? typed * 1e9 : typed;
  const rounded = Math.round(stored);
  return Math.abs(rounded) <= Number.MAX_SAFE_INTEGER ? rounded : null;
}

export function thresholdFromStored(unit: string, value: number): string {
  if (unit === "us") return String(value / 1_000);
  if (unit === "nusd") return String(value / 1e9);
  return String(value);
}

// 90 is "90 s", 300 is "5 min", 14400 is "4 h".
export function formatSpan(secs: number): string {
  if (secs % 3_600 === 0 && secs >= 3_600) return `${secs / 3_600} h`;
  if (secs % 60 === 0 && secs >= 60) return `${secs / 60} min`;
  return `${secs} s`;
}

export function seriesLabel(key: readonly SeriesKey[]): string {
  if (key.length === 0) return "All spans";
  return key
    .map((part) => `${part.dimension}=${part.value === "" ? "(empty)" : part.value}`)
    .join(", ");
}

const COMPARISON_TEXT: Readonly<Record<AlertComparison, string>> = {
  gt: "above",
  gte: "at or above",
  lt: "below",
  lte: "at or below",
};

// "Errors (sum) above 5 over 5 min".
export function conditionText(
  rule: Pick<
    AlertRule,
    "measure" | "aggregation" | "comparison" | "threshold" | "unit" | "window_secs"
  >,
): string {
  const measure = MEASURES.find((option) => option.value === rule.measure)?.label ?? rule.measure;
  const aggregation = rule.aggregation === "sum" ? "" : ` ${rule.aggregation}`;
  const threshold = formatValue(rule.unit, rule.threshold);
  return `${measure}${aggregation} ${COMPARISON_TEXT[rule.comparison]} ${threshold} over ${formatSpan(rule.window_secs)}`;
}

// Why a test-send failed, in the page's words. The server sends a stable code and never the receiver's
// answer (contract 12.6).
const TEST_FAILURES: Readonly<Record<string, string>> = {
  connect_failed:
    "Crucible couldn't connect to the receiver. Check the address and that it is running.",
  timeout: "The receiver didn't answer in time.",
  tls_failed: "The receiver's TLS certificate wasn't accepted.",
  no_address: "The receiver's host name has no address. Check the name.",
  dns_failed: "The receiver's host name couldn't be looked up. Check the name.",
  protocol_error: "The receiver didn't answer in HTTP. Check the URL and its port.",
  receiver_rejected:
    "The receiver refused the test (a 4xx answer). Check the URL and its credentials.",
  receiver_failed: "The receiver failed (a 5xx answer). Check the receiver's own logs.",
  redirect_not_followed:
    "The receiver answered with a redirect, which Crucible doesn't follow. Use the final URL.",
  metadata_address: "The URL points at a cloud metadata address, which Crucible never calls.",
  private_address:
    "This server only calls public addresses (egress.mode = strict). Ask an operator to add the range to egress.allow_cidrs.",
  loopback_address:
    "This server only calls public addresses (egress.mode = strict). Ask an operator to add the range to egress.allow_cidrs.",
  link_local_address:
    "This server only calls public addresses (egress.mode = strict). Ask an operator to add the range to egress.allow_cidrs.",
  cgnat_address:
    "This server only calls public addresses (egress.mode = strict). Ask an operator to add the range to egress.allow_cidrs.",
  reserved_address:
    "This server only calls public addresses (egress.mode = strict). Ask an operator to add the range to egress.allow_cidrs.",
  https_required: "This server only calls https URLs (egress.mode = strict). Use an https:// URL.",
  own_infrastructure: "The URL points at one of Crucible's own databases.",
  smtp_not_configured:
    "This server has no mail server. Ask an operator to set smtp.host and smtp.from.",
  smtp_unavailable: "The mail server didn't accept the message. Try again in a moment.",
  smtp_rejected: "The mail server rejected the message. Check the recipients.",
  secret_missing: "The channel's secret can't be read. Save its URL again, or rotate its secret.",
  secret_unreadable:
    "The channel's secret can't be read. Save its URL again, or rotate its secret.",
};

export function testFailure(code: string | undefined): string {
  const known = code === undefined ? undefined : TEST_FAILURES[code];
  return known ?? "The test wasn't delivered. Check the receiver, then send another test.";
}

// Why a URL was refused when a channel was saved: the code of the `/url` field error.
export function urlRefusal(code: string | undefined): string {
  switch (code) {
    case "metadata_address":
      return "That is a cloud metadata address, which Crucible never calls. Use the receiver's own address.";
    case "https_required":
      return "This server only calls https URLs (egress.mode = strict). Use an https:// URL.";
    case "own_infrastructure":
      return "That is one of Crucible's own databases. Use the receiver's own address.";
    case "private_address":
    case "loopback_address":
    case "link_local_address":
    case "cgnat_address":
    case "reserved_address":
      return "This server only calls public addresses (egress.mode = strict). Ask an operator to add the range to egress.allow_cidrs, or to set egress.mode = open.";
    case "unsupported_scheme":
      return "Use an http:// or https:// URL.";
    default:
      return "Enter a full URL, like https://example.com/hooks/crucible, without a user name or password.";
  }
}
