import { X } from "lucide-react";
import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";

import { IconButton } from "../../../components/ui/IconButton";
import type { HistoryEntry } from "../../../lib/validators";
import { copy } from "./copy";
import { TimelineColumn } from "./TimelineColumn";
import { useHistoryEntries } from "./useHistoryEntries";
import { VersionView } from "./VersionView";

/** What Tab can reach. Hidden ones (the column not shown below `md`) are
 *  filtered out where the browser can tell. */
const TABBABLE =
  'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

function tabbables(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(TABBABLE)).filter((el) =>
    typeof el.checkVisibility === "function" ? el.checkVisibility() : true,
  );
}

/**
 * The page's history: a modal panel from the right with the timeline of
 * editing sessions and, beside it (below `md`, instead of it), the version
 * of the selected entry. The backdrop, the close button and Escape close
 * it, and so does a successful restore; focus starts on the close button,
 * stays in the panel, and goes back to what had it before the drawer
 * opened.
 */
export function HistoryDrawer({
  docId,
  canRestore,
  onClose,
}: {
  docId: string;
  canRestore: boolean;
  onClose: () => void;
}) {
  const history = useHistoryEntries(docId);
  // The clicked entry, resolved against the list on every render so a
  // poll's newer end and counts reach the version pane; an entry no longer
  // listed keeps the clicked one until the next selection.
  const [selected, setSelected] = useState<HistoryEntry | null>(null);
  const [showChanges, setShowChanges] = useState(true);
  const entry = selected === null ? null : (history.entries.find((e) => e.id === selected.id) ?? selected);

  const titleId = useId();
  const panelRef = useRef<HTMLDivElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const timelineRef = useRef<HTMLDivElement>(null);

  // Below `md` the drawer shows one column at a time and hides the one that
  // held the control just activated, so focus would fall to <body>, outside
  // the dialog. Selecting an entry focuses Back (`focusBack`); Back focuses
  // the entry again once the timeline shows. From `md` up both columns stay
  // and nothing moves. Without `matchMedia` (jsdom) it is wide.
  const narrow =
    entry !== null && typeof window.matchMedia === "function" && !window.matchMedia("(min-width: 768px)").matches;
  const returnTo = useRef<number | null>(null);
  function onBack() {
    returnTo.current = selected?.id ?? null;
    setSelected(null);
  }
  useEffect(() => {
    if (selected !== null || returnTo.current === null) return;
    const id = returnTo.current;
    returnTo.current = null;
    timelineRef.current?.querySelector<HTMLElement>(`[data-testid="history-entry-${id}"]`)?.focus();
  }, [selected]);

  // Focus moves in on open and back to the opener (the History button) on
  // close, however the drawer closes.
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    closeRef.current?.focus();
    return () => {
      if (opener?.isConnected) opener.focus();
    };
  }, []);

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Escape") {
      e.stopPropagation();
      onClose();
      return;
    }
    if (e.key !== "Tab" || !panelRef.current) return;
    // Tab and Shift+Tab go round the panel's controls, never out of it.
    const controls = tabbables(panelRef.current);
    if (controls.length === 0) return;
    const firstControl = controls[0]!;
    const lastControl = controls[controls.length - 1]!;
    const at = document.activeElement;
    if (e.shiftKey && (at === firstControl || at === panelRef.current)) {
      e.preventDefault();
      lastControl.focus();
    } else if (!e.shiftKey && at === lastControl) {
      e.preventDefault();
      firstControl.focus();
    }
  }

  return (
    <>
      <div data-testid="history-backdrop" className="fixed inset-0 z-40 bg-black/30" onClick={onClose} />
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid="history-drawer"
        // Focusable but not a tab stop: a click on its text focuses the
        // panel, not <body>, so its keys still come here.
        tabIndex={-1}
        onKeyDown={onKeyDown}
        className="fixed right-0 top-0 h-dvh z-40 w-[min(1100px,92vw)] max-md:w-full bg-surface border-l border-border shadow-xl flex flex-col outline-none"
      >
        <header className="flex items-center gap-2 px-4 py-3 border-b border-border">
          <h2 id={titleId} className="m-0 flex-1 text-base font-semibold text-fg">
            {copy.title}
          </h2>
          <IconButton ref={closeRef} data-testid="history-close" label={copy.close} size="sm" onClick={onClose}>
            <X size={14} aria-hidden />
          </IconButton>
        </header>
        <div className="flex flex-1 min-h-0">
          <div
            ref={timelineRef}
            data-testid="history-timeline"
            className={`w-full md:w-[340px] shrink-0 overflow-auto md:border-r border-border ${
              entry === null ? "" : "max-md:hidden"
            }`}
          >
            <TimelineColumn
              history={history}
              selectedId={entry?.id ?? null}
              onSelect={setSelected}
              scrollRoot={timelineRef}
            />
          </div>
          <div
            data-testid="history-version-pane"
            className={`flex-1 min-w-0 overflow-auto flex flex-col ${entry === null ? "max-md:hidden" : ""}`}
          >
            {entry === null ? (
              <p className="m-0 p-4 text-sm text-fg-muted">{copy.selectEntry}</p>
            ) : (
              <VersionView
                docId={docId}
                entry={entry}
                showChanges={showChanges}
                onShowChanges={setShowChanges}
                canRestore={canRestore}
                onBack={onBack}
                onRestored={onClose}
                focusBack={narrow}
              />
            )}
          </div>
        </div>
      </div>
    </>
  );
}
