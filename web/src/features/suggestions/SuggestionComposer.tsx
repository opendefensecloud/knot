import { useMutation, useQueryClient } from "@tanstack/react-query";
import type React from "react";
import { useState } from "react";

import { suggestionsApi, type NewSuggestionBody, type SuggestionKind } from "../../lib/suggestions.api";
import { notifyFailure } from "../../lib/limitErrors";
import { useUi, type PendingSuggestion } from "../../stores/ui";

/** The server's limits, in characters (see the suggestions migration). */
const MAX_PROPOSED = 10_000;
const MAX_REASON = 4_096;

/** What the composer says when the server refuses a suggestion; the form
 *  stays open with what was typed. */
export function createErrorMessage(code: string): string {
  switch (code) {
    case "suggestion.too_long":
    case "suggestion.body_too_large":
      return "Suggestion is too long";
    case "suggestion.invalid_anchor":
      return "Couldn't suggest a change here.";
    case "doc.archived":
      return "This page is archived and can't take suggestions";
    case "acl.editor_required":
      return "You can't suggest changes on this page";
    default:
      return "Couldn't send suggestion";
  }
}

/** A suggestion replaces text inside one block, so its text is one line:
 *  every line break, from a paste or anywhere else, becomes a space. */
export function toSingleLine(text: string): string {
  return text.replace(/\r\n|\r|\n/g, " ");
}

/** The kind the server expects for these texts: a caret is an insertion,
 *  an emptied range a deletion, anything else a replacement. */
export function deriveKind(pending: PendingSuggestion, replacement: string): SuggestionKind {
  if (pending.kind === "insert") return "insert";
  return replacement === "" ? "delete" : "replace";
}

/**
 * The composer at the top of the Suggestions tab, shown while
 * `pendingSuggestion` is set. The page itself never changes here: the
 * suggestion is sent beside it, and an owner applies it on accept.
 */
export function SuggestionComposer({ docId }: { docId: string }) {
  const pending = useUi((s) => s.pendingSuggestion);
  if (!pending) return null;
  // Keyed by the anchors, so a new selection starts a fresh form.
  return <ComposerForm key={`${pending.positionY}:${pending.positionYEnd}`} docId={docId} pending={pending} />;
}

function ComposerForm({ docId, pending }: { docId: string; pending: PendingSuggestion }) {
  const clearPendingSuggestion = useUi((s) => s.clearPendingSuggestion);
  const notify = useUi((s) => s.notify);
  const qc = useQueryClient();
  const [replacement, setReplacement] = useState(pending.originalText);
  const [reason, setReason] = useState("");
  const isInsert = pending.kind === "insert";

  const create = useMutation({
    mutationFn: (body: NewSuggestionBody) => suggestionsApi.create(docId, body),
    onSuccess: async (r) => {
      if ("error" in r) { notifyFailure(notify, r.error, createErrorMessage(r.error.code)); return; }
      clearPendingSuggestion();
      await qc.invalidateQueries({ queryKey: ["suggestions", docId] });
    },
  });

  const unchanged = replacement === pending.originalText;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (unchanged || create.isPending) return;
    const body: NewSuggestionBody = {
      kind: deriveKind(pending, replacement),
      position_y: pending.positionY,
      position_y_end: pending.positionYEnd,
      original_text: pending.originalText,
      context_before: pending.contextBefore,
      proposed_text: replacement,
    };
    const why = reason.trim();
    if (why) body.reason = why;
    create.mutate(body);
  }

  // A text input drops line breaks from pasted text without a trace, which
  // would glue words together: paste them as spaces instead.
  function handlePaste(e: React.ClipboardEvent<HTMLInputElement>) {
    const text = e.clipboardData.getData("text/plain");
    if (!/[\r\n]/.test(text)) return;
    e.preventDefault();
    const input = e.currentTarget;
    const start = input.selectionStart ?? replacement.length;
    const end = input.selectionEnd ?? start;
    const pasted = toSingleLine(text);
    const next = (replacement.slice(0, start) + pasted + replacement.slice(end)).slice(0, MAX_PROPOSED);
    setReplacement(next);
    const caret = Math.min(start + pasted.length, next.length);
    window.requestAnimationFrame(() => input.setSelectionRange(caret, caret));
  }

  const fieldLabel = "block text-[12px] font-medium text-fg-muted mb-1";
  const fieldInput =
    "w-full px-2 py-1.5 text-sm rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-blue-600";

  return (
    <form
      data-testid="suggestion-composer"
      onSubmit={handleSubmit}
      className="border-b border-border bg-blue-600/5 px-4 py-3 flex flex-col gap-3"
    >
      <h3 className="m-0 text-sm font-semibold text-fg">Suggest a change</h3>

      {!isInsert && (
        <div>
          <span className={fieldLabel}>Current text</span>
          <p className="m-0 text-sm text-fg-muted line-through whitespace-pre-wrap break-words">
            {pending.originalText}
          </p>
        </div>
      )}

      <label className="block">
        <span className={fieldLabel}>{isInsert ? "Insert" : "Replace with"}</span>
        <input
          type="text"
          data-testid="suggestion-composer-replace"
          value={replacement}
          maxLength={MAX_PROPOSED}
          autoFocus
          onChange={(e) => setReplacement(toSingleLine(e.target.value))}
          onPaste={handlePaste}
          className={fieldInput}
        />
      </label>

      <label className="block">
        <span className={fieldLabel}>Why? (optional)</span>
        <textarea
          data-testid="suggestion-composer-reason"
          value={reason}
          maxLength={MAX_REASON}
          rows={2}
          onChange={(e) => setReason(e.target.value)}
          className={`${fieldInput} resize-y`}
        />
      </label>

      <p className="m-0 text-[12px] text-fg-muted">The page stays as it is until an owner accepts.</p>

      <div className="flex justify-end gap-2">
        <button
          type="button"
          data-testid="suggestion-composer-cancel"
          onClick={clearPendingSuggestion}
          className="inline-flex items-center h-8 px-3 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors"
        >
          Cancel
        </button>
        <button
          type="submit"
          data-testid="suggestion-composer-submit"
          disabled={unchanged || create.isPending}
          className="inline-flex items-center h-8 px-3 rounded bg-blue-600 text-white text-[13px] font-medium hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed"
        >
          Send suggestion
        </button>
      </div>
    </form>
  );
}
