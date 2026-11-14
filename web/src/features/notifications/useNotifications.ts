import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { unwrapResult } from "../../lib/api";
import { notificationsApi, type UnreadCount } from "../../lib/notifications.api";

/** Delivery is polling, not push — see the spec's §6. Window focus covers
 *  most of the perceived latency; this is the ceiling. */
export const POLL_INTERVAL_MS = 30_000;

// Moved to lib/api; re-exported so its importers keep this path.
export { unwrapResult };

export function formatBadge(count: number, capped: boolean): string {
  if (count <= 0) return "";
  return capped ? "99+" : String(count);
}

export function useUnreadCount() {
  return useQuery<UnreadCount>({
    queryKey: ["notifications", "unread_count"],
    queryFn: async () => unwrapResult(await notificationsApi.unreadCount()),
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: true,
  });
}

/** The inbox, newest first, a page at a time (`fetchNextPage` loads the
 *  next one). Fetched again whenever a list mounts, so opening the inbox
 *  never shows a list older than the badge next to it. */
export function useNotificationList(filter: "all" | "unread") {
  return useInfiniteQuery({
    queryKey: ["notifications", "list", filter],
    queryFn: async ({ pageParam }) => unwrapResult(await notificationsApi.list(filter, pageParam)),
    initialPageParam: undefined as number | undefined,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
    select: (data) => data.pages.flatMap((page) => page.items),
    refetchOnMount: "always",
  });
}

export function useMarkRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (ids: number[] | "all") => {
      unwrapResult(
        ids === "all" ? await notificationsApi.markAllRead() : await notificationsApi.markRead(ids),
      );
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["notifications"] });
    },
  });
}
