import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useState } from "react";

import { IconButton } from "../../components/ui/IconButton";
import { commentsApi, type Comment } from "../../lib/comments.api";
import { suggestionsApi } from "../../lib/suggestions.api";
import { notifyFailure } from "../../lib/limitErrors";
import { useUi, type SidebarTab } from "../../stores/ui";
import { isOpenStatus } from "../suggestions/SuggestionCard";
import { SuggestionsPanel } from "../suggestions/SuggestionsPanel";
import { CommentComposer } from "./CommentComposer";
import { CommentThread } from "./CommentThread";

type ThreadGroup = {
  threadId: string;
  root: Comment;
  replies: Comment[];
  resolvedAt: string | null;
};

function groupByThread(comments: Comment[]): ThreadGroup[] {
  const roots = new Map<string, Comment>();
  const repliesMap = new Map<string, Comment[]>();

  for (const c of comments) {
    if (c.parent_id === null) {
      roots.set(c.thread_id, c);
    } else {
      const list = repliesMap.get(c.thread_id) ?? [];
      list.push(c);
      repliesMap.set(c.thread_id, list);
    }
  }

  const groups: ThreadGroup[] = [];
  for (const [threadId, root] of roots) {
    const replies = (repliesMap.get(threadId) ?? []).slice().sort(
      (a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime(),
    );
    groups.push({ threadId, root, replies, resolvedAt: root.resolved_at });
  }

  // Active threads first, then resolved; within each bucket sort by creation time
  groups.sort((a, b) => {
    const aResolved = a.resolvedAt !== null;
    const bResolved = b.resolvedAt !== null;
    if (aResolved !== bResolved) return aResolved ? 1 : -1;
    return new Date(a.root.created_at).getTime() - new Date(b.root.created_at).getTime();
  });

  return groups;
}

/** One header tab: its label and count, pressed while it shows. */
function SidebarTabButton({
  tab,
  label,
  count,
  active,
  onSelect,
}: {
  tab: SidebarTab;
  label: string;
  count: number;
  active: boolean;
  onSelect: (t: SidebarTab) => void;
}) {
  return (
    <button
      type="button"
      data-testid={`sidebar-tab-${tab}`}
      aria-pressed={active}
      onClick={() => onSelect(tab)}
      className={`inline-flex items-center gap-1.5 h-8 px-2.5 rounded text-sm font-semibold transition-colors ${
        active ? "bg-muted text-fg" : "text-fg-muted hover:text-fg hover:bg-muted/60"
      }`}
    >
      {label}{" "}
      <span className="text-[12px] font-medium tabular-nums text-fg-muted">{count}</span>
    </button>
  );
}

export function CommentSidebar({ docId }: { docId: string }) {
  const [showResolved, setShowResolved] = useState(false);
  const closeCommentSidebar = useUi((s) => s.closeCommentSidebar);
  const setActiveCommentId = useUi((s) => s.setActiveCommentId);
  const pendingAnchor = useUi((s) => s.pendingAnchor);
  const clearPendingAnchor = useUi((s) => s.clearPendingAnchor);
  const notify = useUi((s) => s.notify);
  const sidebarTab = useUi((s) => s.sidebarTab);
  const setSidebarTab = useUi((s) => s.setSidebarTab);
  const qc = useQueryClient();

  const list = useQuery({
    queryKey: ["comments", docId, showResolved],
    queryFn: () => commentsApi.list(docId, showResolved),
  });

  const createThread = useMutation({
    mutationFn: (v: { body: string; mentions: string[] }) =>
      commentsApi.createThread(
        docId,
        v.body,
        pendingAnchor?.positionY ?? null,
        pendingAnchor?.positionYEnd ?? null,
        pendingAnchor?.anchorText ?? null,
        v.mentions,
      ),
    onSuccess: async (r) => {
      if ("error" in r) { notifyFailure(notify, r.error, "Couldn't post comment"); return; }
      clearPendingAnchor();
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  // The pending suggestions, for the tab's count. The same key the panel and
  // KnotEditor read, so this is one shared fetch.
  const pendingSuggestions = useQuery({
    queryKey: ["suggestions", docId, false],
    queryFn: () => suggestionsApi.list(docId, false),
  });

  const comments: Comment[] = list.data && "ok" in list.data ? list.data.ok : [];
  const groups = groupByThread(comments);
  const openThreadCount = groups.filter((g) => g.resolvedAt === null).length;
  const pendingSuggestionCount =
    pendingSuggestions.data && "ok" in pendingSuggestions.data
      ? pendingSuggestions.data.ok.filter((s) => isOpenStatus(s.status)).length
      : 0;

  // A thread a link opened (a notification) may have been resolved since,
  // and is only listed with the resolved ones: show those, once per visit.
  // Set while rendering, not in an effect: it settles in one extra render.
  const threadLink = useUi((s) => s.threadLink);
  const [handledLink, setHandledLink] = useState<typeof threadLink>(null);
  if (list.isSuccess && threadLink && threadLink !== handledLink) {
    setHandledLink(threadLink);
    if (!showResolved && !groups.some((g) => g.threadId === threadLink.threadId)) {
      setShowResolved(true);
    }
  }

  return (
    <div
      role="dialog"
      data-testid="comment-sidebar"
      className="fixed right-0 top-0 h-dvh w-[400px] max-w-full z-40 bg-surface border-l border-border shadow-xl flex flex-col"
    >
      <header className="flex items-center gap-2 px-4 py-3 border-b border-border">
        <div className="flex-1 flex items-center gap-1">
          <SidebarTabButton
            tab="comments"
            label="Comments"
            count={openThreadCount}
            active={sidebarTab === "comments"}
            onSelect={setSidebarTab}
          />
          <SidebarTabButton
            tab="suggestions"
            label="Suggestions"
            count={pendingSuggestionCount}
            active={sidebarTab === "suggestions"}
            onSelect={setSidebarTab}
          />
        </div>
        {sidebarTab === "comments" && (
          <label className="text-[13px] text-fg-muted flex items-center gap-1.5 cursor-pointer select-none">
            <input
              type="checkbox"
              data-testid="comment-show-resolved"
              checked={showResolved}
              onChange={(e) => setShowResolved(e.target.checked)}
              className="accent-accent"
            />
            Show resolved
          </label>
        )}
        <IconButton
          data-testid="comment-sidebar-close"
          label="Close"
          size="sm"
          onClick={() => { setActiveCommentId(null); closeCommentSidebar(); }}
        >
          <X size={14} aria-hidden />
        </IconButton>
      </header>

      {sidebarTab === "suggestions" ? (
        <SuggestionsPanel docId={docId} />
      ) : (
        <>
          {pendingAnchor && (
            <div className="border-b border-border bg-accent/5">
              <div className="px-4 pt-2 text-[12px] text-fg-muted">
                New comment on: <em>&ldquo;{pendingAnchor.anchorText}&rdquo;</em>
              </div>
              <CommentComposer
                placeholder="Start a new thread…"
                submitLabel="Comment"
                isPending={createThread.isPending}
                onSubmit={(body, mentions) => createThread.mutate({ body, mentions })}
                data-testid-input="comment-composer-input-new"
                data-testid-submit="comment-composer-submit-new"
              />
            </div>
          )}

          <div className="flex-1 overflow-y-auto">
            {list.isLoading && (
              <p className="px-4 py-3 text-fg-muted text-sm">Loading…</p>
            )}
            {!list.isLoading && groups.length === 0 && !pendingAnchor && (
              <p className="px-4 py-3 text-fg-muted text-sm">No comments yet.</p>
            )}
            {groups.map((g) => (
              <CommentThread
                key={g.threadId}
                docId={docId}
                threadId={g.threadId}
                root={g.root}
                replies={g.replies}
              />
            ))}
          </div>
        </>
      )}
    </div>
  );
}
