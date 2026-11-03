/**
 * Live state of a pending suggestion, worked out against the editor's
 * current document.
 *
 *   - same:    the anchored text still reads as it did when suggested;
 *   - changed: the anchors resolve inside one text block, but the text
 *              between them differs (the card shows both versions);
 *   - removed: the anchors no longer resolve, the range collapsed, or it no
 *              longer lies inside one text node (a line break now splits
 *              it), or the character an insertion is tied to was deleted.
 *              Accept is disabled.
 *
 * The server repeats this decision on accept and refuses rather than guess,
 * so this only has to be right about what the card shows.
 */

import type { Editor } from "@tiptap/core";
import type * as Y from "yjs";

import { decodeSuggestionRange } from "./anchor";

/** `from` lets the sidebar order cards by document position (null when the range doesn't resolve). */
export type SuggestionLive = { state: "same" | "changed" | "removed"; currentText: string; from: number | null };

/** The fields of a suggestion `classifySuggestion` reads. */
export type ClassifiableSuggestion = {
  kind: "replace" | "insert" | "delete";
  position_y: string;
  position_y_end: string;
  original_text: string;
};

/** Classifies one suggestion against the editor's current document. */
export function classifySuggestion(editor: Editor, ydoc: Y.Doc, s: ClassifiableSuggestion): SuggestionLive {
  const range = decodeSuggestionRange(editor, ydoc, s.position_y, s.position_y_end);
  if (!range) return { state: "removed", currentText: "", from: null };

  const { from, to } = range;
  const removed: SuggestionLive = { state: "removed", currentText: "", from };
  const doc = editor.state.doc;
  const $from = doc.resolve(from);
  // The decoder already returns null for an anchor inside a deleted block or
  // tied to anything but text; this guards the textblock the server applies
  // in. Checked for every kind.
  if (!$from.parent.isTextblock) return removed;
  // Resolves, but the server would refuse it as lost (see SuggestionRange).
  if (range.anchorLost) return removed;

  if (s.kind === "insert") return { state: "same", currentText: "", from };

  if (from >= to || doc.resolve(to).parent !== $from.parent) return removed;

  const currentText = doc.textBetween(from, to);
  return { state: currentText === s.original_text ? "same" : "changed", currentText, from };
}
