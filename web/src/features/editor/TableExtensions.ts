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

import { mergeAttributes } from "@tiptap/core";
// The four table nodes now ship from one package as named exports; the
// aggregate has no default export, so `import Table from …` fails to compile.
// TableKit is deliberately not used: it registers camelCase node names.
import { Table, TableCell, TableHeader, TableRow } from "@tiptap/extension-table";
import type { DOMOutputSpec } from "@tiptap/pm/model";

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
