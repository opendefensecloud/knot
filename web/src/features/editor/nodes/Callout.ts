import Blockquote from "@tiptap/extension-blockquote";
import { InputRule } from "@tiptap/core";
import { ReactNodeViewRenderer } from "@tiptap/react";

import { calloutKind, normaliseTitle, type CalloutKind } from "../callouts";
import { CalloutView } from "./CalloutView";

declare module "@tiptap/core" {
  interface Commands<ReturnType> {
    callout: {
      /**
       * Makes the quote at the caret a box of `kind` (a box changes its
       * kind; a plain quote becomes a box, not a box inside it), or wraps
       * the block at the caret in a new box.
       */
      setCallout: (kind: CalloutKind) => ReturnType;
    };
  }
}

/** `[!warning] ` and the other four, any case, typed at a paragraph's start. */
const MARKER = /^\[!(note|tip|important|warning|caution)\]\s$/i;

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

  addCommands() {
    return {
      ...this.parent?.(),
      setCallout:
        (kind) =>
        ({ state, tr, dispatch, commands }) => {
          const { $from } = state.selection;
          for (let d = $from.depth; d > 0; d--) {
            if ($from.node(d).type.name !== this.name) continue;
            if (dispatch) tr.setNodeAttribute($from.before(d), "kind", kind);
            return true;
          }
          return commands.wrapIn(this.name, { kind });
        },
    };
  },

  addInputRules() {
    const type = this.type;
    return [
      ...(this.parent?.() ?? []),
      // `[!warning] ` at the start of a quote's first paragraph gives the
      // quote that kind; with StarterKit's `> ` rule, `> [!warning] ` typed
      // in one go makes a box. Elsewhere it stays text.
      new InputRule({
        find: MARKER,
        handler: ({ state, range, match }) => {
          const $from = state.doc.resolve(range.from);
          const depth = $from.depth - 1;
          const kind = calloutKind(match[1]);
          if (!kind || depth < 1 || $from.node(depth).type !== type || $from.index(depth) !== 0) return null;
          state.tr.delete(range.from, range.to).setNodeAttribute($from.before(depth), "kind", kind);
        },
      }),
    ];
  },

  addNodeView() {
    return ReactNodeViewRenderer(CalloutView);
  },
});
