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
 * this; the spec's Constraints should list it next to the one above.
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
 * that lands on position 1 (the first character of the page), and with a
 * liveness check: an anchor inside a deleted text node or block is null.
 */
function relativeToAbsolute(
  y: Y.Doc,
  documentType: Y.XmlFragment,
  relPos: Y.RelativePosition,
  mapping: ProsemirrorMapping,
): number | null {
  const decodedPos = Y.createAbsolutePositionFromRelativePosition(relPos, y);
  if (
    decodedPos === null ||
    (decodedPos.type !== documentType && !Y.isParentOf(documentType, (decodedPos.type as any)._item))
  ) {
    return null;
  }
  if (!inLiveDocument(decodedPos.type, documentType)) {
    return null;
  }
  let type: any = decodedPos.type;
  let pos = 0;
  if (type.constructor === Y.XmlText) {
    pos = decodedPos.index;
  } else if (type._item === null || !type._item.deleted) {
    let n: any = type._first;
    let i = 0;
    while (i < type._length && i < decodedPos.index && n !== null) {
      if (!n.deleted) {
        const t = n.content.type;
        i++;
        if (t instanceof Y.XmlText) {
          pos += (t as any)._length;
        } else {
          const mapped: any = mapping.get(t);
          if (mapped == null) {
            return null;
          }
          pos += mapped.nodeSize as number;
        }
      }
      n = n.right;
    }
    pos += 1; // increase because we go out of n
  }
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
  return pos - 1; // we don't count the most outer tag, because it is a fragment
}

/** Start ties to the first selected char (assoc 0), end to the last (assoc -1). Insert: from===to, both assoc -1. */
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
    const end = relativePositionToBase64(absoluteToRelative(to, fragment, mapping, -1));
    const start = from === to ? end : relativePositionToBase64(absoluteToRelative(from, fragment, mapping, 0));
    return { start, end };
  } catch {
    return null;
  }
}

/**
 * Like decodeAnchorRange, but WITHOUT y-tiptap's isMisresolvedTextPosition check.
 *
 * Also returns null when either anchor no longer lies inside the current
 * document, or when the end resolves before the start: the server treats
 * that as a lost anchor too, so the range is not swapped into something that
 * looks valid.
 */
export function decodeSuggestionRange(
  editor: Editor,
  ydoc: Y.Doc,
  startB64: string,
  endB64: string,
): { from: number; to: number } | null {
  const mapping = getMapping(editor);
  if (!mapping) return null;

  const fragment = ydoc.getXmlFragment("default");
  const size = editor.state.doc.content.size;
  try {
    const from = relativeToAbsolute(ydoc, fragment, relativePositionFromBase64(startB64), mapping);
    if (from === null || from < 0 || from > size) return null;
    const to = startB64 === endB64 ? from : relativeToAbsolute(ydoc, fragment, relativePositionFromBase64(endB64), mapping);
    if (to === null || to < from || to > size) return null;
    return { from, to };
  } catch {
    return null;
  }
}
