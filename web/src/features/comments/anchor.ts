/**
 * Y.RelativePosition anchors for comment threads.
 *
 * encode/decode convert between a ProseMirror absolute position and a
 * base64-encoded Y.RelativePosition so the server can store it opaquely
 * in the `position_y` column and the client can resolve it back to a
 * pixel offset even after concurrent edits.
 *
 * The y-prosemirror mapping is obtained from the ySyncPlugin state that
 * Tiptap's Collaboration extension installs. If the plugin isn't mounted
 * (e.g. viewer mode before the editor is ready), both functions return null.
 */

import type { Editor } from "@tiptap/core";
import type { Node as PmNode } from "@tiptap/pm/model";
import * as Y from "yjs";
import {
  absolutePositionToRelativePosition,
  relativePositionToAbsolutePosition,
  ySyncPluginKey,
} from "y-prosemirror";

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
