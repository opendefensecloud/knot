/**
 * DateTimeExtension — typing `//` opens a small date+time picker. On
 * confirm, inserts text in the user's local format wrapped in a link
 * mark whose href is `knot://time/<rfc3339-utc>`. No new schema node
 * is introduced — datetimes live in the existing link mark, so the
 * markdown round-trip is automatic ([Jun 4 2:00 PM](knot://time/...)).
 *
 * Click on an existing datetime link reopens the picker prefilled with
 * the original ISO so the value can be edited.
 *
 * The popup uses native `<input type="date">` + `<input type="time">`,
 * which avoids pulling in a date-picker dependency and keeps the
 * bundle small. Browsers render their own platform-appropriate UI.
 */

import { Extension } from "@tiptap/core";
import type { RawCommands } from "@tiptap/core";
import Suggestion from "@tiptap/suggestion";
import { Plugin, PluginKey } from "@tiptap/pm/state";
import { type Editor } from "@tiptap/core";

declare module "@tiptap/core" {
  interface Commands<ReturnType> {
    knotDateTime: {
      /** Open the date+time picker at the current cursor; on Apply,
       *  inserts the chip. Used by the toolbar Calendar button. */
      openDateTimePicker: () => ReturnType;
    };
  }
}

export const TIME_HREF_PREFIX = "knot://time/";

/** Format an ISO UTC timestamp as a short local-time label for the chip text. */
export function formatLocalChip(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  // E.g. "Jun 4, 2026 2:00 PM" — concise enough for inline reading.
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Build the ISO and chip text from the picker's separate date/time values. */
export function buildIso(dateStr: string, timeStr: string): string | null {
  // dateStr "YYYY-MM-DD"; timeStr "HH:MM" (no seconds). Combine and treat
  // as local time, then convert to ISO UTC.
  if (!dateStr) return null;
  const [y, m, d] = dateStr.split("-").map(Number);
  const [hh = 0, mm = 0] = (timeStr || "00:00").split(":").map(Number);
  if (!y || !m || !d) return null;
  const local = new Date(y, m - 1, d, hh, mm);
  if (Number.isNaN(local.getTime())) return null;
  return local.toISOString();
}

/** Split an ISO UTC string into local "YYYY-MM-DD" + "HH:MM" pieces. */
export function splitIso(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: "", time: "" };
  const pad = (n: number) => String(n).padStart(2, "0");
  const date = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return { date, time };
}

function insertDatetime(
  editor: Editor,
  range: { from: number; to: number },
  iso: string,
) {
  const text = formatLocalChip(iso);
  const href = `${TIME_HREF_PREFIX}${iso}`;
  editor
    .chain()
    .focus()
    .insertContentAt(range, [
      { type: "text", text, marks: [{ type: "link", attrs: { href } }] },
      { type: "text", text: " " },
    ])
    .run();
}

/** Lightweight DOM popover with native date+time inputs. */
class DateTimePopup {
  el: HTMLDivElement;
  dateInput: HTMLInputElement;
  timeInput: HTMLInputElement;
  applyBtn: HTMLButtonElement;
  clearBtn: HTMLButtonElement;
  onApply: (iso: string) => void = () => {};
  onClear: (() => void) | null = null;
  /** Cached so destroy() can detach the document-level listener. */
  private onDocMouseDown: (ev: MouseEvent) => void;

  constructor() {
    this.el = document.createElement("div");
    this.el.className =
      "knot-datetime-popup fixed z-50 rounded-md border border-border bg-surface shadow-lg p-2 text-sm flex items-center gap-2";
    this.el.style.display = "none";
    this.el.dataset.testid = "datetime-popup";

    this.dateInput = document.createElement("input");
    this.dateInput.type = "date";
    this.dateInput.className = "rounded border border-border bg-bg px-2 py-1 text-fg";
    this.dateInput.dataset.testid = "datetime-date";

    this.timeInput = document.createElement("input");
    this.timeInput.type = "time";
    this.timeInput.className = "rounded border border-border bg-bg px-2 py-1 text-fg";
    this.timeInput.dataset.testid = "datetime-time";

    this.applyBtn = document.createElement("button");
    this.applyBtn.type = "button";
    this.applyBtn.textContent = "Apply";
    this.applyBtn.className =
      "rounded bg-accent px-2 py-1 text-on-accent text-xs hover:opacity-90";
    this.applyBtn.dataset.testid = "datetime-apply";

    this.clearBtn = document.createElement("button");
    this.clearBtn.type = "button";
    this.clearBtn.textContent = "Clear";
    this.clearBtn.className = "rounded px-2 py-1 text-fg-muted text-xs hover:text-fg";
    this.clearBtn.dataset.testid = "datetime-clear";
    this.clearBtn.style.display = "none";

    this.el.append(this.dateInput, this.timeInput, this.applyBtn, this.clearBtn);
    document.body.appendChild(this.el);

    this.applyBtn.addEventListener("mousedown", (ev) => {
      ev.preventDefault();
      const iso = buildIso(this.dateInput.value, this.timeInput.value);
      if (iso) this.onApply(iso);
      this.hide();
    });
    this.clearBtn.addEventListener("mousedown", (ev) => {
      ev.preventDefault();
      this.onClear?.();
      this.hide();
    });
    // Clicking outside the popup closes it without applying. Cached
    // so destroy() can remove it; otherwise every editor mount/unmount
    // would leak a listener.
    this.onDocMouseDown = (ev: MouseEvent) => {
      if (this.el.style.display === "none") return;
      if (!this.el.contains(ev.target as Node)) this.hide();
    };
    document.addEventListener("mousedown", this.onDocMouseDown);
  }

  show(
    rect: DOMRect | null,
    initial: { iso?: string } = {},
    handlers: { onApply: (iso: string) => void; onClear?: () => void },
  ) {
    this.onApply = handlers.onApply;
    this.onClear = handlers.onClear ?? null;
    if (initial.iso) {
      const split = splitIso(initial.iso);
      this.dateInput.value = split.date;
      this.timeInput.value = split.time;
      this.clearBtn.style.display = "";
    } else {
      // Default to today + nearest upcoming hour.
      const now = new Date();
      now.setMinutes(0, 0, 0);
      now.setHours(now.getHours() + 1);
      const split = splitIso(now.toISOString());
      this.dateInput.value = split.date;
      this.timeInput.value = split.time;
      this.clearBtn.style.display = "none";
    }
    if (rect) {
      this.el.style.left = `${Math.round(rect.left)}px`;
      this.el.style.top = `${Math.round(rect.bottom + 4)}px`;
    }
    this.el.style.display = "flex";
    // Focus the date input so keyboard users can edit immediately.
    this.dateInput.focus();
  }

  hide() {
    this.el.style.display = "none";
  }

  isOpen() {
    return this.el.style.display !== "none";
  }

  destroy() {
    document.removeEventListener("mousedown", this.onDocMouseDown);
    this.el.remove();
  }
}

/** Plugin key used so the click handler can find the shared popup instance. */
const dateTimePluginKey = new PluginKey("knotDateTime");

export const DateTimeExtension = Extension.create({
  name: "knotDateTime",

  addStorage() {
    return { popup: null as DateTimePopup | null };
  },

  addCommands() {
    return {
      openDateTimePicker:
        () =>
        ({ editor }) => {
          const popup = (editor.storage.knotDateTime as { popup: DateTimePopup | null }).popup;
          if (!popup) return false;
          const { from, to } = editor.state.selection;
          const coords = editor.view.coordsAtPos(from);
          const rect = {
            left: coords.left,
            top: coords.top,
            right: coords.right,
            bottom: coords.bottom,
            width: 0,
            height: coords.bottom - coords.top,
            x: coords.left,
            y: coords.top,
            toJSON: () => ({}),
          } as DOMRect;
          popup.show(rect, {}, {
            onApply: (iso) => insertDatetime(editor, { from, to }, iso),
          });
          return true;
        },
    } as Partial<RawCommands>;
  },

  addProseMirrorPlugins() {
    const editor = this.editor;
    const popup = new DateTimePopup();
    (editor.storage.knotDateTime as { popup: DateTimePopup | null }).popup = popup;
    editor.on("destroy", () => popup.destroy());

    // Plugin for click-to-edit on existing chips.
    const clickPlugin = new Plugin({
      key: dateTimePluginKey,
      props: {
        handleClickOn(view, _pos, _node, _nodePos, event) {
          const target = event.target as HTMLElement | null;
          const anchor = target?.closest("a") as HTMLAnchorElement | null;
          if (!anchor) return false;
          const href = anchor.getAttribute("href") ?? "";
          if (!href.startsWith(TIME_HREF_PREFIX)) return false;
          if (event.metaKey || event.ctrlKey) return false;
          event.preventDefault();
          const iso = href.slice(TIME_HREF_PREFIX.length);
          // Find the text node range of the link so Apply can replace it.
          const linkPos = view.posAtDOM(anchor, 0);
          const from = linkPos;
          const to = linkPos + (anchor.textContent?.length ?? 0);
          const rect = anchor.getBoundingClientRect();
          popup.show(rect, { iso }, {
            onApply: (nextIso) => {
              const text = formatLocalChip(nextIso);
              const href = `${TIME_HREF_PREFIX}${nextIso}`;
              editor
                .chain()
                .focus()
                .insertContentAt(
                  { from, to },
                  { type: "text", text, marks: [{ type: "link", attrs: { href } }] },
                )
                .run();
            },
            onClear: () => {
              editor.chain().focus().deleteRange({ from, to }).run();
            },
          });
          return true;
        },
      },
    });

    return [
      clickPlugin,
      Suggestion<{ iso: string }>({
        editor: this.editor,
        pluginKey: new PluginKey("knotDateTimeSuggestion"),
        char: "/",
        // Tiptap's Suggestion only triggers when the char before the
        // trigger matches one of `allowedPrefixes`. The default is
        // [' '], which means single `/` after a space fires but a
        // second `/` (preceded by the first one) never matches. We
        // want the *opposite* — fire only when preceded by another
        // `/`, leaving single `/` free for a future slash-command
        // system. Set allowedPrefixes to ['/'] to invert.
        allowedPrefixes: ["/"],
        // Belt-and-braces: confirm the previous char really is `/`
        // (Suggestion's prefix check uses regex character classes
        // and we want to be explicit here).
        allow: ({ state, range }) => {
          if (range.from < 1) return false;
          const before = state.doc.textBetween(range.from - 1, range.from, "\n", "\0");
          return before === "/";
        },
        items: () => [{ iso: "" }],
        command: ({ editor, range }) => {
          // Suggestion fired on the SECOND `/` (allowedPrefixes='/' means
          // the first `/` is the prefix, not part of the range). Extend
          // by 1 backward to swallow the first `/` too when we replace.
          const triggerRange = { from: range.from - 1, to: range.to };
          // Position popup near the cursor.
          const coords = editor.view.coordsAtPos(range.from);
          const rect = {
            left: coords.left,
            top: coords.top,
            right: coords.right,
            bottom: coords.bottom,
            width: 0,
            height: coords.bottom - coords.top,
            x: coords.left,
            y: coords.top,
            toJSON: () => ({}),
          } as DOMRect;
          popup.show(rect, {}, {
            onApply: (iso) => insertDatetime(editor, triggerRange, iso),
          });
        },
        // Auto-fire command on first activation so the picker pops
        // open as soon as the second `/` is typed — there is no
        // item-list to select from, the picker IS the UI. We track
        // whether we've already fired for this activation so we
        // don't repeatedly re-open the popover as the user keeps
        // typing characters after `//`.
        render: () => {
          let opened = false;
          return {
            onStart: (props) => {
              if (opened) return;
              opened = true;
              props.command(props.items[0]!);
            },
            onUpdate: () => {},
            onExit: () => {
              opened = false;
            },
            onKeyDown: () => false,
          };
        },
      }),
    ];
  },
});
