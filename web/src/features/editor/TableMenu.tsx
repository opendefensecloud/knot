import { Columns3, Rows3, Table as TableIcon, Trash2 } from "lucide-react";
import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

import type { Editor } from "@tiptap/react";

const ITEM_CLS =
  "flex w-full items-center gap-2 px-3 py-1.5 text-left text-[13px] hover:bg-muted focus-visible:bg-muted focus-visible:outline-none disabled:opacity-40 disabled:cursor-not-allowed";
/** Space the menu keeps from the viewport's edges, in px. */
const VIEWPORT_GUTTER_PX = 16;

type Item = {
  testId: string;
  label: string;
  icon: ReactNode;
  destructive?: boolean;
  run: (editor: Editor) => void;
};

const ITEMS: Item[] = [
  { testId: "toolbar-table-add-row", label: "Add row below", icon: <Rows3 size={15} aria-hidden />,
    run: (e) => e.chain().focus().addRowAfter().run() },
  { testId: "toolbar-table-delete-row", label: "Delete row", icon: <Rows3 size={15} aria-hidden />,
    run: (e) => e.chain().focus().deleteRow().run() },
  { testId: "toolbar-table-add-col", label: "Add column right", icon: <Columns3 size={15} aria-hidden />,
    run: (e) => e.chain().focus().addColumnAfter().run() },
  { testId: "toolbar-table-delete-col", label: "Delete column", icon: <Columns3 size={15} aria-hidden />,
    run: (e) => e.chain().focus().deleteColumn().run() },
  { testId: "toolbar-table-delete", label: "Delete table", icon: <Trash2 size={15} aria-hidden />, destructive: true,
    run: (e) => e.chain().focus().deleteTable().run() },
];

/**
 * The toolbar's table button: "Insert table" outside a table, a menu of row,
 * column and table actions inside one.
 *
 * Both take the one slot. The actions used to be five more buttons that
 * appeared when the caret entered a table, which wrapped the toolbar onto a
 * second row and pushed the page down under the pointer: a drag across cells
 * then selected the cells a row above, and every click into a table made the
 * page jump. Inside a table there is also nothing to insert: GFM has no
 * table in a table, and the export would write one as escaped text.
 */
export function TableMenu({ editor, btnClass }: { editor: Editor; btnClass: string }) {
  const [menuOpen, setOpen] = useState(false);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();
  const inTable = editor.isActive("table");
  // Leaving the table (a remote edit can delete it) takes the menu's subject
  // away; the menu then neither shows nor comes back on return.
  const open = menuOpen && inTable;

  const enabledItems = () =>
    Array.from(menuRef.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)') ?? []);

  useEffect(() => {
    if (!open) return;
    enabledItems()[0]?.focus();
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

  // The menu hangs from the trigger's left edge; on a narrow toolbar that can
  // run past the viewport's right edge.
  useLayoutEffect(() => {
    const menu = menuRef.current;
    if (!open || !menu) return;
    const { left, right, width } = menu.getBoundingClientRect();
    if (width === 0) return;
    const max = document.documentElement.clientWidth - VIEWPORT_GUTTER_PX;
    const dx = left < VIEWPORT_GUTTER_PX ? VIEWPORT_GUTTER_PX - left : right > max ? max - right : 0;
    if (dx !== 0) menu.style.transform = `translateX(${dx}px)`;
  }, [open]);

  if (!inTable) {
    return (
      <button
        type="button"
        data-testid="toolbar-table"
        title="Insert table"
        aria-label="Insert table"
        onClick={() => {
          setOpen(false);
          editor.chain().focus().insertTable({ rows: 3, cols: 3, withHeaderRow: true }).run();
        }}
        className={btnClass}
      >
        <TableIcon size={15} aria-hidden />
      </button>
    );
  }

  function onMenuKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Tab") {
      setOpen(false);
      return;
    }
    const items = enabledItems();
    if (items.length === 0) return;
    const i = items.indexOf(document.activeElement as HTMLElement);
    let next: number;
    if (e.key === "ArrowDown") next = i < 0 ? 0 : (i + 1) % items.length;
    else if (e.key === "ArrowUp") next = i <= 0 ? items.length - 1 : i - 1;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = items.length - 1;
    else return;
    e.preventDefault();
    items[next]?.focus();
  }

  return (
    <div className="relative">
      <button
        ref={buttonRef}
        type="button"
        data-testid="toolbar-table-menu"
        title="Table"
        aria-label="Table"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen(!open)}
        className={`${btnClass} ${open ? "bg-muted text-fg" : ""}`}
      >
        <TableIcon size={15} aria-hidden />
      </button>
      {open && (
        <div
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label="Table"
          onKeyDown={onMenuKeyDown}
          className="absolute left-0 top-full z-30 mt-1 w-48 max-w-[calc(100vw-2rem)] rounded-md border border-border bg-surface shadow-lg py-1"
        >
          {ITEMS.map((item) => (
            <button
              key={item.testId}
              type="button"
              role="menuitem"
              data-testid={item.testId}
              onClick={() => {
                setOpen(false);
                item.run(editor);
              }}
              className={`${ITEM_CLS} ${item.destructive ? "text-destructive" : "text-fg"}`}
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
