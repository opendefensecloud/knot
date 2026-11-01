import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import type { ApiError } from "../../lib/api";
import type { WhatsNew } from "../../lib/validators";

import { aboutApi } from "./about.api";

const WHATS_NEW_KEY = ["whats-new"];

/** The running build and what this user hasn't seen of it. One query
 *  shared by the sidebar, the upgrade card and the About page. */
export function useWhatsNew() {
  return useQuery({ queryKey: WHATS_NEW_KEY, queryFn: () => aboutApi.whatsNew() });
}

/**
 * Marks the running version seen. Returns a stable function.
 *
 * Optimistic: the upgrade card goes the moment it is dismissed instead of
 * waiting on the server. The refetch afterwards puts it back if the server
 * did not record it.
 */
export function useMarkSeen(): () => void {
  const qc = useQueryClient();
  const { mutate } = useMutation({
    mutationFn: () => aboutApi.markSeen(),
    onMutate: async () => {
      await qc.cancelQueries({ queryKey: WHATS_NEW_KEY });
      qc.setQueryData<{ ok: WhatsNew } | { error: ApiError }>(WHATS_NEW_KEY, (prev) =>
        prev && "ok" in prev ? { ok: { ...prev.ok, unseen: [] } } : prev,
      );
    },
    onSettled: () => qc.invalidateQueries({ queryKey: WHATS_NEW_KEY }),
  });
  return mutate;
}
