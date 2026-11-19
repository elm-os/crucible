"use client";

import { type InfiniteData, useInfiniteQuery, useQueryClient } from "@tanstack/react-query";
import type { Meta } from "@/api/generated/Meta";
import type { TimeWindow } from "@/api/types";
import { type Range, rangeWindow } from "./time-range";

interface Paged<T> {
  data: T[];
  next_cursor: string | null;
  meta: Meta;
}

export interface Fetched<P> {
  page: P;
  window: TimeWindow;
}

type Position = { cursor: string; window: TimeWindow } | null;

export type PagedData<P> = InfiniteData<Fetched<P>, unknown>;

// One list paged on `next_cursor`. A cursor is bound to the window of its first page, so every later
// page reuses that window instead of reading the clock again; a refetch starts from a new first page.
export function usePagedList<T, P extends Paged<T>>(
  queryKey: readonly unknown[],
  range: Range,
  fetchPage: (window: TimeWindow, cursor: string | null, signal: AbortSignal) => Promise<P>,
) {
  const queryClient = useQueryClient();
  const query = useInfiniteQuery({
    queryKey,
    initialPageParam: null as Position,
    queryFn: async ({ pageParam, signal }): Promise<Fetched<P>> => {
      const window = pageParam?.window ?? rangeWindow(range, Date.now());
      return { page: await fetchPage(window, pageParam?.cursor ?? null, signal), window };
    },
    getNextPageParam: (last): Position =>
      last.page.next_cursor === null
        ? null
        : { cursor: last.page.next_cursor, window: last.window },
  });
  // After `cursor_invalid` the list starts over from one fresh first page.
  const restart = () => queryClient.resetQueries({ queryKey, exact: true });
  return { query, restart };
}

export function rowsOf<T, P extends Paged<T>>(data: PagedData<P>): T[] {
  return data.pages.flatMap((fetched) => fetched.page.data);
}

// The newest page's meta speaks for the list: it was read last.
export function metaOf<P extends Paged<unknown>>(data: PagedData<P>): Meta | undefined {
  return data.pages.at(-1)?.page.meta;
}
