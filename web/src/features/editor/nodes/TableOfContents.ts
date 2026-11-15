import { Node, mergeAttributes } from "@tiptap/core";
import { ReactNodeViewRenderer } from "@tiptap/react";

import { TableOfContentsView } from "./TableOfContentsView";

/**
 * TableOfContents — atom block that lists the page's headings. It stores
 * only `maxLevel` (1–6, default 3): the list is built from the current
 * headings every time it renders, so it never goes stale and never writes
 * to the document. Storage is the snake_case `table_of_contents` node of
 * `tools/schema.json`. Markdown form: `[[_TOC_]]`, with
 * `<!-- knot:toc maxLevel=N -->` before it when N is not 3.
 */

export const TOC_DEFAULT_LEVEL = 3;

/** `value` as a depth 1–6; anything else is the default. */
export function clampTocLevel(value: unknown): number {
  const n = typeof value === "number" ? value : Number.parseInt(String(value), 10);
  return Number.isInteger(n) && n >= 1 && n <= 6 ? n : TOC_DEFAULT_LEVEL;
}

export const TableOfContents = Node.create({
  name: "table_of_contents",
  group: "block",
  atom: true,
  selectable: true,
  draggable: true,
  addAttributes() {
    return {
      maxLevel: {
        default: TOC_DEFAULT_LEVEL,
        parseHTML: (el) => clampTocLevel(el.getAttribute("data-max-level")),
        renderHTML: (attrs) => ({ "data-max-level": String(clampTocLevel(attrs.maxLevel)) }),
      },
    };
  },
  parseHTML() {
    return [{ tag: "div[data-knot-toc]" }];
  },
  renderHTML({ HTMLAttributes }) {
    return ["div", mergeAttributes(HTMLAttributes, { "data-knot-toc": "true" })];
  },
  addNodeView() {
    return ReactNodeViewRenderer(TableOfContentsView);
  },
});
