/**
 * Y.RelativePosition anchors for comment threads.
 *
 * encode/decode convert between a ProseMirror absolute position and a
 * base64-encoded Y.RelativePosition so the server can store it opaquely
 * in the `position_y` column and the client can resolve it back to a
 * pixel offset even after concurrent edits.
 *
 * The mapping is obtained from the ySyncPlugin state that Tiptap's
 * Collaboration extension installs. It MUST be imported from the same
 * package that extension installs its plugin from (@tiptap/y-tiptap), not
 * from y-prosemirror: both call `new PluginKey("y-sync")`, ProseMirror
 * uniquifies the second one, and `getState` is a bare property read — so the
 * wrong import silently returns undefined and every anchor becomes null. If the plugin isn't mounted
 * (e.g. viewer mode before the editor is ready), both functions return null.
 */

import type { Editor } from "@tiptap/core";
import type { Node as PmNode } from "@tiptap/pm/model";
import * as Y from "yjs";
import {
  absolutePositionToRelativePosition,
  relativePositionToAbsolutePosition,
  ySyncPluginKey,
} from "@tiptap/y-tiptap";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type ProsemirrorMapping = Map<Y.AbstractType<any>, PmNode | PmNode[]>;

/** Shape of the ySyncPlugin's per-editor state. */
interface YSyncState {
  binding?: {
    mapping: ProsemirrorMapping;
  } | null;
}

function getMapping(editor: Editor): ProsemirrorMapping | null {
  const ystate = ySyncPluginKey.getState(editor.state) as YSyncState | null | undefined;
  return ystate?.binding?.mapping ?? null;
}

/** Returns null when the ySyncPlugin mapping isn't available. */
export function encodeAnchor(
  editor: Editor,
  ydoc: Y.Doc,
  absPos: number,
): string | null {
  const mapping = getMapping(editor);
  if (!mapping) return null;

  const fragment = ydoc.getXmlFragment("default");
  try {
    // eslint-disable-next-line @typescript-eslint/no-unsafe-assignment
    const rel: Y.RelativePosition = absolutePositionToRelativePosition(absPos, fragment, mapping);
    const bytes = Y.encodeRelativePosition(rel);
    return btoa(String.fromCharCode(...bytes));
  } catch {
    return null;
  }
}

/** Returns null when the anchor can no longer be resolved (text was deleted). */
export function decodeAnchor(
  editor: Editor,
  ydoc: Y.Doc,
  b64: string,
): number | null {
  const mapping = getMapping(editor);
  if (!mapping) return null;

  try {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const rel = Y.decodeRelativePosition(bytes);
    const fragment = ydoc.getXmlFragment("default");
    const abs = relativePositionToAbsolutePosition(ydoc, fragment, rel, mapping);
    if (abs === null) return null;
    return abs;
  } catch {
    return null;
  }
}

/**
 * Encode a (from, to) selection as a pair of Y.RelativePosition anchors.
 * Returns null when the mapping isn't available; either component can be
 * null individually, but we return the pair so callers can persist both
 * or fall back together.
 */
export function encodeAnchorRange(
  editor: Editor,
  ydoc: Y.Doc,
  from: number,
  to: number,
): { start: string | null; end: string | null } {
  return {
    start: encodeAnchor(editor, ydoc, from),
    end: encodeAnchor(editor, ydoc, to),
  };
}

/**
 * Resolve a stored {start, end} pair back to ProseMirror absolute positions.
 * Returns null if either component fails to resolve — callers can then fall
 * back to a single-point caret or hide the highlight entirely.
 */
export function decodeAnchorRange(
  editor: Editor,
  ydoc: Y.Doc,
  startB64: string,
  endB64: string,
): { from: number; to: number } | null {
  const from = decodeAnchor(editor, ydoc, startB64);
  if (from === null) return null;
  const to = decodeAnchor(editor, ydoc, endB64);
  if (to === null) return null;
  if (to < from) return { from: to, to: from };
  return { from, to };
}
