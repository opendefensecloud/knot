/**
 * The marks and the node attribute a "Show changes" view carries.
 *
 * The server's diff render (`pm_json_diff`) puts `historyInsert` on text the
 * entry added and `historyDelete` on text it removed (inside code blocks and
 * next to inline code too, which only the viewer's schema allows, see
 * `createSchemaExtensions({viewer: true})`), and `historyChange` =
 * `"added"` | `"removed"` | `"changed"` on a block it added, removed or
 * changed. Only the read-only history viewer registers these; the live
 * editor's schema never has them.
 */

import { Extension, Mark } from "@tiptap/core";

import { NODE_KINDS } from "../../editor/schema";

/** Text the entry added: `<ins class="history-insert">`. */
export const HistoryInsert = Mark.create({
  name: "historyInsert",
  inclusive: false,
  renderHTML() {
    return ["ins", { class: "history-insert", "data-history-insert": "" }, 0];
  },
});

/** Text the entry removed: `<del class="history-delete">`. */
export const HistoryDelete = Mark.create({
  name: "historyDelete",
  inclusive: false,
  renderHTML() {
    return ["del", { class: "history-delete", "data-history-delete": "" }, 0];
  },
});

/**
 * `historyChange` on every node but `doc` and `text`, rendered as
 * `data-history-change`. A React node view does not get a global attribute's
 * `renderHTML` output, so the `code_block`, `excalidraw_board` and
 * `attachment` views set it on their wrappers themselves.
 */
export const HistoryChange = Extension.create({
  name: "historyChange",
  addGlobalAttributes() {
    return [
      {
        types: NODE_KINDS.filter((kind) => kind !== "doc" && kind !== "text"),
        attributes: {
          historyChange: {
            default: null,
            parseHTML: () => null,
            renderHTML: (attrs: { historyChange?: string | null }) =>
              attrs.historyChange ? { "data-history-change": attrs.historyChange } : {},
          },
        },
      },
    ];
  },
});

export const historyDiffExtensions = [HistoryInsert, HistoryDelete, HistoryChange];
