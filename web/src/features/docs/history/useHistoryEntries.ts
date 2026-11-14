import { useInfiniteQuery } from "@tanstack/react-query";
import { useMemo } from "react";

import { unwrapResult } from "../../../lib/api";
import { historyApi } from "../../../lib/history.api";
import type { HistoryEntry, HistoryPage } from "../../../lib/validators";
import { dedupeEntries } from "./timeline";

/** The poll while nothing is listed yet: a fresh page's first session
 *  should appear soon after it is written. */
const EMPTY_POLL_MS = 5_000;
/** The poll once entries are listed, also while the page catches up. */
const POLL_MS = 15_000;

/**
 * The page's timeline, a keyset page at a time, polled for as long as the
 * caller is mounted (the drawer is open), whatever the entries' state: an
 * open session grows, a new one starts, a sealed one gets its counts.
 * Every poll refetches each loaded page from the first, recomputing its
 * cursor, so a session whose end moved above a cursor already used comes
 * back on an earlier page; `entries` holds each id once.
 */
export function useHistoryEntries(docId: string) {
  const query = useInfiniteQuery({
    queryKey: ["history", docId, "entries"],
    // An HTTP error throws (`unwrapResult`), so it sets `isError` and never
    // becomes a page.
    queryFn: async ({ pageParam }) => unwrapResult(await historyApi.entries(docId, pageParam)),
    initialPageParam: null as string | null,
    getNextPageParam: (p: HistoryPage) => p.next_cursor ?? undefined,
    refetchInterval: (q) => {
      const pages = q.state.data?.pages ?? [];
      return pages.some((p) => p.entries.length > 0) ? POLL_MS : EMPTY_POLL_MS;
    },
  });

  const pages = query.data?.pages;
  const entries: HistoryEntry[] = useMemo(() => dedupeEntries(pages ?? []), [pages]);

  return {
    entries,
    first: pages?.[0] ?? null,
    isLoading: query.isLoading,
    isError: query.isError,
    hasData: query.data !== undefined,
    /** When the last fetch succeeded, also when it brought nothing new
     *  (structural sharing then keeps `entries` the same object). */
    dataUpdatedAt: query.dataUpdatedAt,
    refetch: query.refetch,
    fetchNextPage: query.fetchNextPage,
    hasNextPage: query.hasNextPage,
    isFetchingNextPage: query.isFetchingNextPage,
  };
}
