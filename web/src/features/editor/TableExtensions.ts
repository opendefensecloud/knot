/**
 * TableExtensions — Tiptap table extensions renamed to the snake_case
 * node kinds in our generated schema (`table`, `table_row`, `table_cell`,
 * `table_header`).
 *
 * Each extension adds an `align` attribute on cells/headers that mirrors
 * GFM column alignment and round-trips through to_markdown.
 *
 * Column widths are proportions, not pixels: see columnWidths.ts for the
 * model and TableColumnView.ts for rendering and drag-resizing.
 */

import { mergeAttributes, type Editor } from "@tiptap/core";
// The four table nodes now ship from one package as named exports; the
// aggregate has no default export, so `import Table from …` fails to compile.
// TableKit is deliberately not used: it registers camelCase node names.
import { Table, TableCell, TableHeader, TableRow } from "@tiptap/extension-table";
import type { DOMOutputSpec } from "@tiptap/pm/model";
import { Plugin, PluginKey, TextSelection } from "@tiptap/pm/state";

import { readColumnWeights, resolveShares, sharePercent } from "./columnWidths";
import { TableColumnView } from "./TableColumnView";

const alignAttr = {
  align: {
    default: null as null | "left" | "center" | "right",
    parseHTML: (el: HTMLElement) => {
      const v = el.style.textAlign || el.getAttribute("data-align");
      if (v === "left" || v === "center" || v === "right") return v;
      return null;
    },
    renderHTML: (attrs: { align?: string | null }) => {
      if (!attrs.align) return {};
      return {
        "data-align": attrs.align,
        style: `text-align: ${attrs.align};`,
      };
    },
  },
};

export const KnotTable = Table.extend({
  name: "table",
  // The base extension's content expression is "tableRow+", which refers to
  // the camelCase name. Renaming the node alone doesn't update that string —
  // it has to be overridden to match our snake_case schema.
  content: "table_row+",

  // Every table renders through TableColumnView, in view and edit mode
  // alike. Upstream picks its view (and whether to install prosemirror-tables'
  // pixel resizing) once, from the editability at creation — which is view
  // mode for every existing doc — so a later setEditable() could not add the
  // resize handles.
  addNodeView() {
    return ({ node, view, getPos, HTMLAttributes }) =>
      new TableColumnView(node, view, getPos, mergeAttributes(this.options.HTMLAttributes, HTMLAttributes));
  },

  // Tiptap runs a later extension's shortcuts first, and the table is
  // registered after the lists and code blocks: in a cell, its Tab and
  // Shift-Tab (next/previous cell) took the keys from both. Moving cells also
  // selects the whole cell, so the next keystroke replaced it.
  addKeyboardShortcuts() {
    const parent = this.parent?.() ?? {};
    return {
      ...parent,
      Tab: (props) => (blockTakesTab(this.editor, 1) ? false : (parent.Tab?.(props) ?? false)),
      "Shift-Tab": (props) => (blockTakesTab(this.editor, -1) ? false : (parent["Shift-Tab"]?.(props) ?? false)),
      ArrowDown: () => exitBelowTable(this.editor),
    };
  },

  addProseMirrorPlugins() {
    return [...(this.parent?.() ?? []), clickBelowLastTable()];
  },

  // getHTML() and the clipboard: the same percentage columns as the view, so
  // a table pasted elsewhere keeps its proportions. Cells still carry their
  // `colwidth` weights, which is what a paste back into knot reads.
  renderHTML({ node, HTMLAttributes }) {
    const columns: DOMOutputSpec[] = resolveShares(readColumnWeights(node)).map((share) => [
      "col",
      { style: `width: ${sharePercent(share)}` },
    ]);
    return [
      "div",
      { class: "tableWrapper" },
      [
        "table",
        mergeAttributes(this.options.HTMLAttributes, HTMLAttributes, { style: "width: 100%" }),
        ["colgroup", {}, ...columns],
        ["tbody", 0],
      ],
    ];
  },
}).configure({
  // prosemirror-tables' resizing stores pixels and pins the table to their
  // sum; TableColumnView replaces it.
  resizable: false,
  HTMLAttributes: { class: "knot-table" },
});

export const KnotTableRow = TableRow.extend({
  name: "table_row",
  // Same fix here: original is "(tableCell | tableHeader)*".
  content: "(table_cell | table_header)*",
  // The prosemirror-tables plugin uses tableRole to find related node types.
  // Re-declare it here because the parent extension hardcodes the camelCase
  // type names in its `parseHTML`/spec.
  tableRole: "row",
});

export const KnotTableCell = TableCell.extend({
  name: "table_cell",
  tableRole: "cell",
  addAttributes() {
    return {
      ...this.parent?.(),
      ...alignAttr,
    };
  },
});

export const KnotTableHeader = TableHeader.extend({
  name: "table_header",
  tableRole: "header_cell",
  addAttributes() {
    return {
      ...this.parent?.(),
      ...alignAttr,
    };
  },
});

/**
 * Whether Tab (`dir` 1) or Shift-Tab (-1) belongs to the block the caret is
 * in rather than to the table around it: a code block indents its line, and
 * a list item indents or outdents when it can. Only blocks inside the
 * caret's cell count, so in a table that sits in a list item Tab still moves
 * between cells.
 */
function blockTakesTab(editor: Editor, dir: 1 | -1): boolean {
  const { $from } = editor.state.selection;
  for (let depth = $from.depth; depth > 0; depth -= 1) {
    const node = $from.node(depth);
    const role = node.type.spec.tableRole as string | undefined;
    if (role === "cell" || role === "header_cell") return false;
    if (node.type.name === "code_block") return true;
    if (node.type.name === "list_item") {
      return dir > 0 ? editor.can().sinkListItem("list_item") : editor.can().liftListItem("list_item");
    }
  }
  return false;
}

/**
 * ArrowDown on the last line of a table's last row, when nothing follows the
 * table, opens a paragraph below it. prosemirror-tables otherwise looks for
 * a text position after the table, finds none, and keeps the caret in the
 * table, so with no trailing paragraph (`trailingNode` is off, see
 * extensions.ts) there was no way down. This paragraph is the person's own
 * edit, not one made for everyone by opening the page.
 */
function exitBelowTable(editor: Editor): boolean {
  const { state, view } = editor;
  const { selection } = state;
  if (!(selection instanceof TextSelection) || !selection.empty) return false;
  const { $head } = selection;
  let cellDepth = -1;
  // The caret's block must be the last one at every level up to the cell.
  for (let depth = $head.depth - 1; depth > 0; depth -= 1) {
    if ($head.indexAfter(depth) !== $head.node(depth).childCount) return false;
    const role = $head.node(depth).type.spec.tableRole as string | undefined;
    if (role === "cell" || role === "header_cell") {
      cellDepth = depth;
      break;
    }
  }
  const tableDepth = cellDepth - 2;
  if (tableDepth < 1) return false;
  const table = $head.node(tableDepth);
  const parent = $head.node(tableDepth - 1);
  if ($head.indexAfter(tableDepth) !== table.childCount) return false;
  if ($head.indexAfter(tableDepth - 1) !== parent.childCount) return false;
  const paragraph = state.schema.nodes.paragraph;
  if (!paragraph || !parent.canReplaceWith(parent.childCount, parent.childCount, paragraph)) return false;
  if (!view.endOfTextblock("down")) return false;

  const pos = $head.after(tableDepth);
  const tr = state.tr.insert(pos, paragraph.create());
  view.dispatch(tr.setSelection(TextSelection.create(tr.doc, pos + 1)).scrollIntoView());
  return true;
}

/**
 * A click below a table that ends the page opens a paragraph there. The
 * space below it is the table's bottom margin or the editor's empty
 * min-height, neither of which holds a text position: ProseMirror put the
 * caret at whatever position it found nearest, which could be the table's
 * first cell or the paragraph above the table.
 */
function clickBelowLastTable(): Plugin {
  return new Plugin({
    key: new PluginKey("knotClickBelowTable"),
    props: {
      handleDOMEvents: {
        mousedown(view, event) {
          if (!view.editable || event.button !== 0) return false;
          const { doc } = view.state;
          const last = doc.lastChild;
          if (!last || last.type.name !== "table") return false;
          const tablePos = doc.content.size - last.nodeSize;
          const wrapper = view.nodeDOM(tablePos);
          if (!(wrapper instanceof HTMLElement)) return false;
          // Only the editor's own surface or the wrapper's margin: never the
          // cells, the column handles or anything else inside the table.
          if (event.target !== view.dom && event.target !== wrapper) return false;
          const table = wrapper.querySelector("table") ?? wrapper;
          if (event.clientY <= table.getBoundingClientRect().bottom) return false;
          const paragraph = view.state.schema.nodes.paragraph;
          if (!paragraph) return false;

          event.preventDefault();
          const end = doc.content.size;
          const tr = view.state.tr.insert(end, paragraph.create());
          view.dispatch(tr.setSelection(TextSelection.create(tr.doc, end + 1)).scrollIntoView());
          view.focus();
          return true;
        },
      },
    },
  });
}
