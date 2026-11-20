/**
 * PageLinkExtension — typing `[[` opens a page picker. Picking a page
 * replaces `[[query` with a doc link (`knot://doc/<id>`, the page title as
 * its text), the same link mark the toolbar inserts, so no schema change.
 * The last row creates the typed title as a subpage of the current page and
 * links it; it is there only when `createSubpage` is (the user may edit
 * this page). Never inside code: `a[[0]]` is not a link.
 */

import { Extension, type Editor } from "@tiptap/core";
import { PluginKey, type EditorState } from "@tiptap/pm/state";
import Suggestion from "@tiptap/suggestion";

import { DOC_HREF_PREFIX } from "./InternalLinkExtension";
import { pageLinkCandidates, type PageLinkCandidate, type PageLinkPage } from "./pageLinkCandidates";
import { placeFixedPopup } from "./popupPosition";

export type PageLinkItem =
  | { kind: "page"; page: PageLinkCandidate }
  | { kind: "create"; title: string };

export type PageLinkOptions = {
  listPages: () => Promise<PageLinkPage[]>;
  currentDocId: string | null;
  /** null: the user may not add subpages here, so no create row. Resolves
   *  null when creating failed (the caller has told the user). */
  createSubpage: ((title: string) => Promise<{ id: string; title: string } | null>) | null;
};

const TRIGGER = "[[";
const CLOSE = "]]";

/** The picker's suggestion state: where `[[query` stands right now. */
const PAGE_LINK_KEY = new PluginKey("knotPageLinkSuggestion");

type Range = { from: number; to: number };

/** Replaces `range` with a link to `id`, then a space outside the link so
 *  typing on does not extend it. `focus: false` after an awaited create:
 *  the picker never took focus, and a delayed focus would fire after the
 *  user may have moved on. */
function insertDocLink(editor: Editor, range: Range, id: string, title: string, focus = true) {
  const chain = editor.chain();
  (focus ? chain.focus() : chain)
    .insertContentAt(range, [
      { type: "text", text: title, marks: [{ type: "link", attrs: { href: `${DOC_HREF_PREFIX}${id}` } }] },
      { type: "text", text: " " },
    ])
    .run();
}

/** False in a code block and in inline code. */
function allowedHere(state: EditorState, range: Range): boolean {
  const $from = state.doc.resolve(range.from);
  if ($from.parent.type.spec.code) return false;
  const code = state.schema.marks.code;
  if (!code) return true;
  if (state.doc.rangeHasMark(range.from, range.to, code)) return false;
  return !code.isInSet($from.marks());
}

/** Lightweight DOM popup, one per editor. Rows are built with textContent
 *  only: page titles are user input. */
class PageLinkPopup {
  el: HTMLDivElement;
  items: PageLinkItem[] = [];
  selected = 0;
  /** A create request is running: keys stay captured, nothing is picked. */
  busy = false;
  onPick: (item: PageLinkItem) => void = () => {};

  constructor() {
    this.el = document.createElement("div");
    this.el.className =
      "knot-page-link-popup fixed z-50 rounded-md border border-border bg-surface shadow-lg overflow-hidden text-sm";
    this.el.style.minWidth = "220px";
    this.el.style.maxWidth = "360px";
    this.el.style.display = "none";
    this.el.setAttribute("role", "listbox");
    this.el.setAttribute("aria-label", "Page suggestions");
    document.body.appendChild(this.el);
  }

  show(items: PageLinkItem[], rect: DOMRect | null, onPick: (item: PageLinkItem) => void) {
    this.items = items;
    this.selected = 0;
    this.onPick = onPick;
    if (items.length === 0) {
      this.hide();
      return;
    }
    this.render();
    this.el.style.display = "block";
    if (rect) placeFixedPopup(this.el, rect);
  }

  hide() {
    this.el.style.display = "none";
  }

  destroy() {
    this.el.remove();
  }

  onKey(e: KeyboardEvent): boolean {
    if (this.el.style.display === "none" || this.items.length === 0) return false;
    if (e.key === "ArrowDown") {
      this.selected = (this.selected + 1) % this.items.length;
      this.render();
      return true;
    }
    if (e.key === "ArrowUp") {
      this.selected = (this.selected - 1 + this.items.length) % this.items.length;
      this.render();
      return true;
    }
    if (e.key === "Enter" || e.key === "Tab") {
      if (this.busy) return true;
      const item = this.items[this.selected];
      if (!item) return false;
      this.onPick(item);
      return true;
    }
    if (e.key === "Escape") {
      this.hide();
      return true;
    }
    return false;
  }

  private render() {
    this.el.replaceChildren();
    this.el.setAttribute("aria-activedescendant", `knot-page-link-${this.selected}`);
    this.items.forEach((item, i) => {
      const row = document.createElement("button");
      row.type = "button";
      row.id = `knot-page-link-${i}`;
      row.setAttribute("role", "option");
      row.setAttribute("aria-selected", i === this.selected ? "true" : "false");
      row.className = `block w-full text-left px-3 py-1.5 text-fg hover:bg-muted focus:outline-none ${i === this.selected ? "bg-muted" : ""}`;
      if (item.kind === "page") {
        row.dataset.testid = "page-link-item";
        const title = document.createElement("span");
        title.dataset.part = "title";
        title.className = "block truncate";
        title.textContent = item.page.title;
        row.appendChild(title);
        if (item.page.path.length > 0) {
          const path = document.createElement("span");
          path.dataset.part = "path";
          path.className = "block truncate text-xs text-fg-muted";
          path.textContent = item.page.path.join(" › ");
          row.appendChild(path);
        }
      } else {
        row.dataset.testid = "page-link-create";
        row.classList.add("border-t", "border-border");
        row.textContent = `Create subpage "${item.title}"`;
      }
      row.addEventListener("mousedown", (ev) => {
        ev.preventDefault();
        if (!this.busy) this.onPick(item);
      });
      this.el.appendChild(row);
    });
  }
}

export const PageLinkExtension = Extension.create<PageLinkOptions>({
  name: "knotPageLink",
  // Like the mention popup: the open picker owns the arrows, Enter and Tab,
  // also in a table cell, where the table extensions would take them first.
  priority: 1000,

  addOptions() {
    return { listPages: () => Promise.resolve([]), currentDocId: null, createSubpage: null };
  },

  addProseMirrorPlugins() {
    const opts = this.options;
    const popup = new PageLinkPopup();
    this.editor.on("destroy", () => popup.destroy());

    /** Creates the page, then links it where `[[query` still stands, or at
     *  the caret if a collaborator's edit moved that text. */
    async function createAndLink(editor: Editor, range: Range, title: string) {
      if (!opts.createSubpage || popup.busy) return;
      const typed = editor.state.doc.textBetween(range.from, range.to);
      popup.busy = true;
      try {
        const created = await opts.createSubpage(title);
        if (!created || editor.isDestroyed) return;
        const { doc, selection } = editor.state;
        // A collaborator's edit replaces the whole document under y-tiptap,
        // so `range` may be stale; the suggestion plugin has recomputed
        // where `[[query` stands now.
        const live = PAGE_LINK_KEY.getState(editor.state) as { active?: boolean; range?: Range; text?: string } | undefined;
        const stillThere = range.to <= doc.content.size && doc.textBetween(range.from, range.to) === typed;
        const at = live?.active && live.range && live.text === typed
          ? live.range
          : stillThere
            ? range
            : { from: selection.from, to: selection.from };
        insertDocLink(editor, at, created.id, created.title, false);
      } catch {
        // The caller reports its own failures; a throw leaves the text.
      } finally {
        popup.busy = false;
      }
    }

    return [
      Suggestion<PageLinkItem>({
        editor: this.editor,
        pluginKey: PAGE_LINK_KEY,
        char: TRIGGER,
        allowSpaces: true,
        // With spaces allowed the query would run to the end of the line, so
        // `[[Urlaub]] danach` + Enter would create "Urlaub]] danach". A
        // typed `]]` closes the picker instead.
        allow: ({ state, range }) =>
          allowedHere(state, range)
          && !state.doc.textBetween(range.from, range.to).slice(TRIGGER.length).includes(CLOSE),
        items: async ({ query }) => {
          const pages = await opts.listPages();
          const items: PageLinkItem[] = pageLinkCandidates(pages, query, opts.currentDocId).map((page) => ({
            kind: "page",
            page,
          }));
          const title = query.trim();
          if (opts.createSubpage && title !== "") items.push({ kind: "create", title });
          return items;
        },
        command: ({ editor, range, props }) => {
          const item = props as PageLinkItem;
          if (item.kind === "page") insertDocLink(editor, range, item.page.id, item.page.title);
          else void createAndLink(editor, range, item.title);
        },
        render: () => ({
          onStart: (props) => popup.show(props.items, props.clientRect?.() ?? null, (i) => props.command(i)),
          onUpdate: (props) => popup.show(props.items, props.clientRect?.() ?? null, (i) => props.command(i)),
          onKeyDown: (props) => popup.onKey(props.event),
          onExit: () => popup.hide(),
        }),
      }),
    ];
  },
});
