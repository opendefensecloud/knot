import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useCallback, useMemo, useState } from "react";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useSession } from "../../auth/SessionContext";
import { suggestionsApi, type SuggestionWithReplies } from "../../lib/suggestions.api";
import { useUi } from "../../stores/ui";
import { workspaceApi } from "../workspace/workspace.api";
import type { SuggestionLive } from "./liveState";
import { isOpenStatus, SuggestionCard } from "./SuggestionCard";
import { SuggestionComposer } from "./SuggestionComposer";

/** Pending cards in document order, by where their range starts now. Ones
 *  that don't resolve (or aren't classified yet) go last, oldest first. */
export function orderPending(
  pending: SuggestionWithReplies[],
  live: Record<string, SuggestionLive>,
): SuggestionWithReplies[] {
  const created = (s: SuggestionWithReplies) => new Date(s.created_at).getTime();
  return pending.slice().sort((a, b) => {
    const pa = live[a.id]?.from ?? null;
    const pb = live[b.id]?.from ?? null;
    if (pa !== null && pb !== null && pa !== pb) return pa - pb;
    if (pa === null && pb !== null) return 1;
    if (pa !== null && pb === null) return -1;
    return created(a) - created(b);
  });
}

/**
 * The Suggestions tab of the comment sidebar: the composer while a
 * suggestion is being made, pending cards in document order, and a
 * collapsed "Resolved" section. Viewers see the cards read-only.
 */
export function SuggestionsPanel({ docId }: { docId: string }) {
  const [showResolved, setShowResolved] = useState(false);
  const live = useUi((s) => s.suggestionLive);
  const pendingSuggestion = useUi((s) => s.pendingSuggestion);

  const { doc: role } = useEffectiveRole(docId);
  const session = useSession();
  const currentUserId = session.data && "ok" in session.data ? session.data.ok.user_id : "";

  const membersQuery = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
    staleTime: 60_000,
  });
  // Stable while the members don't change: the cards are memoized, and the
  // panel re-renders on every live update (each keystroke in the page).
  const memberMap = useMemo(
    () =>
      new Map(
        membersQuery.data && "ok" in membersQuery.data
          ? membersQuery.data.ok.map((m) => [m.user_id, m.display_name])
          : [],
      ),
    [membersQuery.data],
  );
  const nameOf = useCallback(
    (userId: string | null) => (userId ? memberMap.get(userId) : undefined) ?? "Unknown",
    [memberMap],
  );

  // The same key KnotEditor reads for the highlights, so both share one fetch.
  const pendingQuery = useQuery({
    queryKey: ["suggestions", docId, false],
    queryFn: () => suggestionsApi.list(docId, false),
  });
  const resolvedQuery = useQuery({
    queryKey: ["suggestions", docId, true],
    queryFn: () => suggestionsApi.list(docId, true),
    enabled: showResolved,
  });

  const pendingList = pendingQuery.data && "ok" in pendingQuery.data ? pendingQuery.data.ok : [];
  const pending = orderPending(pendingList.filter((s) => isOpenStatus(s.status)), live);
  const resolved = (resolvedQuery.data && "ok" in resolvedQuery.data ? resolvedQuery.data.ok : [])
    .filter((s) => !isOpenStatus(s.status))
    .sort((a, b) => new Date(b.decided_at ?? b.updated_at).getTime() - new Date(a.decided_at ?? a.updated_at).getTime());

  // A suggestion a link opened (a notification) may have been decided
  // since, and is only listed with the resolved ones: open those, once per
  // visit. Set while rendering, as CommentSidebar does for threads.
  const suggestionLink = useUi((s) => s.suggestionLink);
  const [handledLink, setHandledLink] = useState<typeof suggestionLink>(null);
  if (pendingQuery.isSuccess && suggestionLink && suggestionLink !== handledLink) {
    setHandledLink(suggestionLink);
    if (!showResolved && !pending.some((s) => s.id === suggestionLink.suggestionId)) {
      setShowResolved(true);
    }
  }

  const card = (s: SuggestionWithReplies) => (
    <SuggestionCard
      key={s.id}
      docId={docId}
      suggestion={s}
      role={role}
      currentUserId={currentUserId}
      nameOf={nameOf}
    />
  );

  return (
    <div className="flex-1 overflow-y-auto">
      <SuggestionComposer docId={docId} />

      {pendingQuery.isLoading && (
        <p className="px-4 py-3 text-fg-muted text-sm">Loading…</p>
      )}
      {!pendingQuery.isLoading && pending.length === 0 && !pendingSuggestion && (
        <p className="px-4 py-3 text-fg-muted text-sm">No suggestions yet.</p>
      )}
      {pending.map(card)}

      <section>
        <h3 className="m-0">
          <button
            type="button"
            data-testid="suggestion-show-resolved"
            aria-expanded={showResolved}
            onClick={() => setShowResolved((v) => !v)}
            className="w-full flex items-center gap-1.5 px-4 py-2 text-left text-[13px] font-medium text-fg-muted hover:text-fg hover:bg-muted/40 transition-colors"
          >
            {showResolved ? <ChevronDown size={14} aria-hidden /> : <ChevronRight size={14} aria-hidden />}
            <span>Resolved</span>
          </button>
        </h3>
        {showResolved && resolvedQuery.isLoading && (
          <p className="px-4 py-2 text-fg-muted text-sm">Loading…</p>
        )}
        {showResolved && resolvedQuery.isSuccess && resolved.length === 0 && (
          <p className="px-4 py-2 text-fg-muted text-sm">Nothing resolved yet.</p>
        )}
        {showResolved && resolved.map(card)}
      </section>
    </div>
  );
}
