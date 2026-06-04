/**
 * MentionExtension — typing `@` opens a popup with workspace members.
 * On select, inserts text "@<DisplayName>" wrapped in a link mark whose
 * href is `knot://user/<user_id>`. No new schema node is introduced —
 * mentions live in the existing link mark, so markdown round-trip is
 * automatic (`[@Alice](knot://user/<uuid>)`).
 */

import { Extension } from "@tiptap/core";
import Suggestion from "@tiptap/suggestion";
import { type Editor } from "@tiptap/core";

export const USER_HREF_PREFIX = "knot://user/";

export type MentionMember = {
  user_id: string;
  display_name: string;
  email?: string;
};

export type MentionExtensionOptions = {
  fetchMembers: () => Promise<MentionMember[]>;
};

function insertMention(
  editor: Editor,
  range: { from: number; to: number },
  member: MentionMember,
) {
  const text = `@${member.display_name}`;
  const href = `${USER_HREF_PREFIX}${member.user_id}`;
  editor
    .chain()
    .focus()
    .insertContentAt(range, [
      { type: "text", text, marks: [{ type: "link", attrs: { href } }] },
      // Trailing space outside the link mark so subsequent typing doesn't
      // extend the mention.
      { type: "text", text: " " },
    ])
    .run();
}

/** Lightweight DOM popup. One instance per `@`-suggestion lifecycle. */
class MentionPopup {
  el: HTMLDivElement;
  items: MentionMember[] = [];
  selected = 0;
  onPick: (m: MentionMember) => void = () => {};

  constructor() {
    this.el = document.createElement("div");
    this.el.className =
      "knot-mention-popup fixed z-50 rounded-md border border-border bg-surface shadow-lg overflow-hidden text-sm";
    this.el.style.minWidth = "180px";
    this.el.style.display = "none";
    document.body.appendChild(this.el);
  }

  show(items: MentionMember[], rect: DOMRect | null, onPick: (m: MentionMember) => void) {
    this.items = items;
    this.selected = 0;
    this.onPick = onPick;
    if (items.length === 0) {
      this.hide();
      return;
    }
    this.render();
    if (rect) {
      this.el.style.left = `${Math.round(rect.left)}px`;
      this.el.style.top = `${Math.round(rect.bottom + 4)}px`;
    }
    this.el.style.display = "block";
  }

  hide() {
    this.el.style.display = "none";
  }

  destroy() {
    this.el.remove();
  }

  onKey(e: KeyboardEvent): boolean {
    if (this.el.style.display === "none") return false;
    // Empty popup: stay invisible, don't capture keystrokes (especially
    // Enter, which the user expects to produce a newline).
    if (this.items.length === 0) return false;
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
      const m = this.items[this.selected];
      if (!m) return false;
      this.onPick(m);
      return true;
    }
    if (e.key === "Escape") {
      this.hide();
      return true;
    }
    return false;
  }

  private render() {
    this.el.innerHTML = "";
    this.items.forEach((m, i) => {
      const row = document.createElement("button");
      row.type = "button";
      row.dataset.testid = "mention-item";
      row.className = `block w-full text-left px-3 py-1.5 text-fg hover:bg-muted focus:outline-none ${i === this.selected ? "bg-muted" : ""}`;
      row.textContent = m.display_name;
      row.addEventListener("mousedown", (ev) => {
        ev.preventDefault();
        this.onPick(m);
      });
      this.el.appendChild(row);
    });
  }
}

export const MentionExtension = Extension.create<MentionExtensionOptions>({
  name: "knotMention",

  addOptions() {
    return { fetchMembers: () => Promise.resolve([]) };
  },

  addProseMirrorPlugins() {
    const opts = this.options;
    const popup = new MentionPopup();
    // Tear down the popup when the editor is destroyed.
    this.editor.on("destroy", () => popup.destroy());

    return [
      Suggestion<MentionMember>({
        editor: this.editor,
        char: "@",
        items: async ({ query }) => {
          const all = await opts.fetchMembers();
          const q = query.toLowerCase();
          return all
            .filter(
              (m) =>
                m.display_name.toLowerCase().includes(q)
                || (m.email?.toLowerCase().includes(q) ?? false),
            )
            .slice(0, 6);
        },
        command: ({ editor, range, props }) => {
          insertMention(editor, range, props as MentionMember);
        },
        render: () => ({
          onStart: (props) => {
            popup.show(props.items, props.clientRect?.() ?? null, (m) => props.command(m));
          },
          onUpdate: (props) => {
            popup.show(props.items, props.clientRect?.() ?? null, (m) => props.command(m));
          },
          onKeyDown: (props) => popup.onKey(props.event),
          onExit: () => popup.hide(),
        }),
      }),
    ];
  },
});
