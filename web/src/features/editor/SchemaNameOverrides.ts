/**
 * SchemaNameOverrides — Tiptap's StarterKit ships nodes named in camelCase
 * (`bulletList`, `orderedList`, `listItem`, `hardBreak`, `horizontalRule`),
 * but our canonical schema (`tools/schema.json`) uses snake_case. Without
 * this bridge, the Y.XmlFragment that backs each doc holds `<bulletList>`
 * elements that `knot-markdown` doesn't know how to serialise, and any
 * markdown export 500s with `UnsupportedNode("bulletList")`.
 *
 * Pattern is the same one MermaidCodeBlock uses for `code_block`: import
 * the underlying extension, `.extend({ name, content })` to align with our
 * schema, then disable the original in `StarterKit.configure({ … : false })`.
 */

import HardBreak from "@tiptap/extension-hard-break";
import HorizontalRule from "@tiptap/extension-horizontal-rule";
// The three list nodes now ship from one package, as named exports.
import { BulletList, ListItem, OrderedList } from "@tiptap/extension-list";

/**
 * Renaming a node does not rewrite the OPTIONS its own commands read.
 * BulletList and OrderedList resolve `options.itemTypeName` when building the
 * wrap, and it defaults to the camelCase `listItem` — a node type this schema
 * does not have. So `toggleBulletList()` and `toggleOrderedList()` threw
 * "There is no node type named 'listItem'", taking `Mod-Shift-8` and
 * `Mod-Shift-7` down with them.
 *
 * The toolbar buttons were never affected: they name both types explicitly
 * (`toggleList("bullet_list", "list_item")`), which is why the keyboard
 * shortcuts could be broken this long without anyone noticing.
 *
 * ListItem also declares `bulletListTypeName` / `orderedListTypeName` options,
 * but nothing in the v2 extension reads them — its Enter/Tab/Shift-Tab keymaps
 * all pass `this.name`, which the rename already corrected. They are left
 * unconfigured rather than set to a plausible-looking value that does nothing.
 */
export const KnotBulletList = BulletList.extend({
  name: "bullet_list",
  // Original is "listItem+"; align with renamed item.
  content: "list_item+",
}).configure({ itemTypeName: "list_item" });

export const KnotOrderedList = OrderedList.extend({
  name: "ordered_list",
  // Original is "listItem+".
  content: "list_item+",
}).configure({ itemTypeName: "list_item" });

export const KnotListItem = ListItem.extend({
  name: "list_item",
  // Content stays the same expression because it uses node groups + names
  // that aren't list-internal ("paragraph block*").
});

export const KnotHardBreak = HardBreak.extend({
  name: "hard_break",
});

export const KnotHorizontalRule = HorizontalRule.extend({
  name: "horizontal_rule",
});
