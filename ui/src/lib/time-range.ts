"use client";

import { createLocalStorage } from "@ui-sdk/core";
import { useSearchParams } from "next/navigation";
import { useMemo, useState, useSyncExternalStore } from "react";
import type { Granularity } from "@/api/generated/Granularity";
import type { TimeWindow } from "@/api/types";

export const RANGES = ["1h", "24h", "7d", "30d"] as const;
export type Range = (typeof RANGES)[number];
export const DEFAULT_RANGE: Range = "24h";

export const RANGE_PHRASE: Record<Range, string> = {
  "1h": "the last hour",
  "24h": "the last 24 hours",
  "7d": "the last 7 days",
  "30d": "the last 30 days",
};

const MINUTE_MS = 60_000;
const RANGE_MS: Record<Range, number> = {
  "1h": 60 * MINUTE_MS,
  "24h": 24 * 60 * MINUTE_MS,
  "7d": 7 * 24 * 60 * MINUTE_MS,
  "30d": 30 * 24 * 60 * MINUTE_MS,
};

/**
 * Buckets wide enough to read on a chart of this range: an hour in minutes, a day and a week in hours, and a
 * month in days. Lines and bars of one range share their buckets, so a tile and its chart agree.
 */
export function chartGranularity(range: Range): Granularity {
  switch (range) {
    case "1h":
      return "minute";
    case "24h":
    case "7d":
      return "hour";
    case "30d":
      return "day";
  }
}

export function parseRange(value: string | null | undefined): Range | null {
  return RANGES.find((range) => range === value) ?? null;
}

export function rangeStorageKey(project: string): string {
  return `crucible.range.${project}`;
}

// `to` rounds up to the next minute so a trace sent seconds ago is inside the window, and repeat
// fetches within a minute ask for the same window.
export function rangeWindow(range: Range, now: number): TimeWindow {
  const to = Math.ceil(now / MINUTE_MS) * MINUTE_MS;
  return { from: rfc3339(to - RANGE_MS[range]), to: rfc3339(to) };
}

function rfc3339(ms: number): string {
  return new Date(ms).toISOString().replace(/\.\d{3}Z$/, ".000000Z");
}

const noStoredValue = () => null;

// Precedence: the URL's `range`, then the range last chosen for this project, then 24h.
export function useTimeRange(project: string): [Range, (next: Range) => void] {
  const fromUrl = parseRange(useSearchParams().get("range"));
  const store = useMemo(() => {
    const storage = createLocalStorage(rangeStorageKey(project));
    return { storage, subscribe: (onChange: () => void) => storage.subscribe(onChange) };
  }, [project]);
  const stored = parseRange(
    useSyncExternalStore(store.subscribe, store.storage.get, noStoredValue),
  );

  // The router applies replaceState in a transition, so the choice is held here until the URL has
  // it; otherwise the control would show the old value for a render.
  const [chosen, setChosen] = useState<Range | null>(null);
  if (chosen !== null && chosen === fromUrl) setChosen(null);

  const setRange = (next: Range) => {
    setChosen(next);
    store.storage.set(next);
    const params = new URLSearchParams(window.location.search);
    params.set("range", next);
    window.history.replaceState(null, "", `?${params}`);
  };

  return [chosen ?? fromUrl ?? stored ?? DEFAULT_RANGE, setRange];
}
