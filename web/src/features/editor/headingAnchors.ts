/**
 * Heading anchors and the table of contents' entries. Every heading gets
 * the GitHub-style slug of `headingSlugs.ts` as its DOM id, through a
 * decoration: nothing is stored, so ids follow every edit and never reach
 * the shared document. The public page gives its headings the same ids
 * (`crates/knot-server/src/routes/public.rs`).
 */

import { Extension } from "@tiptap/core";
import type { Node as PMNode } from "@tiptap/pm/model";
import { Plugin, PluginKey } from "@tiptap/pm/state";
import { Decoration, DecorationSet } from "@tiptap/pm/view";

import { slugger } from "./headingSlugs";

/** A heading of the page, with the id it gets. */
type HeadingInfo = { pos: number; size: number; level: number; text: string; slug: string };

/** One line of a table of contents: `level` 0–4 below the shallowest
 *  heading shown (deeper ones stay at 4). */
export type TocEntry = { level: number; text: string; slug: string };

/** The text a heading's anchor and entry come from: up to its first hard
 *  break, as Markdown ends a heading line there. */
function headingText(node: PMNode): string {
  let text = "";
  let ended = false;
  node.forEach((child) => {
    if (ended) return;
    if (child.type.name === "hard_break") ended = true;
    else text += child.textContent;
  });
  return text;
}

/** The headings of `doc` in order, every one slugged (empty ones too, as
 *  the public page does, so the ids agree). Headings in table cells are
 *  left out: Markdown writes a cell as one line of text, so the public page
 *  has no heading there. */
function headingsOf(doc: PMNode): HeadingInfo[] {
  const slug = slugger();
  const out: HeadingInfo[] = [];
  doc.descendants((node, pos) => {
    if (node.type.name === "table_cell" || node.type.name === "table_header") return false;
    if (node.type.name !== "heading") return true;
    const text = headingText(node);
    const level = Number(node.attrs.level);
    out.push({
      pos,
      size: node.nodeSize,
      level: Number.isInteger(level) && level >= 1 && level <= 6 ? level : 1,
      text,
      slug: slug(text),
    });
    return false;
  });
  return out;
}

/** The entries of a table of contents of depth `maxLevel` over `doc`. */
export function tocEntries(doc: PMNode, maxLevel: number): TocEntry[] {
  const shown = headingsOf(doc).filter((h) => h.level <= maxLevel && h.text.trim() !== "");
  const top = Math.min(...shown.map((h) => h.level));
  return shown.map((h) => ({ level: Math.min(h.level - top, 4), text: h.text, slug: h.slug }));
}

function decorations(doc: PMNode): DecorationSet {
  return DecorationSet.create(
    doc,
    headingsOf(doc).map((h) => Decoration.node(h.pos, h.pos + h.size, { id: h.slug })),
  );
}

const KEY = new PluginKey<DecorationSet>("knotHeadingAnchors");

export const HeadingAnchors = Extension.create({
  name: "knotHeadingAnchors",
  addProseMirrorPlugins() {
    return [
      new Plugin<DecorationSet>({
        key: KEY,
        state: {
          init: (_, { doc }) => decorations(doc),
          apply: (tr, old) => (tr.docChanged ? decorations(tr.doc) : old),
        },
        props: { decorations: (state) => KEY.getState(state) },
      }),
    ];
  },
});
