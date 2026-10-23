import { Download, LoaderCircle } from "lucide-react";
import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";

import type { ConnStatus } from "../../components/StatusDot";
import { IconButton } from "../../components/ui/IconButton";
import { useExport } from "./useExport";

type Props = {
  docId: string;
  title: string;
  isTemplate: boolean;
  status: ConnStatus;
  pendingBytes: number;
};

const ITEM_CLS =
  "flex w-full flex-col items-start gap-0.5 px-3 py-1.5 text-left text-[13px] text-fg hover:bg-muted focus-visible:bg-muted focus-visible:outline-none disabled:opacity-40 disabled:cursor-not-allowed";

/** Download-as menu in the page header: Word, OpenDocument, Markdown zips. */
export function ExportMenu({ docId, title, isTemplate, status, pendingBytes }: Props) {
  const [open, setOpen] = useState(false);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();
  const { exporting, run } = useExport({ docId, title, status, pendingBytes });

  const enabledItems = () =>
    Array.from(menuRef.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)') ?? []);

  useEffect(() => {
    if (!open) return;
    enabledItems()[0]?.focus();
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      // The trigger is excluded so its own click can toggle the menu shut;
      // otherwise mousedown would close it and the click would reopen it.
      if (menuRef.current?.contains(t) || buttonRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setOpen(false);
      buttonRef.current?.focus();
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

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

  function choose(action: () => void) {
    setOpen(false);
    buttonRef.current?.focus();
    action();
  }

  function downloadZip(descendants: boolean) {
    const a = document.createElement("a");
    a.href = `/api/docs/${docId}/export?descendants=${descendants ? "true" : "false"}`;
    a.download = `${title || "doc"}.zip`;
    a.click();
  }

  return (
    <div className="relative">
      <IconButton
        ref={buttonRef}
        data-testid="doc-export"
        label="Export"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen((v) => !v)}
      >
        {exporting ? (
          <LoaderCircle size={16} className="animate-spin" aria-hidden />
        ) : (
          <Download size={16} aria-hidden />
        )}
      </IconButton>
      {open && (
        <div
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label="Export"
          onKeyDown={onMenuKeyDown}
          className="absolute right-0 top-full z-30 mt-1 w-72 max-w-[calc(100vw-2rem)] rounded-md border border-border bg-surface shadow-lg py-1"
        >
          <button
            type="button"
            role="menuitem"
            data-testid="export-docx"
            disabled={exporting}
            onClick={() => choose(() => void run("docx"))}
            className={ITEM_CLS}
          >
            <span>Word document</span>
            <span className="text-[12px] text-fg-muted">.docx · opens in Word, LibreOffice, Google Docs</span>
          </button>
          <button
            type="button"
            role="menuitem"
            data-testid="export-odt"
            disabled={exporting}
            onClick={() => choose(() => void run("odt"))}
            className={ITEM_CLS}
          >
            <span>OpenDocument text</span>
            <span className="text-[12px] text-fg-muted">.odt · LibreOffice&apos;s own format</span>
          </button>
          {!isTemplate && (
            <>
              <div role="separator" className="my-1 border-t border-border" />
              <button
                type="button"
                role="menuitem"
                data-testid="export-md"
                onClick={() => choose(() => downloadZip(false))}
                className={ITEM_CLS}
              >
                <span>Markdown (zip)</span>
              </button>
              <button
                type="button"
                role="menuitem"
                data-testid="export-md-subtree"
                onClick={() => choose(() => downloadZip(true))}
                className={ITEM_CLS}
              >
                <span>Markdown with subpages (zip)</span>
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
