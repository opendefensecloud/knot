/**
 * CommentsHighlightExtension — paints an inline background tint for every
 * comment thread that has a resolved (position_y, position_y_end) range.
 *
 * The list of comments + the active comment id are pushed in from React via
 * editor.storage updates; the plugin recomputes its DecorationSet whenever
 * (a) the comments list changes, (b) the active id changes, or (c) the
 * underlying Yjs document changes (so a peer's edit shifts the highlight
 * with the anchored text).
 *
 * Click-to-focus is handled here too: clicking on a span with
 * data-comment-id sets the active id via the injected setActiveCommentId.
 */

import { Extension } from "@tiptap/core";
import { Plugin, PluginKey } from "@tiptap/pm/state";
import { Decoration, DecorationSet } from "@tiptap/pm/view";
import * as Y from "yjs";

import { decodeAnchorRange } from "../comments/anchor";

export type HighlightedComment = {
  id: string;
  thread_id: string;
  position_y: string;
  position_y_end: string;
};

export type CommentsHighlightOptions = {
  doc: Y.Doc;
  comments: HighlightedComment[];
  activeCommentId: string | null;
  onSelect: (commentId: string) => void;
};

const PLUGIN_KEY = new PluginKey<DecorationSet>("commentsHighlight");

// Storage shape held on the extension. React updates this object via
// `editor.extensionStorage.commentsHighlight.X = ...` and then dispatches
// a no-op transaction with `setMeta` to trigger a re-decoration.
type Storage = {
  comments: HighlightedComment[];
  activeCommentId: string | null;
  doc: Y.Doc | null;
  onSelect: ((id: string) => void) | null;
};

export const CommentsHighlightExtension = Extension.create<CommentsHighlightOptions, Storage>({
  name: "commentsHighlight",

  addOptions() {
    return {
      doc: null as unknown as Y.Doc,
      comments: [],
      activeCommentId: null,
      onSelect: () => {},
    };
  },

  addStorage() {
    return {
      comments: [],
      activeCommentId: null,
      doc: null,
      onSelect: null,
    };
  },

  onCreate() {
    this.storage.comments = this.options.comments;
    this.storage.activeCommentId = this.options.activeCommentId;
    this.storage.doc = this.options.doc;
    this.storage.onSelect = this.options.onSelect;
  },

  addProseMirrorPlugins() {
    const storage = this.storage;
    return [
      new Plugin<DecorationSet>({
        key: PLUGIN_KEY,
        state: {
          init: (_config, state) => buildDecorations(state.doc, storage),
          apply: (tr, oldSet, _oldState, newState) => {
            const refresh = tr.getMeta(PLUGIN_KEY) as unknown;
            if (tr.docChanged || refresh === "refresh") {
              return buildDecorations(newState.doc, storage);
            }
            return oldSet.map(tr.mapping, tr.doc);
          },
        },
        props: {
          decorations: (state) => PLUGIN_KEY.getState(state) ?? DecorationSet.empty,
          handleClickOn: (_view, _pos, _node, _nodePos, event) => {
            const target = event.target as HTMLElement | null;
            if (!target) return false;
            const span = target.closest<HTMLElement>("[data-comment-id]");
            if (!span) return false;
            const id = span.dataset.commentId;
            if (!id) return false;
            storage.onSelect?.(id);
            return true;
          },
        },
      }),
    ];
  },
});

function buildDecorations(
  pmDoc: import("@tiptap/pm/model").Node,
  storage: Storage,
): DecorationSet {
  if (!storage.doc || storage.comments.length === 0) return DecorationSet.empty;
  // We need a Tiptap editor reference to call decodeAnchorRange. The plugin
  // doesn't carry one directly; instead, we keep a private ref via
  // editorRefHolder. See CommentsHighlightExtension.editor below.
  const editor = editorRefHolder.editor;
  if (!editor) return DecorationSet.empty;

  const docSize = pmDoc.content.size;
  const decos: Decoration[] = [];
  for (const c of storage.comments) {
    const range = decodeAnchorRange(editor, storage.doc, c.position_y, c.position_y_end);
    if (!range) continue;
    const from = Math.max(0, Math.min(range.from, docSize));
    const to = Math.max(0, Math.min(range.to, docSize));
    if (from >= to) continue;
    const isActive = storage.activeCommentId === c.thread_id || storage.activeCommentId === c.id;
    const cls = `comment-highlight${isActive ? " comment-highlight--active" : ""}`;

    // Atom blocks (excalidraw_board, etc.) have no text content, so an
    // inline decoration paints nothing visible. Emit a node decoration
    // instead, which lands a class on the NodeView's outer DOM element.
    let atomMatched = false;
    pmDoc.nodesBetween(from, to, (node, pos) => {
      if (node.isAtom && node.type.isBlock) {
        decos.push(
          Decoration.node(pos, pos + node.nodeSize, {
            class: cls,
            "data-comment-id": c.thread_id,
          }),
        );
        atomMatched = true;
      }
      return true;
    });

    if (!atomMatched) {
      decos.push(
        Decoration.inline(from, to, {
          class: cls,
          "data-comment-id": c.thread_id,
        }),
      );
    }
  }
  return DecorationSet.create(pmDoc, decos);
}

/**
 * The plugin can't reach the Tiptap `editor` instance from inside its state
 * callbacks (it would need the editor at construction time, which is
 * chicken-and-egg). React updates this holder via setEditorRef when the
 * editor first mounts. See KnotEditor.
 */
const editorRefHolder: { editor: import("@tiptap/core").Editor | null } = { editor: null };

export function setEditorRef(editor: import("@tiptap/core").Editor | null) {
  editorRefHolder.editor = editor;
}

/** Trigger a re-decoration after updating storage on the editor. */
export function refreshHighlights(editor: import("@tiptap/core").Editor) {
  editor.view.dispatch(editor.state.tr.setMeta(PLUGIN_KEY, "refresh"));
}
