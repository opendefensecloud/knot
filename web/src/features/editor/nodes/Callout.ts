import Blockquote from "@tiptap/extension-blockquote";

import { calloutKind, normaliseTitle } from "../callouts";

/**
 * The blockquote, with the two attributes that make it a callout box:
 * `kind` (one of the five of `callouts.ts`; anything else is a plain
 * quote) and `title` (plain text). Both are optional, so a knot without
 * them reads a box as a plain quote and keeps its text. HTML form:
 * `<blockquote data-kind="warning" data-title="…">`.
 */
export const KnotBlockquote = Blockquote.extend({
  addAttributes() {
    return {
      ...this.parent?.(),
      kind: {
        default: null,
        parseHTML: (el: HTMLElement) => calloutKind(el.getAttribute("data-kind")),
        renderHTML: (attrs: Record<string, unknown>) => {
          const kind = calloutKind(attrs.kind);
          return kind ? { "data-kind": kind } : {};
        },
      },
      title: {
        default: null,
        parseHTML: (el: HTMLElement) => normaliseTitle(el.getAttribute("data-title") ?? "") || null,
        renderHTML: (attrs: Record<string, unknown>) =>
          typeof attrs.title === "string" && attrs.title !== "" ? { "data-title": attrs.title } : {},
      },
    };
  },
});
