import type { AlertComparison } from "@/api/generated/AlertComparison";
import type { AlertNoDataPolicy } from "@/api/generated/AlertNoDataPolicy";
import type { AlertRule } from "@/api/generated/AlertRule";
import type { AlertStatus } from "@/api/generated/AlertStatus";
import type { ChannelKind } from "@/api/generated/ChannelKind";
import type { McpSideParam } from "@/api/generated/McpSideParam";
import type { SeriesKey } from "@/api/generated/SeriesKey";
import type { SourceParam } from "@/api/generated/SourceParam";
import { formatCost, formatCount, formatDuration } from "@/lib/format";

export type Unit = "count" | "tokens" | "us" | "nusd" | "ppm";

// What a rule reads: the span rollup, or the MCP rollup of one side (contracts 9.9 and 12.6).
export type RuleSource = "spans" | "mcp_ops";

export const RULE_SOURCES: { value: RuleSource; label: string }[] = [
  { value: "spans", label: "Spans" },
  { value: "mcp_ops", label: "MCP calls" },
];

export const SIDES: { value: McpSideParam; label: string }[] = [
  { value: "server", label: "Server side" },
  { value: "client", label: "Client side" },
];

export interface MeasureOption {
  value: string;
  label: string;
  unit: Unit;
  aggregations: readonly string[];
  source: RuleSource;
}

const SUM = ["sum"] as const;
const PERCENTILES = ["p50", "p90", "p95", "p99"] as const;

// The measures of the span rollup and of the MCP rollup, which are what alert rules read. An MCP percentile is
// a measure of its own. The server checks the rule again, so a measure it no longer has comes back as a field
// error.
export const MEASURES: MeasureOption[] = [
  { value: "error_count", label: "Errors", unit: "count", aggregations: SUM, source: "spans" },
  { value: "count", label: "Spans", unit: "count", aggregations: SUM, source: "spans" },
  { value: "duration", label: "Duration", unit: "us", aggregations: PERCENTILES, source: "spans" },
  {
    value: "ttft",
    label: "Time to first token",
    unit: "us",
    aggregations: PERCENTILES,
    source: "spans",
  },
  { value: "total_cost", label: "Total cost", unit: "nusd", aggregations: SUM, source: "spans" },
  {
    value: "total_tokens",
    label: "Total tokens",
    unit: "tokens",
    aggregations: SUM,
    source: "spans",
  },
  {
    value: "input_tokens",
    label: "Input tokens",
    unit: "tokens",
    aggregations: SUM,
    source: "spans",
  },
  {
    value: "output_tokens",
    label: "Output tokens",
    unit: "tokens",
    aggregations: SUM,
    source: "spans",
  },
  {
    value: "mcp.error_rate",
    label: "MCP error rate",
    unit: "ppm",
    aggregations: ["rate"],
    source: "mcp_ops",
  },
  {
    value: "mcp.error_count",
    label: "MCP errors",
    unit: "count",
    aggregations: SUM,
    source: "mcp_ops",
  },
  { value: "mcp.calls", label: "MCP calls", unit: "count", aggregations: SUM, source: "mcp_ops" },
  {
    value: "mcp.duration_p50",
    label: "MCP duration p50",
    unit: "us",
    aggregations: ["p50"],
    source: "mcp_ops",
  },
  {
    value: "mcp.duration_p90",
    label: "MCP duration p90",
    unit: "us",
    aggregations: ["p90"],
    source: "mcp_ops",
  },
  {
    value: "mcp.duration_p99",
    label: "MCP duration p99",
    unit: "us",
    aggregations: ["p99"],
    source: "mcp_ops",
  },
];

export function measuresOf(source: RuleSource): MeasureOption[] {
  return MEASURES.filter((measure) => measure.source === source);
}

export const NO_DIMENSION = "none";

const DIMENSION_OPTIONS: { value: string; label: string; source: RuleSource }[] = [
  { value: "environment", label: "Environment", source: "spans" },
  { value: "model", label: "Model", source: "spans" },
  { value: "observation_type", label: "Span type", source: "spans" },
  { value: "level", label: "Level", source: "spans" },
  { value: "release", label: "Release", source: "spans" },
  { value: "server_name", label: "Server", source: "mcp_ops" },
  { value: "target_name", label: "Tool", source: "mcp_ops" },
];

// The dimensions a rule of `source` may group by, after None.
export function dimensionsOf(source: RuleSource): { value: string; label: string }[] {
  return [
    { value: NO_DIMENSION, label: "None" },
    ...DIMENSION_OPTIONS.filter((dimension) => dimension.source === source),
  ];
}

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

// The capture sources an MCP rule may read, as the MCP pages name them.
export const CAPTURE_SOURCES: { value: SourceParam; label: string }[] = [
  { value: "auto", label: "Automatic" },
  { value: "sdk", label: "SDK" },
  { value: "proxy", label: "Proxy" },
  { value: "otlp", label: "OTLP" },
];

// "Server side, capture source Automatic": what an MCP rule reads.
export function mcpReadText(rule: Pick<AlertRule, "side" | "capture_source">): string {
  const side = rule.side === "client" ? "Client side" : "Server side";
  const source = CAPTURE_SOURCES.find((option) => option.value === rule.capture_source);
  return `${side}, capture source ${source?.label ?? "Automatic"}`;
}

export function isUnit(unit: string): unit is Unit {
  return (
    unit === "count" || unit === "tokens" || unit === "us" || unit === "nusd" || unit === "ppm"
  );
}

const percent = new Intl.NumberFormat("en-US", { maximumFractionDigits: 4 });

// A stored value as people read it: counts as they are, microseconds as a duration, nano-USD as dollars,
// parts per million as a percentage.
export function formatValue(unit: string, value: number): string {
  if (unit === "us") return formatDuration(value);
  if (unit === "nusd") return formatCost(value / 1e9);
  if (unit === "ppm") return `${percent.format(value / 10_000)}%`;
  return formatCount(value);
}

// What the threshold field asks for, per unit.
export const THRESHOLD_UNIT: Readonly<Record<Unit, string>> = {
  count: "",
  tokens: "tokens",
  us: "milliseconds",
  nusd: "US dollars",
  ppm: "percent",
};

// The threshold as typed, in the stored unit: a whole count, milliseconds as microseconds, dollars as
// nano-USD, a percentage as parts per million. Commas, a leading dollar sign and a trailing percent sign are
// forgiven. `null` when it is not a number.
export function thresholdToStored(unit: Unit, text: string): number | null {
  const cleaned = text.trim().replace(/^\$/, "").replace(/%$/, "").replaceAll(",", "");
  if (cleaned === "" || !/^-?\d*\.?\d*$/.test(cleaned)) return null;
  const typed = Number(cleaned);
  if (!Number.isFinite(typed)) return null;
  const scale = unit === "us" ? 1_000 : unit === "nusd" ? 1e9 : unit === "ppm" ? 10_000 : 1;
  const rounded = Math.round(typed * scale);
  return Math.abs(rounded) <= Number.MAX_SAFE_INTEGER ? rounded : null;
}

export function thresholdFromStored(unit: string, value: number): string {
  if (unit === "us") return String(value / 1_000);
  if (unit === "nusd") return String(value / 1e9);
  if (unit === "ppm") return String(value / 10_000);
  return String(value);
}

// 90 is "90 s", 300 is "5 min", 14400 is "4 h".
export function formatSpan(secs: number): string {
  if (secs % 3_600 === 0 && secs >= 3_600) return `${secs / 3_600} h`;
  if (secs % 60 === 0 && secs >= 60) return `${secs / 60} min`;
  return `${secs} s`;
}

// A series by its dimension values; a rule without dimensions has one series, of all its spans or calls.
export function seriesLabel(key: readonly SeriesKey[], source = "spans"): string {
  if (key.length === 0) return source === "mcp_ops" ? "All calls" : "All spans";
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

// "Errors above 5 over 5 min"; a percentile is named when the measure has several.
export function conditionText(
  rule: Pick<
    AlertRule,
    "measure" | "aggregation" | "comparison" | "threshold" | "unit" | "window_secs"
  >,
): string {
  const option = MEASURES.find((candidate) => candidate.value === rule.measure);
  const measure = option?.label ?? rule.measure;
  const named = option === undefined || option.aggregations.length > 1;
  const aggregation = rule.aggregation === "sum" || !named ? "" : ` ${rule.aggregation}`;
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
  receiver_rejected: "The receiver refused the test. Check the URL and its credentials.",
  receiver_failed: "The receiver ran into an error of its own. Check the receiver's logs.",
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
