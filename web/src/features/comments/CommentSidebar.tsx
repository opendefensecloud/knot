import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { commentsApi, type Comment } from "../../lib/comments.api";
import { useUi } from "../../stores/ui";
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

export function CommentSidebar({ docId }: { docId: string }) {
  const [showResolved, setShowResolved] = useState(false);
  const closeCommentSidebar = useUi((s) => s.closeCommentSidebar);
  const pendingAnchor = useUi((s) => s.pendingAnchor);
  const clearPendingAnchor = useUi((s) => s.clearPendingAnchor);
  const notify = useUi((s) => s.notify);
  const qc = useQueryClient();

  const list = useQuery({
    queryKey: ["comments", docId, showResolved],
    queryFn: () => commentsApi.list(docId, showResolved),
  });

  const createThread = useMutation({
    mutationFn: (body: string) =>
      commentsApi.createThread(
        docId,
        body,
        pendingAnchor?.positionY ?? null,
        pendingAnchor?.anchorText ?? null,
      ),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't post comment"); return; }
      clearPendingAnchor();
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const comments: Comment[] = list.data && "ok" in list.data ? list.data.ok : [];
  const groups = groupByThread(comments);

  return (
    <div
      role="dialog"
      data-testid="comment-sidebar"
      style={{
        position: "fixed", top: 0, right: 0, bottom: 0,
        width: 400, maxWidth: "100vw",
        background: "white",
        borderLeft: "1px solid #e5e5e5",
        boxShadow: "-4px 0 12px rgba(0,0,0,0.1)",
        zIndex: 40,
        display: "flex", flexDirection: "column",
      }}
    >
      <header style={{ display: "flex", alignItems: "center", padding: 12, borderBottom: "1px solid #e5e5e5" }}>
        <h2 style={{ margin: 0, flex: 1, fontSize: 16 }}>Comments</h2>
        <label style={{ fontSize: 13, marginRight: 12, display: "flex", alignItems: "center", gap: 4, cursor: "pointer" }}>
          <input
            type="checkbox"
            data-testid="comment-show-resolved"
            checked={showResolved}
            onChange={(e) => setShowResolved(e.target.checked)}
          />
          Show resolved
        </label>
        <button
          type="button"
          data-testid="comment-sidebar-close"
          onClick={closeCommentSidebar}
          style={{ background: "none", border: "none", cursor: "pointer", fontSize: 16 }}
        >
          ✕
        </button>
      </header>

      {/* New thread composer — shown when a selection triggered the sidebar */}
      {pendingAnchor && (
        <div style={{ borderBottom: "1px solid #e5e5e5", background: "#f5f8ff" }}>
          <div style={{ padding: "8px 16px 0", fontSize: 12, color: "#555" }}>
            New comment on: <em>&ldquo;{pendingAnchor.anchorText}&rdquo;</em>
          </div>
          <CommentComposer
            placeholder="Start a new thread…"
            submitLabel="Comment"
            isPending={createThread.isPending}
            onSubmit={(body) => createThread.mutate(body)}
            data-testid-input="comment-composer-input-new"
            data-testid-submit="comment-composer-submit-new"
          />
        </div>
      )}

      {/* Thread list */}
      <div style={{ flex: 1, overflowY: "auto" }}>
        {list.isLoading && (
          <p style={{ padding: 16, color: "#888" }}>Loading…</p>
        )}
        {!list.isLoading && groups.length === 0 && !pendingAnchor && (
          <p style={{ padding: 16, color: "#888" }}>No comments yet.</p>
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
    </div>
  );
}
