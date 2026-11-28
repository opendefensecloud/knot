import { createPortal } from "react-dom";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

export type ContextMenuItem = {
  label: string;
  testId: string;
  destructive?: boolean;
  disabled?: boolean;
  onSelect: () => void;
};

/** Space kept between a menu moved up and the bottom of the window, in px. */
const VIEWPORT_GAP = 8;

export function ContextMenu({
  x,
  y,
  items,
  onClose,
}: {
  x: number;
  y: number;
  items: ContextMenuItem[];
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // Opened near the bottom of the window (the last rows of the sidebar), the
  // menu would run past it and its lower items would be out of reach: measure
  // it before paint and move it up until it fits, keeping a small gap.
  const [top, setTop] = useState(y);
  useLayoutEffect(() => {
    const height = ref.current?.getBoundingClientRect().height ?? 0;
    const overflow = y + height + VIEWPORT_GAP - window.innerHeight;
    setTop(overflow > 0 ? Math.max(VIEWPORT_GAP, y - overflow) : y);
  }, [y, items.length]);

  useEffect(() => {
    const onDoc = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  // Portalled to the end of <body>, the menu is out of reach by Tab; take
  // focus so keyboard users land on its first item.
  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  }, []);

  function moveFocus(step: 1 | -1) {
    const buttons = Array.from(
      ref.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [],
    );
    if (buttons.length === 0) return;
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    buttons[(at + step + buttons.length) % buttons.length]!.focus();
  }

  return createPortal(
    <div
      ref={ref}
      role="menu"
      data-testid="context-menu"
      className="fixed z-50 min-w-[160px] max-h-[calc(100vh-16px)] overflow-y-auto py-1 rounded-md bg-surface border border-border shadow-lg"
      style={{ top, left: x }}
      // The portal moves the menu in the DOM, not in the React tree: its
      // events still bubble through the components that rendered it. The
      // sidebar renders it next to its draggable row, not inside it, so they
      // reach the row's <li> and what encloses it, never the row's drag
      // listeners. That <li> cancels a contextmenu bubbling up from here, so
      // the browser's own menu can't open over this one; it handles nothing
      // else. The stops are defensive: rendered inside a drag source, a
      // mousedown or touchstart would start a drag, and a key handler there
      // could take the menu's keys. Stopping here also stops the native
      // event at the portal container, before the document listeners above,
      // so Escape is handled here too.
      onKeyDown={(e) => {
        if (e.key === "Escape") onClose();
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          moveFocus(e.key === "ArrowDown" ? 1 : -1);
        }
        e.stopPropagation();
      }}
      onPointerDown={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onTouchStart={(e) => e.stopPropagation()}
    >
      {items.map((it) => (
        <button
          key={it.testId}
          type="button"
          role="menuitem"
          data-testid={it.testId}
          disabled={it.disabled}
          onClick={() => {
            onClose();
            it.onSelect();
          }}
          className={`block w-full text-left text-sm px-3 py-1.5 transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
            it.destructive
              ? "text-destructive hover:bg-destructive/10"
              : "text-fg hover:bg-muted"
          }`}
        >
          {it.label}
        </button>
      ))}
    </div>,
    document.body,
  );
}
