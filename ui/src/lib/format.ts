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
