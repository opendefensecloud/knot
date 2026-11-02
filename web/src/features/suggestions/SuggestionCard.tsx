import { useMutation, useQueryClient } from "@tanstack/react-query";
import type React from "react";
import { memo, useEffect, useRef, useState } from "react";

import type { Role } from "../../auth/useEffectiveRole";
import { Avatar } from "../../components/ui/Avatar";
import { relTime } from "../../lib/relTime";
import {
  suggestionsApi,
  type SuggestionKind,
  type SuggestionStatus,
  type SuggestionWithReplies,
} from "../../lib/suggestions.api";
import { useUi } from "../../stores/ui";
import { CommentComposer } from "../comments/CommentComposer";
import type { SuggestionLive } from "./liveState";

/** What a card's `data-state` says: the live state of a pending one
 *  (`unknown` until the editor has classified it), else its status. */
export type CardState = SuggestionLive["state"] | "unknown" | "accepted" | "rejected" | "withdrawn";

const KIND_LABEL: Record<SuggestionKind, string> = {
  replace: "Replace",
  insert: "Insert",
  delete: "Delete",
};

const STATUS_LABEL: Record<Exclude<SuggestionStatus, "pending" | "applying">, string> = {
  accepted: "Accepted",
  rejected: "Rejected",
  withdrawn: "Withdrawn",
};

const NOTE_CHANGED = "Changed since it was suggested";
const NOTE_REMOVED = "The text this suggestion was about was removed.";
const NOTE_CHANGED_WHILE_LOOKING = "The text changed while you were looking. Check it and accept again.";

/** `applying` is the server's short claim during an accept: still open. */
export function isOpenStatus(status: SuggestionStatus): status is "pending" | "applying" {
  return status === "pending" || status === "applying";
}

/** Whether `el` shows at least partly inside the window. */
function inViewport(el: Element): boolean {
  const r = el.getBoundingClientRect();
  return r.bottom > 0 && r.top < window.innerHeight && r.height > 0;
}

type Row = { label: string; text: string; tone: "old" | "now" | "new" };

/** The label/value rows of a card. "Now" only ever shows text the editor
 *  read from the page just now; without that (unknown, removed, resolved)
 *  the stored texts are shown under labels that claim nothing about the
 *  page, except that an unknown replace reads "Now" from what was stored. */
function rowsFor(s: SuggestionWithReplies, state: CardState, live: SuggestionLive | undefined): Row[] {
  const proposed: Row[] =
    s.kind === "insert"
      ? [{ label: "Add", text: s.proposed_text, tone: "new" }]
      : s.kind === "replace"
        ? [{ label: "Suggested", text: s.proposed_text, tone: "new" }]
        : [];

  if (state === "same" && live) {
    if (s.kind === "insert") return proposed;
    if (s.kind === "delete") return [{ label: "Remove", text: live.currentText, tone: "old" }];
    return [{ label: "Now", text: live.currentText, tone: "old" }, ...proposed];
  }
  if (state === "changed" && live) {
    return [
      { label: "When suggested", text: s.original_text, tone: "old" },
      { label: "Now", text: live.currentText, tone: "now" },
      ...proposed,
    ];
  }
  if (s.kind === "insert") return proposed;
  if (s.kind === "delete") return [{ label: "Remove", text: s.original_text, tone: "old" }];
  const before = state === "unknown" ? "Now" : "When suggested";
  return [{ label: before, text: s.original_text, tone: "old" }, ...proposed];
}

const TONE_CLASS: Record<Row["tone"], string> = {
  old: "line-through text-fg-muted",
  now: "text-fg",
  new: "text-fg font-medium",
};

interface Props {
  docId: string;
  suggestion: SuggestionWithReplies;
  /** The viewer's effective role on the doc; Owner decides, Editor+ replies. */
  role: Role | null;
  currentUserId: string;
  /** A member's display name, for the author, the decider and repliers. */
  nameOf: (userId: string | null) => string;
}

/**
 * One suggestion in the Suggestions tab. A pending card shows the change
 * against the page as the editor reads it now (`suggestionLive`), with
 * Accept / Reject for owners and Withdraw for its author; a resolved one
 * shows who decided and when.
 */
export const SuggestionCard = memo(function SuggestionCard({
  docId,
  suggestion: s,
  role,
  currentUserId,
  nameOf,
}: Props) {
  const qc = useQueryClient();
  const notify = useUi((st) => st.notify);
  const live = useUi((st) => st.suggestionLive[s.id]);
  const activeSuggestionId = useUi((st) => st.activeSuggestionId);
  const setActiveSuggestionId = useUi((st) => st.setActiveSuggestionId);
  // Set after a 409 `suggestion.changed`; cleared on the next attempt.
  const [changedWhileLooking, setChangedWhileLooking] = useState(false);

  // A decided suggestion's final status; null while it is still open.
  const decided = isOpenStatus(s.status) ? null : s.status;
  const open = decided === null;
  const state: CardState = decided ?? live?.state ?? "unknown";
  const isOwner = role === "owner";
  const canReply = open && (role === "owner" || role === "editor");
  const isAuthor = s.author_id === currentUserId;
  const isActive = activeSuggestionId === s.id;

  const invalidate = () => qc.invalidateQueries({ queryKey: ["suggestions", docId] });

  const accept = useMutation({
    mutationFn: (seenText: string) => suggestionsApi.accept(docId, s.id, seenText),
    onMutate: () => setChangedWhileLooking(false),
    onSuccess: async (r) => {
      if ("error" in r) {
        switch (r.error.code) {
          case "suggestion.changed":
            // The card re-reads the page and shows what is there now; the
            // owner checks it and accepts again. Never retried by itself.
            setChangedWhileLooking(true);
            break;
          case "suggestion.anchor_lost":
          case "suggestion.not_pending":
            // The refetch and reclassification tell the story: removed, or
            // decided by someone else.
            break;
          default:
            notify("error", "Couldn't accept suggestion");
        }
      }
      await invalidate();
    },
  });

  const reject = useMutation({
    mutationFn: () => suggestionsApi.reject(docId, s.id),
    onSuccess: async (r) => {
      if ("error" in r && r.error.code !== "suggestion.not_pending") {
        notify("error", "Couldn't reject suggestion");
      }
      await invalidate();
    },
  });

  const withdraw = useMutation({
    mutationFn: () => suggestionsApi.withdraw(docId, s.id),
    onSuccess: async (r) => {
      if ("error" in r && r.error.code !== "suggestion.not_pending") {
        notify("error", "Couldn't withdraw suggestion");
      }
      await invalidate();
    },
  });

  const reply = useMutation({
    mutationFn: (v: { body: string; mentions: string[] }) =>
      suggestionsApi.reply(docId, s.id, v.body, v.mentions),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't post reply"); return; }
      await invalidate();
    },
  });

  const cardRef = useRef<HTMLDivElement | null>(null);
  // Brought into view inside the sidebar when it becomes active, e.g. from
  // a click on its highlight or a notification link.
  useEffect(() => {
    if (isActive) cardRef.current?.scrollIntoView?.({ behavior: "smooth", block: "nearest" });
  }, [isActive]);

  // And its highlight in the page, once per activation, unless it already
  // shows (a click on the highlight itself). After a `?suggestion=` link the
  // highlight only exists once the editor has synced and classified the
  // suggestion, so this retries as `live` arrives, a frame after the
  // highlights were redrawn.
  const highlightShownRef = useRef(false);
  useEffect(() => {
    if (!isActive) {
      highlightShownRef.current = false;
      return;
    }
    if (!open || highlightShownRef.current) return;
    const frame = window.requestAnimationFrame(() => {
      const el = document.querySelector(`[data-suggestion-id="${s.id}"]`);
      if (!el) return;
      highlightShownRef.current = true;
      if (!inViewport(el)) el.scrollIntoView?.({ behavior: "smooth", block: "center" });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [isActive, open, live, s.id]);

  function focusCard() {
    setActiveSuggestionId(s.id);
    const el = document.querySelector(`[data-suggestion-id="${s.id}"]`);
    if (el) highlightShownRef.current = true;
    el?.scrollIntoView?.({ behavior: "smooth", block: "center" });
  }

  function handleKeyDown(e: React.KeyboardEvent<HTMLDivElement>) {
    // Only the card itself: keys typed in its reply box or on its buttons
    // keep their own meaning.
    if (e.target !== e.currentTarget) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      focusCard();
    }
  }

  const canAccept =
    isOwner && s.status === "pending" && live !== undefined && live.state !== "removed" && !accept.isPending;
  const busy = accept.isPending || reject.isPending || withdraw.isPending || s.status === "applying";

  const notes: string[] = [];
  if (open && state === "changed") notes.push(NOTE_CHANGED);
  if (open && state === "removed") notes.push(NOTE_REMOVED);
  if (open && changedWhileLooking) notes.push(NOTE_CHANGED_WHILE_LOOKING);

  const rows = rowsFor(s, state, live);
  const authorName = nameOf(s.author_id);
  const actionBtn =
    "text-[12px] px-2 h-7 rounded transition-colors disabled:opacity-40 disabled:cursor-not-allowed";

  return (
    <div
      ref={cardRef}
      data-testid={`suggestion-card-${s.id}`}
      data-state={state}
      data-active={isActive ? "true" : undefined}
      tabIndex={0}
      onClick={focusCard}
      onKeyDown={handleKeyDown}
      className={`border-l-4 border-b border-border px-4 py-3 cursor-pointer transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-600 ${
        open ? "border-l-blue-600" : "border-l-transparent"
      } ${
        !open
          ? "bg-muted/40 opacity-80"
          : isActive
            ? "bg-blue-600/10 ring-1 ring-blue-600/40"
            : "bg-surface hover:bg-muted/40"
      }`}
    >
      <div className="flex gap-2 items-center mb-2">
        <Avatar name={authorName} seed={s.author_id} size="sm" />
        <span className="font-semibold text-[13px] text-fg">{authorName}</span>
        <span className="text-fg-muted text-[12px]" title={new Date(s.created_at).toLocaleString()}>
          {relTime(s.created_at)}
        </span>
        <span className="ml-auto text-[11px] font-medium px-1.5 py-0.5 rounded bg-blue-600/10 text-blue-700 dark:text-blue-300">
          {KIND_LABEL[s.kind]}
        </span>
      </div>

      {s.kind === "insert" && s.context_before && (
        <p className="m-0 mb-1 text-[12px] text-fg-muted">
          after <em>&ldquo;…{s.context_before}&rdquo;</em>
        </p>
      )}

      <dl className="m-0 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        {rows.map((r) => (
          <div key={r.label} data-testid="suggestion-row" className="contents">
            <dt className="text-[12px] text-fg-muted pt-0.5">{r.label}</dt>
            <dd className={`m-0 whitespace-pre-wrap break-words ${TONE_CLASS[r.tone]}`}>{r.text}</dd>
          </div>
        ))}
      </dl>

      {s.reason && (
        <p className="m-0 mt-2 text-[13px] text-fg whitespace-pre-wrap">{s.reason}</p>
      )}

      {notes.length > 0 && (
        <div
          data-testid={`suggestion-note-${s.id}`}
          className="mt-2 px-2 py-1 rounded-sm bg-amber-500/10 text-[12px] text-fg"
        >
          {notes.map((n) => <p key={n} className="m-0">{n}</p>)}
        </div>
      )}

      {decided !== null && (
        <p className="m-0 mt-2 text-[12px] text-fg-muted">
          <span className="font-medium text-fg">{STATUS_LABEL[decided]}</span>
          {s.decided_by && <> by {nameOf(s.decided_by)}</>}
          {s.decided_at && <> · {relTime(s.decided_at)}</>}
        </p>
      )}

      {open && (
        <div className="mt-2 flex flex-wrap items-center gap-1" onClick={(e) => e.stopPropagation()}>
          {isOwner ? (
            <>
              <button
                type="button"
                data-testid={`suggestion-accept-${s.id}`}
                disabled={!canAccept || busy}
                onClick={() => {
                  if (!live) return;
                  setActiveSuggestionId(s.id);
                  accept.mutate(s.kind === "insert" ? "" : live.currentText);
                }}
                className={`${actionBtn} bg-blue-600 text-white font-medium hover:opacity-90`}
              >
                Accept
              </button>
              <button
                type="button"
                data-testid={`suggestion-reject-${s.id}`}
                disabled={busy}
                onClick={() => reject.mutate()}
                className={`${actionBtn} text-fg-muted hover:text-fg hover:bg-muted`}
              >
                Reject
              </button>
            </>
          ) : (
            <span data-testid={`suggestion-waiting-${s.id}`} className="text-[12px] text-fg-muted">
              Waiting for an owner
            </span>
          )}
          {isAuthor && (
            <button
              type="button"
              data-testid={`suggestion-withdraw-${s.id}`}
              disabled={busy}
              onClick={() => withdraw.mutate()}
              className={`${actionBtn} ml-auto text-fg-muted hover:text-fg hover:bg-muted`}
            >
              Withdraw
            </button>
          )}
        </div>
      )}

      {s.replies.length > 0 && (
        <div className="border-l-2 border-border pl-3 mt-3">
          {s.replies.map((r) => {
            const name = nameOf(r.author_id);
            return (
              <div key={r.id} data-testid={`suggestion-reply-${r.id}`} className="mb-2">
                <div className="flex gap-2 items-center mb-1">
                  <Avatar name={name} seed={r.author_id} size="sm" />
                  <span className="font-semibold text-[13px] text-fg">{name}</span>
                  <span className="text-fg-muted text-[12px]">{relTime(r.created_at)}</span>
                </div>
                <p className="m-0 text-sm text-fg whitespace-pre-wrap pl-7">{r.body}</p>
              </div>
            );
          })}
        </div>
      )}

      {canReply && (
        <div onClick={(e) => e.stopPropagation()} className="-mx-3">
          <CommentComposer
            placeholder="Reply…"
            submitLabel="Reply"
            isPending={reply.isPending}
            onSubmit={(body, mentions) => reply.mutate({ body, mentions })}
            data-testid-input={`suggestion-reply-input-${s.id}`}
            data-testid-submit={`suggestion-reply-submit-${s.id}`}
          />
        </div>
      )}
    </div>
  );
});
