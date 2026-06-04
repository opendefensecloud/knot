/**
 * InternalLinkExtension — intercepts clicks on links whose href is a
 * `knot://doc/<uuid>` sentinel and navigates via react-router instead of a
 * full page reload. Also tags those links with a `data-knot-doc` attribute
 * so CSS can give them a subtle styling difference from external links.
 *
 * Live-title resolution (replacing the link's own text with the target's
 * current title) is intentionally NOT done here — that would mutate the
 * Yjs document on every render. Instead, the link's text is whatever the
 * author wrote (typically the doc title at insertion time), and renaming
 * propagates only on the next time the user explicitly re-creates the
 * link. A future tooltip can show the current title without mutating
 * stored content.
 */

import { Extension } from "@tiptap/core";
import { Plugin, PluginKey } from "@tiptap/pm/state";
import { Decoration, DecorationSet } from "@tiptap/pm/view";
import { type NavigateFunction } from "react-router-dom";

const DOC_HREF_PREFIX = "knot://doc/";
const USER_HREF_PREFIX = "knot://user/";

export const InternalLinkExtension = Extension.create<{
  navigate: NavigateFunction | null;
}>({
  name: "knotInternalLink",

  addOptions() {
    return { navigate: null };
  },

  addProseMirrorPlugins() {
    const navigate = this.options.navigate;
    return [
      new Plugin({
        key: new PluginKey("knotInternalLinkDecorations"),
        state: {
          init: (_, { doc }) => buildDecorations(doc),
          apply: (tr, old) => (tr.docChanged ? buildDecorations(tr.doc) : old),
        },
        props: {
          decorations(state) {
            return this.getState(state) ?? null;
          },
          handleClickOn(_view, _pos, _node, _nodePos, event, _direct) {
            if (!navigate) return false;
            // Let the browser handle modifier-clicks (open-in-new-tab etc.).
            // Also bail on middle/right clicks so context menus still work.
            if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey)
              return false;
            if (event.button !== 0) return false;
            const target = event.target as HTMLElement | null;
            // Match either the decoration class OR the raw href, so a
            // freshly-arrived remote link is navigated correctly even if a
            // transaction lands before the decoration plugin re-runs.
            const a = target?.closest<HTMLAnchorElement>(
              "a[data-knot-doc], a[href^='knot://doc/']",
            );
            if (!a) return false;
            const href = a.getAttribute("href") ?? "";
            const docId = href.startsWith(DOC_HREF_PREFIX)
              ? href.slice(DOC_HREF_PREFIX.length)
              : null;
            if (!docId) return false;
            event.preventDefault();
            void navigate(`/doc/${docId}`);
            return true;
          },
        },
      }),
    ];
  },
});

function buildDecorations(doc: import("@tiptap/pm/model").Node): DecorationSet {
  const decos: Decoration[] = [];
  doc.descendants((node, pos) => {
    if (!node.isText) return;
    const link = node.marks.find((m) => m.type.name === "link");
    if (!link) return;
    const href = (link.attrs.href as string | undefined) ?? "";
    if (href.startsWith(DOC_HREF_PREFIX)) {
      decos.push(
        Decoration.inline(pos, pos + node.nodeSize, {
          class: "knot-internal-link",
          "data-knot-doc": "true",
        }),
      );
    } else if (href.startsWith(USER_HREF_PREFIX)) {
      decos.push(
        Decoration.inline(pos, pos + node.nodeSize, {
          class: "knot-mention",
          "data-knot-mention": "true",
        }),
      );
    }
  });
  return DecorationSet.create(doc, decos);
}
