import { createPortal } from "react-dom";
import { useEffect, useRef } from "react";

export type ContextMenuItem = {
  label: string;
  testId: string;
  destructive?: boolean;
  disabled?: boolean;
  onSelect: () => void;
};

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
      className="fixed z-50 min-w-[160px] py-1 rounded-md bg-surface border border-border shadow-lg"
      style={{ top: y, left: x }}
      // The portal moves the menu in the DOM, not in the React tree: its
      // events still bubble to the component that rendered it. In the
      // sidebar that is a draggable row, whose sensors start a drag on
      // mousedown and touchstart, and whose keys must not be taken from
      // the menu. Stopping here also stops the native event at the portal
      // container, before the document listeners above, so Escape is
      // handled here too.
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
