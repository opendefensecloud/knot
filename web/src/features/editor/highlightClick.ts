/**
 * Which highlight a click in the editor lands on, shared by the comment and
 * suggestion highlight plugins.
 *
 * Both plugins see every click and the comment plugin runs first, so each
 * one checking `closest()` for its own attribute alone would let a comment
 * swallow every click on text that also carries a suggestion. Both ask this
 * helper instead and only handle the click when the answer is theirs.
 *
 * - The nearest element carrying either id wins, so a suggestion span inside
 *   a node-decorated comment (or the reverse) goes to the inner one.
 * - ProseMirror merges overlapping inline decorations into a single span, so
 *   text under both a comment and a suggestion is one element with both ids.
 *   Then the highlight covering less text wins, being the more specific one;
 *   a tie goes to the suggestion.
 */

export type HighlightHit = { kind: "comment" | "suggestion"; id: string };

export function highlightAt(root: HTMLElement, target: EventTarget | null): HighlightHit | null {
  const el = (target as Element | null)?.closest?.<HTMLElement>("[data-comment-id], [data-suggestion-id]");
  if (!el) return null;
  const commentId = el.dataset.commentId;
  const suggestionId = el.dataset.suggestionId;
  if (commentId && suggestionId) {
    const comment = extent(root, "data-comment-id", commentId);
    const suggestion = extent(root, "data-suggestion-id", suggestionId);
    return comment < suggestion ? { kind: "comment", id: commentId } : { kind: "suggestion", id: suggestionId };
  }
  if (suggestionId) return { kind: "suggestion", id: suggestionId };
  if (commentId) return { kind: "comment", id: commentId };
  return null;
}

/** The text length of every element in `root` carrying `attr="id"`. */
function extent(root: HTMLElement, attr: string, id: string): number {
  let length = 0;
  for (const el of root.querySelectorAll(`[${attr}]`)) {
    if (el.getAttribute(attr) === id) length += el.textContent?.length ?? 0;
  }
  return length;
}
