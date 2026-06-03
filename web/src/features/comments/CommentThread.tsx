import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useSession } from "../../auth/SessionContext";
import { commentsApi, ALLOWED_EMOJIS, type Comment } from "../../lib/comments.api";
import { workspaceApi } from "../workspace/workspace.api";
import { useUi } from "../../stores/ui";
import { CommentComposer } from "./CommentComposer";

function relTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

// ---------------------------------------------------------------------------
// Reaction row
// ---------------------------------------------------------------------------

function ReactionRow({
  docId,
  comment,
  currentUserId,
}: {
  docId: string;
  comment: Comment;
  currentUserId: string;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [emojiPickerOpen, setEmojiPickerOpen] = useState(false);

  const addReaction = useMutation({
    mutationFn: (emoji: string) =>
      commentsApi.addReaction(docId, comment.id, emoji as (typeof ALLOWED_EMOJIS)[number]),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't add reaction"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const removeReaction = useMutation({
    mutationFn: (emoji: string) => commentsApi.removeReaction(docId, comment.id, emoji),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't remove reaction"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const existing = Object.entries(comment.reactions);

  return (
    <div style={{ display: "flex", flexWrap: "wrap", gap: 4, marginTop: 4 }}>
      {existing.map(([emoji, userIds]) => {
        const reacted = userIds.includes(currentUserId);
        return (
          <button
            key={emoji}
            type="button"
            data-testid={`comment-react-emoji-${comment.id}-${emoji}`}
            onClick={() => {
              if (reacted) {
                removeReaction.mutate(emoji);
              } else {
                addReaction.mutate(emoji);
              }
            }}
            style={{
              padding: "2px 8px",
              borderRadius: 12,
              border: reacted ? "1px solid #0050ff" : "1px solid #ddd",
              background: reacted ? "#e0e8ff" : "white",
              cursor: "pointer",
              fontSize: 13,
            }}
          >
            {emoji} {userIds.length}
          </button>
        );
      })}

      {/* Add reaction button */}
      <div style={{ position: "relative" }}>
        <button
          type="button"
          data-testid={`comment-react-add-${comment.id}`}
          onClick={() => setEmojiPickerOpen((o) => !o)}
          style={{
            padding: "2px 8px",
            borderRadius: 12,
            border: "1px solid #ddd",
            background: "white",
            cursor: "pointer",
            fontSize: 13,
          }}
        >
          +
        </button>
        {emojiPickerOpen && (
          <div
            style={{
              position: "absolute",
              top: "100%",
              left: 0,
              zIndex: 10,
              background: "white",
              border: "1px solid #ddd",
              borderRadius: 4,
              boxShadow: "0 4px 12px rgba(0,0,0,0.1)",
              display: "flex",
              gap: 4,
              padding: 4,
            }}
          >
            {ALLOWED_EMOJIS.map((emoji) => (
              <button
                key={emoji}
                type="button"
                onClick={() => {
                  setEmojiPickerOpen(false);
                  addReaction.mutate(emoji);
                }}
                style={{
                  fontSize: 18,
                  padding: 4,
                  background: "none",
                  border: "none",
                  cursor: "pointer",
                }}
              >
                {emoji}
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Single comment row
// ---------------------------------------------------------------------------

function CommentRow({
  docId,
  comment,
  currentUserId,
  authorName,
}: {
  docId: string;
  comment: Comment;
  currentUserId: string;
  authorName: string;
}) {
  return (
    <div
      data-testid={`comment-body-${comment.id}`}
      style={{ marginBottom: 12 }}
    >
      <div style={{ display: "flex", gap: 8, marginBottom: 4 }}>
        <span style={{ fontWeight: 600, fontSize: 13 }}>{authorName}</span>
        <span style={{ color: "#888", fontSize: 12 }}>{relTime(comment.created_at)}</span>
      </div>
      <p style={{ margin: 0, fontSize: 14, whiteSpace: "pre-wrap" }}>{comment.body}</p>
      <ReactionRow docId={docId} comment={comment} currentUserId={currentUserId} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Thread
// ---------------------------------------------------------------------------

interface Props {
  docId: string;
  threadId: string;
  root: Comment;
  replies: Comment[];
}

export function CommentThread({ docId, threadId, root, replies }: Props) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const session = useSession();
  const sessionUser = session.data && "ok" in session.data ? session.data.ok : null;
  const currentUserId = sessionUser?.user_id ?? "";

  const { doc: docRole } = useEffectiveRole(docId);
  const canManage = docRole === "owner" || docRole === "editor";
  const isResolved = root.resolved_at !== null;

  const membersQuery = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
    staleTime: 60_000,
  });
  const memberMap = new Map(
    membersQuery.data && "ok" in membersQuery.data
      ? membersQuery.data.ok.map((m) => [m.user_id, m.display_name])
      : [],
  );

  function authorName(userId: string): string {
    return memberMap.get(userId) ?? "Unknown";
  }

  const resolve = useMutation({
    mutationFn: () => commentsApi.resolve(docId, threadId),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't resolve thread"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const unresolve = useMutation({
    mutationFn: () => commentsApi.unresolve(docId, threadId),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't unresolve thread"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const replyMutation = useMutation({
    mutationFn: (body: string) => commentsApi.reply(docId, threadId, body),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't post reply"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  return (
    <div
      data-testid={`comment-thread-${threadId}`}
      style={{
        borderBottom: "1px solid #f0f0f0",
        padding: "12px 16px",
        background: isResolved ? "#fafafa" : "white",
        opacity: isResolved ? 0.8 : 1,
      }}
    >
      {/* Thread header */}
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-start", marginBottom: 8 }}>
        {root.anchor_text && (
          <blockquote
            style={{
              margin: "0 0 8px",
              padding: "4px 8px",
              borderLeft: "3px solid #0050ff",
              background: "#f0f4ff",
              fontSize: 12,
              color: "#444",
            }}
          >
            {root.anchor_text}
          </blockquote>
        )}
        {canManage && (
          isResolved ? (
            <button
              type="button"
              data-testid={`comment-unresolve-${threadId}`}
              onClick={() => unresolve.mutate()}
              disabled={unresolve.isPending}
              style={{ fontSize: 12, padding: "2px 8px", cursor: "pointer" }}
            >
              Unresolve
            </button>
          ) : (
            <button
              type="button"
              data-testid={`comment-resolve-${threadId}`}
              onClick={() => resolve.mutate()}
              disabled={resolve.isPending}
              style={{ fontSize: 12, padding: "2px 8px", cursor: "pointer" }}
            >
              Resolve
            </button>
          )
        )}
      </div>

      {/* Root comment */}
      <CommentRow
        docId={docId}
        comment={root}
        currentUserId={currentUserId}
        authorName={authorName(root.author_id)}
      />

      {/* Replies */}
      {replies.length > 0 && (
        <div style={{ borderLeft: "2px solid #eee", paddingLeft: 12, marginTop: 8 }}>
          {replies.map((r) => (
            <CommentRow
              key={r.id}
              docId={docId}
              comment={r}
              currentUserId={currentUserId}
              authorName={authorName(r.author_id)}
            />
          ))}
        </div>
      )}

      {/* Reply composer — hidden when resolved */}
      {!isResolved && (
        <CommentComposer
          placeholder="Reply…"
          submitLabel="Reply"
          isPending={replyMutation.isPending}
          onSubmit={(body) => replyMutation.mutate(body)}
          data-testid-input={`comment-composer-input-reply-${threadId}`}
          data-testid-submit={`comment-composer-submit-reply-${threadId}`}
        />
      )}
    </div>
  );
}
