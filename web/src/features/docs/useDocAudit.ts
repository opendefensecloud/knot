import { useInfiniteQuery } from "@tanstack/react-query";

import { unwrapResult } from "../notifications/useNotifications";

import { auditApi, type AuditCategory } from "./audit.api";

/**
 * A page's audit log, newest first, a page at a time (`fetchNextPage` loads
 * older events). The stats ride on the first page: `data.pages[0].stats`.
 * Fetched again whenever the page mounts, so it never shows an old log.
 * Errors carry `status` and `code` (see `unwrapResult`), so a 4xx is not
 * retried. Switching the filter keeps showing the previous list (and its
 * stats, which don't depend on the filter) until the new one arrives;
 * another page's log is never shown in its place.
 */
export function useDocAudit(docId: string, category: AuditCategory, enabled: boolean) {
  return useInfiniteQuery({
    queryKey: ["doc-audit", docId, category],
    queryFn: async ({ pageParam }) =>
      unwrapResult(await auditApi.list(docId, { category, cursor: pageParam })),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
    refetchOnMount: "always",
    staleTime: 0,
    placeholderData: (previousData, previousQuery) =>
      previousQuery?.queryKey[1] === docId ? previousData : undefined,
    enabled,
  });
}
