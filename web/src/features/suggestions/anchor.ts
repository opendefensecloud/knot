/**
 * Y.RelativePosition anchors for change suggestions.
 *
 * Same storage format as comment anchors (base64 `Y.RelativePosition` v1
 * bytes), but a different encoding. y-tiptap's
 * `absolutePositionToRelativePosition` ties every position to the character
 * before it (assoc -1), so for a range the start would sit on the character
 * *before* the selection and text typed there would fall inside the range —
 * and be deleted on accept. Here:
 *
 *   - the start ties to the first selected character (assoc 0),
 *   - the end ties to the last selected character (assoc -1),
 *   - an insertion point ties both anchors to the character before it
 *     (assoc -1), the same as a comment anchor.
 *
 * The server resolves exactly these bytes (`knot-crdt` `suggestions.rs`), and
 * a test pins them against its fixture.
 *
 * Known limitation: because the end ties to the last selected character and
 * Yjs places text typed after a deleted character to the right of its
 * tombstone, retyping the end of the range shrinks it to what is left. The
 * card then shows that remainder as "Now". This is accepted.
 *
 * Known limitation: typing directly before the range a character equal to the
 * range's first character can still land inside it. y-tiptap's `updateYText`
 * diffs old and new text with lib0 `simpleDiff` (common prefix first), so the
 * typed "b" before "brave" is recorded *after* the original "b", and the range
 * reads "bbrave". The card then shows "changed", so the owner sees it, but
 * accepting would delete the typed character. No anchor encoding can prevent
 * this; the spec's Constraints list it next to the one above.
 *
 * The walk below is a copy of y-tiptap's position conversion
 * (`absolutePositionToRelativePosition` / `relativePositionToAbsolutePosition`
 * in `@tiptap/y-tiptap` 3.0.9) because neither takes an assoc, and the decoder
 * there refuses item anchors that resolve to position 1 — the page's very
 * first character, where a suggestion on the first word starts.
 */

/* eslint-disable @typescript-eslint/no-explicit-any, @typescript-eslint/no-unsafe-assignment, @typescript-eslint/no-unsafe-member-access, @typescript-eslint/no-unsafe-argument */

import type { Editor } from "@tiptap/core";
import * as Y from "yjs";

import {
  getMapping,
  relativePositionFromBase64,
  relativePositionToBase64,
  type ProsemirrorMapping,
} from "../comments/anchor";

/** y-tiptap's module-private `createRelativePosition(type, item)`. */
function createRelativePosition(type: any, item: Y.Item): Y.RelativePosition {
  let typeid: Y.ID | null = null;
  let tname: string | null = null;
  if (type._item === null) {
    tname = Y.findRootTypeKey(type);
  } else {
    typeid = Y.createID(type._item.id.client, type._item.id.clock);
  }
  return new Y.RelativePosition(typeid, tname, item.id);
}

/**
 * `instanceof Y.XmlText` without narrowing: the walk below reads Yjs
 * internals (`_item`, `_length`) that the public typings do not declare, so
 * it keeps `n` untyped like the JavaScript it is copied from.
 */
function isXmlText(n: unknown): boolean {
  return n instanceof Y.XmlText;
}

/**
 * y-tiptap's `absolutePositionToRelativePosition` with an `assoc` for the
 * text case. Only the XmlText branch passes it on; the structural fallbacks
 * are unchanged.
 */
function absoluteToRelative(
  absPos: number,
  type: Y.XmlFragment,
  mapping: ProsemirrorMapping,
  assoc: number,
): Y.RelativePosition {
  let pos = absPos;
  const root: any = type;
  if (pos === 0) {
    return Y.createRelativePositionFromTypeIndex(type, 0, -1);
  }
  let n: any = root._first === null ? null : root._first.content.type;
  while (n !== null && root !== n) {
    if (isXmlText(n)) {
      if (n._length >= pos) {
        return Y.createRelativePositionFromTypeIndex(n, pos, assoc);
      } else {
        pos -= n._length;
      }
      if (n._item !== null && n._item.next !== null) {
        n = n._item.next.content.type;
      } else {
        do {
          n = n._item === null ? null : n._item.parent;
          pos--;
        } while (n !== root && n !== null && n._item !== null && n._item.next === null);
        if (n !== null && n !== root) {
          n = n._item === null ? null : n._item.next.content.type;
        }
      }
    } else {
      const pNodeSize: number = (mapping.get(n) as any)?.nodeSize ?? 0;
      if (n._first !== null && pos < pNodeSize) {
        n = n._first.content.type;
        pos--;
      } else {
        if (pos === 1 && n._length === 0 && pNodeSize > 1) {
          // edge case, should end in this paragraph
          return new Y.RelativePosition(
            n._item === null ? null : n._item.id,
            n._item === null ? Y.findRootTypeKey(n) : null,
            null,
          );
        }
        pos -= pNodeSize;
        if (n._item !== null && n._item.next !== null) {
          n = n._item.next.content.type;
        } else {
          if (pos === 0) {
            // set to end of n.parent
            n = n._item === null ? n : n._item.parent;
            return new Y.RelativePosition(
              n._item === null ? null : n._item.id,
              n._item === null ? Y.findRootTypeKey(n) : null,
              null,
            );
          }
          do {
            n = n._item.parent;
            pos--;
          } while (n !== root && n._item.next === null);
          // if n is null at this point, we have an unexpected case
          if (n !== root) {
            n = n._item.next.content.type;
          }
        }
      }
    }
    if (n === null) {
      throw new Error("unexpected case");
    }
    if (pos === 0 && n.constructor !== Y.XmlText && n !== root) {
      return createRelativePosition(n._item.parent, n._item);
    }
  }
  return Y.createRelativePositionFromTypeIndex(type, root._length, -1);
}

/**
 * Whether `type` and every block above it up to `documentType` are still in
 * the document. Yjs resolves an anchor whose text node or block was deleted
 * to that deleted type as long as its items were kept rather than collected
 * (an UndoManager keeps the ones it tracks — the editor's yUndoPlugin does so
 * for the owner's own edits), and the walk below would then count it as if it
 * were live. The server refuses such an anchor (`is_deleted` + `in_live_page`
 * in `knot-crdt` `suggestions.rs`), so the client does too.
 */
function inLiveDocument(type: any, documentType: Y.XmlFragment): boolean {
  let t: any = type;
  while (t !== documentType) {
    if (t._item === null || t._item.deleted) return false;
    t = t._item.parent;
  }
  return true;
}

/**
 * y-tiptap's `relativePositionToAbsolutePosition` without its
 * `isMisresolvedTextPosition` check, which returns null for every item anchor
 * that lands on position 1 (the first character of the page), and with two
 * checks the server makes too: an anchor inside a deleted text node or block
 * is null, and so is one that resolves to anything but text (a block, or the
 * paragraph next to a line break).
 *
 * Also returns the XmlText the anchor resolved into.
 */
function relativeToAbsolute(
  y: Y.Doc,
  documentType: Y.XmlFragment,
  relPos: Y.RelativePosition,
  mapping: ProsemirrorMapping,
): { pos: number; text: Y.XmlText } | null {
  const decodedPos = Y.createAbsolutePositionFromRelativePosition(relPos, y);
  if (
    decodedPos === null ||
    !isXmlText(decodedPos.type) ||
    !Y.isParentOf(documentType, (decodedPos.type as any)._item)
  ) {
    return null;
  }
  if (!inLiveDocument(decodedPos.type, documentType)) {
    return null;
  }
  const text = decodedPos.type as Y.XmlText;
  let type: any = text;
  let pos = decodedPos.index;
  while (type !== documentType && type._item !== null) {
    const parent = type._item.parent;
    if (parent._item === null || !parent._item.deleted) {
      pos += 1; // the start tag
      let n: any = parent._first;
      // now iterate until we found type
      while (n !== null) {
        const contentType = n.content.type;
        if (contentType === type) {
          break;
        }
        if (!n.deleted) {
          if (contentType instanceof Y.XmlText) {
            pos += (contentType as any)._length;
          } else {
            const mapped: any = mapping.get(contentType);
            if (mapped == null) {
              return null;
            }
            pos += mapped.nodeSize as number;
          }
        }
        n = n.right;
      }
    }
    type = parent;
  }
  return { pos: pos - 1, text }; // we don't count the most outer tag, because it is a fragment
}

/**
 * Whether the character `rel` is tied to has been deleted. Its tombstone
 * still resolves (to where it was, in the text that held it) whether its
 * item was collected or kept. Like the server (`anchored_char_deleted`), this
 * asks about that character itself: an undo that brings it back writes a new
 * one, and the anchor stays lost. An anchor scoped to a whole text node (the
 * start of a block) has no character and is never deleted here.
 */
function tiedToDeleted(rel: Y.RelativePosition, ydoc: Y.Doc): boolean {
  if (rel.item === null) return false;
  const struct = Y.getItem(ydoc.store, rel.item);
  return !(struct instanceof Y.Item) || struct.deleted;
}

/** A decoded suggestion range; see `decodeSuggestionRange`. */
export type SuggestionRange = {
  from: number;
  to: number;
  /**
   * The anchors still resolve, but the server would refuse them as lost:
   * the two ends lie in different text nodes (a line break now splits the
   * range), or the character an insertion point is tied to was deleted —
   * e.g. by Enter before it, which moves the rest of the line into a new
   * paragraph, leaving the anchor behind on the old one.
   */
  anchorLost: boolean;
};

/**
 * `rel` if it resolves into text, else the same point tied to text directly
 * next to it, else null.
 *
 * The walk above ties a point with no text on its side to the inline node or
 * block there instead: a caret on the empty line between two line breaks,
 * after a break that ends the paragraph, or before one that starts it. The
 * server only resolves anchors inside an XmlText, so such an anchor would be
 * refused as lost on accept while the card offered it as applicable.
 */
function textAnchor(rel: Y.RelativePosition, ydoc: Y.Doc, assoc: number): Y.RelativePosition | null {
  const abs = Y.createAbsolutePositionFromRelativePosition(rel, ydoc);
  if (abs === null) return null;
  if (isXmlText(abs.type)) return rel;
  if (!(abs.type instanceof Y.XmlElement || abs.type instanceof Y.XmlFragment)) return null;
  const before: unknown = abs.index > 0 ? abs.type.get(abs.index - 1) : undefined;
  const after: unknown = abs.index < abs.type.length ? abs.type.get(abs.index) : undefined;
  // A point right after text ties to its last character, the way an
  // insertion point always does; one right before text to its start.
  if (before instanceof Y.XmlText && (assoc < 0 || !(after instanceof Y.XmlText))) {
    return Y.createRelativePositionFromTypeIndex(before, before.length, -1);
  }
  if (after instanceof Y.XmlText) return Y.createRelativePositionFromTypeIndex(after, 0, assoc);
  return null;
}

/**
 * Start ties to the first selected char (assoc 0), end to the last (assoc -1). Insert: from===to, both assoc -1.
 *
 * Null when a position has no text to tie to (see `textAnchor`); the caller
 * then says it couldn't suggest a change there.
 */
export function encodeSuggestionAnchors(
  editor: Editor,
  ydoc: Y.Doc,
  from: number,
  to: number,
): { start: string; end: string } | null {
  const mapping = getMapping(editor);
  if (!mapping) return null;

  const fragment = ydoc.getXmlFragment("default");
  try {
    const endRel = textAnchor(absoluteToRelative(to, fragment, mapping, -1), ydoc, -1);
    if (endRel === null) return null;
    const end = relativePositionToBase64(endRel);
    if (from === to) return { start: end, end };
    const startRel = textAnchor(absoluteToRelative(from, fragment, mapping, 0), ydoc, 0);
    if (startRel === null) return null;
    return { start: relativePositionToBase64(startRel), end };
  } catch {
    return null;
  }
}

/**
 * Like decodeAnchorRange, but WITHOUT y-tiptap's isMisresolvedTextPosition check.
 *
 * Also returns null when either anchor no longer lies inside text of the
 * current document, or when the end resolves before the start: the server
 * treats that as a lost anchor too, so the range is not swapped into
 * something that looks valid. Where the anchors still resolve but the server
 * would refuse them all the same, `anchorLost` says so and `from` still
 * places the card.
 *
 * Equal anchors are an insertion point; a range's two anchors always differ.
 */
export function decodeSuggestionRange(
  editor: Editor,
  ydoc: Y.Doc,
  startB64: string,
  endB64: string,
): SuggestionRange | null {
  const mapping = getMapping(editor);
  if (!mapping) return null;

  const fragment = ydoc.getXmlFragment("default");
  const size = editor.state.doc.content.size;
  try {
    const startRel = relativePositionFromBase64(startB64);
    const start = relativeToAbsolute(ydoc, fragment, startRel, mapping);
    if (start === null || start.pos < 0 || start.pos > size) return null;
    const from = start.pos;
    if (startB64 === endB64) return { from, to: from, anchorLost: tiedToDeleted(startRel, ydoc) };
    const end = relativeToAbsolute(ydoc, fragment, relativePositionFromBase64(endB64), mapping);
    if (end === null || end.pos < from || end.pos > size) return null;
    return { from, to: end.pos, anchorLost: end.text !== start.text };
  } catch {
    return null;
  }
}
