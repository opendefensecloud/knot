/**
 * What a change suggestion may target: plain text inside one text block (a
 * paragraph, heading, list item paragraph or table cell paragraph).
 *
 * Checked before anything is sent, so the selection float and the toolbar can
 * disable "Suggest change" with a reason instead of letting the server refuse
 * later. Mentions, dates and page links are `link` marks with `knot://`
 * hrefs; replacing part of one would break the chip, so a selection touching
 * one is refused, and so is a caret inside one. So is a selection over a
 * line break: a hard break, or a newline in a code block.
 */

import type { Mark } from "@tiptap/pm/model";
import type { EditorState } from "@tiptap/pm/state";

export type SuggestionScope =
  | { ok: true; kind: "replace" | "insert"; from: number; to: number; text: string; contextBefore: string }
  | { ok: false; reason: "cross_block" | "inline_atom" | "empty_block" };

type ScopeRefusal = Extract<SuggestionScope, { ok: false }>["reason"];

/** The disabled-button tooltip for each refusal. */
export const SCOPE_TOOLTIP: Record<ScopeRefusal, string> = {
  cross_block: "Select text inside a single paragraph to suggest a change",
  inline_atom: "Suggestions can't include mentions, dates, page links or line breaks",
  empty_block: "Type in the block instead",
};

/** How much of the block before the anchor a card quotes ("after …"). */
const CONTEXT_CHARS = 40;

const INTERNAL_HREF = /^knot:\/\/(doc|user|time)\//;

/** The knot:// link mark among `marks`, if any. */
function internalLink(marks: readonly Mark[]): Mark | undefined {
  return marks.find((m) => m.type.name === "link" && INTERNAL_HREF.test(String(m.attrs.href ?? "")));
}

/** Drops a low surrogate left at the front by cutting a pair in half. */
function trimSplitSurrogate(s: string): string {
  const first = s.charCodeAt(0);
  return first >= 0xdc00 && first <= 0xdfff ? s.slice(1) : s;
}

/** Classifies the current selection for a change suggestion. */
export function suggestionScope(state: EditorState): SuggestionScope {
  const { $from, $to, from, to } = state.selection;
  const block = $from.parent;
  if (block !== $to.parent || !block.isTextblock) {
    return { ok: false, reason: "cross_block" };
  }

  if (from === to) {
    if (block.content.size === 0) return { ok: false, reason: "empty_block" };
    // A caret strictly inside a chip: the same knot:// link on both sides.
    const before = $from.nodeBefore ? internalLink($from.nodeBefore.marks) : undefined;
    const after = $from.nodeAfter ? internalLink($from.nodeAfter.marks) : undefined;
    if (before && after && before.eq(after)) return { ok: false, reason: "inline_atom" };
  } else {
    let atom = false;
    state.doc.nodesBetween(from, to, (node) => {
      if (atom) return false;
      if (node.type.name === "hard_break" || (node.isInline && !node.isText)) atom = true;
      else if (node.isText && internalLink(node.marks)) atom = true;
      return !atom;
    });
    // A code block holds its lines as one text with line breaks in it.
    if (atom || /[\n\r]/.test(state.doc.textBetween(from, to))) return { ok: false, reason: "inline_atom" };
  }

  const blockStart = $from.start();
  const contextBefore = trimSplitSurrogate(
    state.doc.textBetween(Math.max(blockStart, from - CONTEXT_CHARS), from),
  );
  return {
    ok: true,
    kind: from === to ? "insert" : "replace",
    from,
    to,
    text: state.doc.textBetween(from, to),
    contextBefore,
  };
}
