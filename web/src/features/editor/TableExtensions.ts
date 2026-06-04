/**
 * TableExtensions — Tiptap table extensions renamed to the snake_case
 * node kinds in our generated schema (`table`, `table_row`, `table_cell`,
 * `table_header`).
 *
 * Each extension adds an `align` attribute on cells/headers that mirrors
 * GFM column alignment and round-trips through to_markdown.
 */

import Table from "@tiptap/extension-table";
import TableRow from "@tiptap/extension-table-row";
import TableCell from "@tiptap/extension-table-cell";
import TableHeader from "@tiptap/extension-table-header";

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
}).configure({
  resizable: true,
  HTMLAttributes: { class: "knot-table" },
});

export const KnotTableRow = TableRow.extend({
  name: "table_row",
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
