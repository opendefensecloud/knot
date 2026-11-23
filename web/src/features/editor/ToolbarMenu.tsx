import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

import type { Editor } from "@tiptap/react";

const ITEM_CLS =
  "flex w-full items-center gap-2 px-3 py-1.5 text-left text-[13px] text-fg hover:bg-muted focus-visible:bg-muted focus-visible:outline-none";

export type ToolbarMenuItem = {
  testId: string;
  label: string;
  icon: ReactNode;
  /** For a menu of choices: whether this one is the current one. */
  checked?: boolean;
  run: () => void;
};

type Props = {
  editor: Editor;
  btnClass: string;
  testId: string;
  label: string;
  icon: ReactNode;
  /** Highlights the trigger, e.g. while the caret is in what it acts on. */
  active?: boolean;
  items: ToolbarMenuItem[];
};

/**
 * A toolbar button that opens a menu: several actions in one slot, so the
 * toolbar stays one row at the default width (doc-dock.spec.ts). Arrow
 * keys, Home and End move between items; Escape on the button or in the
 * menu closes and returns to the text; a click outside or Tab closes.
 */
export function ToolbarMenu({ editor, btnClass, testId, label, icon, active = false, items }: Props) {
  const [open, setOpen] = useState(false);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();
  const radio = items.some((item) => item.checked !== undefined);

  const entries = () =>
    Array.from(menuRef.current?.querySelectorAll<HTMLElement>('[role^="menuitem"]') ?? []);

  useEffect(() => {
    if (!open) return;
    entries()[0]?.focus();
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      // The trigger is excluded so its own click can toggle the menu shut.
      if (menuRef.current?.contains(t) || buttonRef.current?.contains(t)) return;
      setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  /** Escape on the trigger or in the menu (not elsewhere, e.g. a dialog
   *  over it) closes the menu and returns to the text. */
  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key !== "Escape" || !open) return;
    e.preventDefault();
    setOpen(false);
    editor.commands.focus();
  }

  function onMenuKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Tab") {
      setOpen(false);
      return;
    }
    const all = entries();
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
    <div className="relative" onKeyDown={onKeyDown}>
      <button
        ref={buttonRef}
        type="button"
        data-testid={testId}
        title={label}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen(!open)}
        className={`${btnClass} ${open || active ? "bg-muted text-fg" : ""}`}
      >
        {icon}
      </button>
      {open && (
        <div
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label={label}
          onKeyDown={onMenuKeyDown}
          className="absolute left-0 top-full z-30 mt-1 w-48 max-w-[calc(100vw-2rem)] rounded-md border border-border bg-surface shadow-lg py-1"
        >
          {items.map((item) => (
            <button
              key={item.testId}
              type="button"
              role={radio ? "menuitemradio" : "menuitem"}
              aria-checked={radio ? item.checked === true : undefined}
              data-testid={item.testId}
              onClick={() => {
                setOpen(false);
                item.run();
              }}
              className={ITEM_CLS}
            >
              {item.icon}
              <span>{item.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
