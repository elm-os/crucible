"use client";

import { useSearchParams } from "next/navigation";
import { useState } from "react";
import type { Level } from "@/api/generated/Level";
import type { ObservationType } from "@/api/generated/ObservationType";
import type { ListFilters } from "@/api/types";

export const LEVELS: readonly Level[] = ["debug", "default", "warning", "error"];
export const OBSERVATION_TYPES: readonly ObservationType[] = [
  "span",
  "generation",
  "event",
  "embedding",
  "agent",
  "tool",
  "chain",
  "retriever",
  "evaluator",
  "guardrail",
];

type ListField = Exclude<keyof ListFilters, "name">;

export interface FilterField {
  key: ListField;
  label: string;
  // Fields with a closed set of values are chosen from it; the rest are typed.
  values?: readonly string[];
  placeholder?: string;
}

export const FILTER_FIELDS: readonly FilterField[] = [
  { key: "environment", label: "Environment", placeholder: "production" },
  { key: "level", label: "Level", values: LEVELS },
  { key: "observation_type", label: "Type", values: OBSERVATION_TYPES },
  { key: "user_id", label: "User", placeholder: "user id" },
  { key: "session_id", label: "Session", placeholder: "session id" },
  { key: "tags", label: "Tag", placeholder: "tag" },
  { key: "metadata", label: "Metadata", placeholder: "key=value" },
  { key: "release", label: "Release", placeholder: "release" },
  { key: "version", label: "Version", placeholder: "version" },
];

const LIST_KEYS = FILTER_FIELDS.map((field) => field.key);

// The filter parameters of a page URL: the API's own names, a list repeated once per value.
export function filtersFromParams(params: URLSearchParams): ListFilters {
  const filters: ListFilters = {};
  for (const field of FILTER_FIELDS) {
    const values = params.getAll(field.key).filter((value) => value !== "");
    const allowed = field.values;
    const kept = allowed ? values.filter((value) => allowed.includes(value)) : values;
    if (kept.length > 0) Object.assign(filters, { [field.key]: [...new Set(kept)] });
  }
  const name = params.get("name");
  if (name) filters.name = name;
  return filters;
}

export function hasFilters(filters: ListFilters): boolean {
  return Object.keys(filters).length > 0;
}

// One condition of the filter, for the chips that show and remove it.
export interface Condition {
  key: keyof ListFilters;
  value: string;
  label: string;
}

export function conditions(filters: ListFilters): Condition[] {
  const out: Condition[] = [];
  for (const field of FILTER_FIELDS) {
    for (const value of filters[field.key] ?? []) {
      out.push({ key: field.key, value, label: `${field.label}: ${value}` });
    }
  }
  if (filters.name) {
    out.push({ key: "name", value: filters.name, label: `Name contains: ${filters.name}` });
  }
  return out;
}

export function withCondition(filters: ListFilters, key: ListField, value: string): ListFilters {
  const current: readonly string[] = filters[key] ?? [];
  if (current.includes(value)) return filters;
  return { ...filters, [key]: [...current, value] };
}

export function withoutCondition(filters: ListFilters, condition: Condition): ListFilters {
  const next: ListFilters = { ...filters };
  if (condition.key === "name") {
    delete next.name;
    return next;
  }
  const kept = ((filters[condition.key] ?? []) as readonly string[]).filter(
    (value) => value !== condition.value,
  );
  if (kept.length === 0) delete next[condition.key];
  else Object.assign(next, { [condition.key]: kept });
  return next;
}

// A stable form of the filter for query keys: equal filters give equal keys.
export function filterKey(filters: ListFilters): string {
  return JSON.stringify(conditions(filters).map((c) => [c.key, c.value]));
}

// Filters live in the URL, so a filtered list can be shared and Back restores it. The router applies
// replaceState in a transition, so a new value is held here until the URL has it.
export function useListFilters(): [ListFilters, (next: ListFilters) => void] {
  const params = useSearchParams();
  const fromUrl = filtersFromParams(params);
  const [chosen, setChosen] = useState<ListFilters | null>(null);
  if (chosen !== null && filterKey(chosen) === filterKey(fromUrl)) setChosen(null);

  const set = (next: ListFilters) => {
    setChosen(next);
    const search = new URLSearchParams(window.location.search);
    for (const key of [...LIST_KEYS, "name"]) search.delete(key);
    for (const condition of conditions(next)) search.append(condition.key, condition.value);
    window.history.replaceState(null, "", `?${search}`);
  };

  return [chosen ?? fromUrl, set];
}
