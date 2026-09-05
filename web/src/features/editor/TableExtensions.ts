/**
 * TableExtensions — Tiptap table extensions renamed to the snake_case
 * node kinds in our generated schema (`table`, `table_row`, `table_cell`,
 * `table_header`).
 *
 * Each extension adds an `align` attribute on cells/headers that mirrors
 * GFM column alignment and round-trips through to_markdown.
 */

// The four table nodes now ship from one package as named exports; the
// aggregate has no default export, so `import Table from …` fails to compile.
// TableKit is deliberately not used: it registers camelCase node names.
import { Table, TableCell, TableHeader, TableRow } from "@tiptap/extension-table";

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
}).configure({
  resizable: true,
  // Tiptap defaults this to false, and TableView (which emits its own
  // wrapper) only mounts when the editor is editable — so view mode, the
  // default here, had no `.tableWrapper` at all and nothing to scroll a
  // colwidth-pinned table inside. prose.css gives the wrapper overflow-x.
  renderWrapper: true,
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
