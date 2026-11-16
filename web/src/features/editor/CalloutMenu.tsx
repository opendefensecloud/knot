import { MessageSquareWarning } from "lucide-react";
import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";

import type { Editor } from "@tiptap/react";

import { CALLOUT_ICON, CALLOUT_KINDS, CALLOUT_LABEL, calloutKind } from "./callouts";

const ITEM_CLS =
  "flex w-full items-center gap-2 px-3 py-1.5 text-left text-[13px] text-fg hover:bg-muted focus-visible:bg-muted focus-visible:outline-none";

/**
 * The toolbar's callout button: a menu of the five kinds. Picked with the
 * caret in a box, it changes the box's kind; in a plain quote, it makes the
 * quote a box; elsewhere it wraps the block at the caret in a new box.
 */
export function CalloutMenu({ editor, btnClass }: { editor: Editor; btnClass: string }) {
  const [open, setOpen] = useState(false);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();
  const current = calloutKind(editor.getAttributes("blockquote").kind);

  const items = () => Array.from(menuRef.current?.querySelectorAll<HTMLElement>('[role="menuitemradio"]') ?? []);

  useEffect(() => {
    if (!open) return;
    items()[0]?.focus();
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      // The trigger is excluded so its own click can toggle the menu shut.
      if (menuRef.current?.contains(t) || buttonRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setOpen(false);
      editor.commands.focus();
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, editor]);

  function onMenuKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Tab") {
      setOpen(false);
      return;
    }
    const all = items();
    const i = all.indexOf(document.activeElement as HTMLElement);
    let next: number;
    if (e.key === "ArrowDown") next = i < 0 ? 0 : (i + 1) % all.length;
    else if (e.key === "ArrowUp") next = i <= 0 ? all.length - 1 : i - 1;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = all.length - 1;
    else return;
    e.preventDefault();
    all[next]?.focus();
  }

  return (
    <div className="relative">
      <button
        ref={buttonRef}
        type="button"
        data-testid="toolbar-callout"
        title="Callout"
        aria-label="Callout"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        aria-pressed={current !== null}
        onClick={() => setOpen(!open)}
        className={`${btnClass} ${open || current ? "bg-muted text-fg" : ""}`}
      >
        <MessageSquareWarning size={15} aria-hidden />
      </button>
      {open && (
        <div
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label="Callout"
          onKeyDown={onMenuKeyDown}
          className="absolute left-0 top-full z-30 mt-1 w-40 max-w-[calc(100vw-2rem)] rounded-md border border-border bg-surface shadow-lg py-1"
        >
          {CALLOUT_KINDS.map((kind) => {
            const Icon = CALLOUT_ICON[kind];
            return (
              <button
                key={kind}
                type="button"
                role="menuitemradio"
                aria-checked={current === kind}
                data-testid={`toolbar-callout-${kind}`}
                onClick={() => {
                  setOpen(false);
                  editor.chain().focus().setCallout(kind).run();
                }}
                className={ITEM_CLS}
              >
                <Icon size={15} aria-hidden />
                <span>{CALLOUT_LABEL[kind]}</span>
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
