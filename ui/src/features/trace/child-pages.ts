"use client";

import { useQueries } from "@tanstack/react-query";
import { listSpanChildren } from "@/api/client";
import type { ChildLoad } from "./trace-tree";

// The pages of children requested for each span, as the cursors that fetch them; null is the first page.
export type PageRequests = ReadonlyMap<string, readonly (string | null)[]>;

export function withFirstPage(requests: PageRequests, parent: string): PageRequests {
  return requests.has(parent) ? requests : new Map(requests).set(parent, [null]);
}

export function withNextPage(requests: PageRequests, parent: string, cursor: string): PageRequests {
  const cursors = requests.get(parent) ?? [null];
  return cursors.includes(cursor) ? requests : new Map(requests).set(parent, [...cursors, cursor]);
}

export function withRestart(requests: PageRequests, parent: string): PageRequests {
  return new Map(requests).set(parent, [null]);
}

export function childPagesKey(project: string, trace: string, parent?: string) {
  const key = [project, "trace", trace, "children"];
  return parent === undefined ? key : [...key, parent];
}

export interface ChildPagesResult {
  pages: ReadonlyMap<string, readonly ChildLoad[]>;
  retry: (parent: string) => void;
}

export function useChildPages(
  project: string,
  trace: string,
  requests: PageRequests,
): ChildPagesResult {
  const entries = [...requests].flatMap(([parent, cursors]) =>
    cursors.map((cursor) => ({ parent, cursor })),
  );
  const results = useQueries({
    queries: entries.map(({ parent, cursor }) => ({
      queryKey: [...childPagesKey(project, trace, parent), cursor],
      queryFn: ({ signal }: { signal: AbortSignal }) =>
        listSpanChildren(project, trace, parent, cursor, signal),
      // A page is fixed by the `as_of` its cursor carries, so it never goes stale.
      staleTime: Number.POSITIVE_INFINITY,
    })),
  });

  const pages = new Map<string, ChildLoad[]>();
  const refetchLast = new Map<string, () => void>();
  entries.forEach(({ parent }, index) => {
    const result = results[index];
    if (!result) return;
    let load: ChildLoad;
    if (result.data !== undefined) load = { status: "success", page: result.data };
    else if (result.isError) load = { status: "error", error: result.error };
    else load = { status: "pending" };
    const loads = pages.get(parent);
    if (loads) loads.push(load);
    else pages.set(parent, [load]);
    refetchLast.set(parent, () => void result.refetch());
  });
  return { pages, retry: (parent) => refetchLast.get(parent)?.() };
}
