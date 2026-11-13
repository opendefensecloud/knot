/**
 * OrderedListMarkerGutter — room for numbered-list markers past item 9 (#95,
 * '"10." shows as ".0."').
 *
 * List markers sit outside the list's content box, in its left padding. That
 * padding fits one digit, so "10." hung a digit further left than it had room
 * for, past the editor host's edge, where `overflow-x: clip` cut its first
 * digit off. A fixed wider padding would only move the problem to item 100
 * and push every short list's text further right than a bullet list's.
 *
 * Instead each <ol> whose widest marker has more than one digit gets a node
 * decoration with that digit count, as the `--knot-ol-digits` custom
 * property, and prose.css widens the padding by one digit (`1ch`) per digit
 * past the first. The widest marker usually belongs to the last item, whose
 * number is `start + items - 1`, so a ten-item list starting at 95 counts as
 * three digits ("104.").
 *
 * A decoration, not an attribute: nothing here reaches the schema, the
 * Y.Doc or the Markdown export. It is recomputed on every document change —
 * ProseMirror keeps the DOM of a node whose markup is unchanged, so a
 * `renderHTML` computed from the item count would go stale as items are
 * added. The walk skips textblocks, so it visits block nodes only.
 */

import { Extension } from "@tiptap/core";
import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import { Plugin, PluginKey } from "@tiptap/pm/state";
import { Decoration, DecorationSet } from "@tiptap/pm/view";

const ORDERED_LIST = "ordered_list";

/** The custom property prose.css reads to size an ordered list's gutter. */
export const MARKER_DIGITS_PROPERTY = "--knot-ol-digits";

/**
 * Characters in the widest marker of an ordered list, not counting the ". "
 * that follows every number.
 *
 * `start` comes straight from node attrs. The schema says int ≥ 1, but pasted
 * HTML (`<ol start="-15">`) gets through Tiptap's parseInt unchanged, and a
 * server-parsed document can hand over a string. A browser numbers from any
 * integer start, so this does too; only a missing or non-numeric start counts
 * as 1. The minus sign is one more character, and with a negative start the
 * widest marker can be the first rather than the last ("-15." in -15 … -13).
 */
export function markerDigits(start: unknown, itemCount: number): number {
  const parsed =
    typeof start === "number" ? start : typeof start === "string" ? Number.parseInt(start, 10) : Number.NaN;
  const first = Number.isFinite(parsed) ? Math.trunc(parsed) : 1;
  const last = first + Math.max(itemCount, 1) - 1;
  return Math.max(String(first).length, String(last).length);
}

function buildDecorations(doc: ProseMirrorNode): DecorationSet {
  const decorations: Decoration[] = [];
  doc.descendants((node, pos) => {
    if (node.isTextblock) return false;
    if (node.type.name === ORDERED_LIST) {
      const digits = markerDigits(node.attrs.start, node.childCount);
      if (digits > 1) {
        decorations.push(
          Decoration.node(pos, pos + node.nodeSize, {
            style: `${MARKER_DIGITS_PROPERTY}: ${digits}`,
          }),
        );
      }
    }
    return true;
  });
  return decorations.length ? DecorationSet.create(doc, decorations) : DecorationSet.empty;
}

const pluginKey = new PluginKey<DecorationSet>("knotOrderedListMarkerGutter");

export const OrderedListMarkerGutter = Extension.create({
  name: "knotOrderedListMarkerGutter",

  addProseMirrorPlugins() {
    return [
      new Plugin<DecorationSet>({
        key: pluginKey,
        state: {
          init: (_config, state) => buildDecorations(state.doc),
          apply: (tr, previous) => (tr.docChanged ? buildDecorations(tr.doc) : previous),
        },
        props: {
          decorations: (state) => pluginKey.getState(state),
        },
      }),
    ];
  },
});
