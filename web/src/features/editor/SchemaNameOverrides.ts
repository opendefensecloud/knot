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

import BulletList from "@tiptap/extension-bullet-list";
import OrderedList from "@tiptap/extension-ordered-list";
import ListItem from "@tiptap/extension-list-item";
import HardBreak from "@tiptap/extension-hard-break";
import HorizontalRule from "@tiptap/extension-horizontal-rule";

export const KnotBulletList = BulletList.extend({
  name: "bullet_list",
  // Original is "listItem+"; align with renamed item.
  content: "list_item+",
});

export const KnotOrderedList = OrderedList.extend({
  name: "ordered_list",
  // Original is "listItem+".
  content: "list_item+",
});

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
