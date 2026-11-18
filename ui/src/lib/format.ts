const timeFormat = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
});

export function formatTime(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : timeFormat.format(date);
}

export function formatDuration(us: number): string {
  if (us < 1_000) return "<1 ms";
  if (us < 1_000_000) return `${Math.round(us / 1_000)} ms`;
  if (us < 60_000_000) return `${(us / 1_000_000).toFixed(1)} s`;
  const seconds = Math.round(us / 1_000_000);
  return `${Math.floor(seconds / 60)} min ${seconds % 60} s`;
}

export function formatOffset(us: number): string {
  return us === 0 ? "0 ms" : `+${formatDuration(us)}`;
}

const smallCost = new Intl.NumberFormat("en-US", {
  style: "currency",
  currency: "USD",
  maximumSignificantDigits: 2,
});
const cost = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });

export function formatCost(usd: number): string {
  if (usd === 0) return "$0";
  if (usd < 0.0001) return "<$0.0001";
  return usd < 1 ? smallCost.format(usd) : cost.format(usd);
}

// The API writes times as RFC 3339 UTC with six fractional digits; Date keeps only milliseconds, so the
// microseconds are read from the text.
export function parseMicros(iso: string): number | null {
  const match = /^(.{19})(?:\.(\d{1,6}))?Z$/.exec(iso);
  if (!match) return null;
  const ms = Date.parse(`${match[1]}Z`);
  if (Number.isNaN(ms)) return null;
  return ms * 1_000 + Number((match[2] ?? "").padEnd(6, "0"));
}
