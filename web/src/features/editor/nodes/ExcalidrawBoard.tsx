import { Node, mergeAttributes } from "@tiptap/core";
import { ReactNodeViewRenderer } from "@tiptap/react";

import { ExcalidrawBoardView } from "./ExcalidrawBoardView";

/**
 * ExcalidrawBoard — atom block node that embeds a server-stored Excalidraw
 * board by id. Storage is the snake_case `excalidraw_board` node registered
 * in the canonical schema (see `web/src/features/editor/schema.ts`). The
 * NodeView fetches a cached SVG preview from `/api/boards/:id/svg`; clicking
 * opens the editor modal (mounted in T10).
 */
export interface ExcalidrawBoardOptions {
  /**
   * The history viewer's board: no `Open` button, a preview that is not
   * clickable, no editor modal. The node view opens the live, editable
   * board whatever `editor.isEditable` says, so a read-only page needs
   * this rather than the editable flag.
   */
  readOnly: boolean;
}

export const ExcalidrawBoard = Node.create<ExcalidrawBoardOptions>({
  name: "excalidraw_board",
  group: "block",
  atom: true,
  selectable: true,
  draggable: true,
  addOptions() {
    return { readOnly: false };
  },
  addAttributes() {
    return {
      board_id: { default: "" },
      label: { default: null },
    };
  },
  parseHTML() {
    return [{ tag: "div[data-excalidraw-board]" }];
  },
  renderHTML({ HTMLAttributes }) {
    return [
      "div",
      mergeAttributes(HTMLAttributes, { "data-excalidraw-board": "true" }),
    ];
  },
  addNodeView() {
    return ReactNodeViewRenderer(ExcalidrawBoardView);
  },
});
