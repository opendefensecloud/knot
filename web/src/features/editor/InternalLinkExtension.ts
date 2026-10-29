/**
 * InternalLinkExtension — intercepts clicks on links whose href is a
 * `knot://doc/<uuid>` sentinel and navigates via react-router instead of a
 * full page reload. Also decorates doc links, @-mentions and date chips
 * with a class (`knot-internal-link`, `knot-mention`, `knot-datetime`) that
 * prose.css styles as a pill. ProseMirror renders the decoration as a span
 * inside the link's `<a>`, so the class is not on the `<a>` itself.
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
const TIME_HREF_PREFIX = "knot://time/";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

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
            // By href, not the decoration: its attributes land on a span
            // inside the <a>, and the href is there as soon as the link is.
            const a = target?.closest<HTMLAnchorElement>("a[href^='knot://doc/']");
            if (!a) return false;
            const href = a.getAttribute("href") ?? "";
            const docId = href.startsWith(DOC_HREF_PREFIX)
              ? href.slice(DOC_HREF_PREFIX.length)
              : null;
            // A pasted link can carry anything after the prefix, such as
            // `<id>/permissions`; only a page id is a page to go to.
            if (!docId || !UUID.test(docId)) return false;
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
    } else if (href.startsWith(TIME_HREF_PREFIX)) {
      decos.push(
        Decoration.inline(pos, pos + node.nodeSize, {
          class: "knot-datetime",
          "data-knot-datetime": "true",
        }),
      );
    }
  });
  return DecorationSet.create(doc, decos);
}
