import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";

import { Button } from "../../components/ui/Button";

/** Shown when a duplicate fails for any reason other than its size. */
const DUPLICATE_FAILED = "Couldn't duplicate this page.";

/** Shown on a 413 (`doc.duplicate_too_large`): the server's caps,
 *  `MAX_DUPLICATE_PAGES` and `MAX_DUPLICATE_BLOB_BYTES`. */
const DUPLICATE_TOO_LARGE = "This page is too large to duplicate (over 200 pages or 200 MB of files).";

/** Shown on a 503 `doc.duplicate_busy`: the server makes only a few copies
 *  at once, and none came free in time. Another 503 isn't the server's. */
const DUPLICATE_BUSY = "Too many copies are being made right now. Try again in a moment.";

/** What to tell the user when a duplicate failed with this error, or with
 *  no answer at all. */
export function duplicateErrorText(error?: { status: number; code: string }): string {
  if (error?.status === 413) return DUPLICATE_TOO_LARGE;
  if (error?.code === "doc.duplicate_busy") return DUPLICATE_BUSY;
  return DUPLICATE_FAILED;
}

/** The controls Tab moves between. */
const FOCUSABLE = "input:not(:disabled), button:not(:disabled)";

/**
 * Asks whether a page's subpages are copied with it. The sidebar shows it
 * only for a page that has subpages; the box starts unticked.
 *
 * It is modal: Escape, Cancel and a click beside it close it, and Tab keeps
 * to its controls. The caller puts focus back where the dialog came from.
 */
export function DuplicateDialog({
  title,
  subpages,
  pending,
  error,
  onCancel,
  onConfirm,
}: {
  /** The page's title. */
  title: string;
  /** How many subpages it has, at every depth. */
  subpages: number;
  /** A copy is being made: Duplicate shows as unavailable. The caller
   *  ignores presses meanwhile itself, because this prop arrives a render
   *  too late to stop a double click. */
  pending: boolean;
  /** Why the last attempt failed, or null. */
  error: string | null;
  onCancel: () => void;
  onConfirm: (includeSubpages: boolean) => void;
}) {
  const [include, setInclude] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const boxId = useId();
  const hintId = useId();

  useEffect(() => {
    ref.current?.querySelector<HTMLElement>(FOCUSABLE)?.focus();
  }, []);

  /** Tab and Shift+Tab go round the dialog's controls, never out of it. */
  function trapTab(e: KeyboardEvent) {
    const controls = Array.from(ref.current?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? []);
    if (controls.length === 0) return;
    e.preventDefault();
    const at = controls.indexOf(document.activeElement as HTMLElement);
    const step = e.shiftKey ? -1 : 1;
    const next = at === -1 ? (e.shiftKey ? controls.length - 1 : 0) : (at + step + controls.length) % controls.length;
    controls[next]!.focus();
  }

  return (
    <div
      className="fixed inset-0 z-50 bg-black/40 backdrop-blur-sm flex items-center justify-center p-4"
      onClick={onCancel}
    >
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid="duplicate-dialog"
        // Focusable but not a tab stop: a click on its text focuses the
        // dialog, not <body>, so its keys still come here.
        tabIndex={-1}
        className="w-[320px] max-w-full grid gap-3 p-4 rounded-lg bg-surface border border-border shadow-xl outline-none"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            onCancel();
          } else if (e.key === "Tab") {
            trapTab(e);
          }
        }}
      >
        <h2 id={titleId} className="m-0 text-[15px] font-semibold text-fg">
          Duplicate “{title}”
        </h2>
        <div className="flex items-start gap-2 text-sm text-fg">
          <input
            id={boxId}
            type="checkbox"
            data-testid="duplicate-subpages"
            checked={include}
            onChange={(e) => setInclude(e.target.checked)}
            aria-describedby={hintId}
            className="mt-0.5 h-4 w-4 shrink-0 accent-accent"
          />
          <div>
            <label htmlFor={boxId} className="cursor-pointer">
              Include its {subpages} {subpages === 1 ? "subpage" : "subpages"}
            </label>
            <div id={hintId} className="text-xs text-fg-muted">
              Every subpage is copied with all its content, drawings and files.
            </div>
          </div>
        </div>
        <p className="m-0 text-xs text-fg-muted">
          The copy gets its own drawings and files, so it keeps working if the original is archived.
        </p>
        {error && (
          <p role="alert" className="m-0 text-sm text-destructive">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button size="sm" className="px-3" onClick={onCancel}>
            Cancel
          </Button>
          {/* Not `disabled` while pending: that would drop focus out of the
              dialog, past the Tab trap and the Escape handler. */}
          <Button
            variant="primary"
            size="sm"
            className="px-3 aria-disabled:opacity-50 aria-disabled:cursor-wait"
            data-testid="duplicate-confirm"
            aria-disabled={pending}
            onClick={() => onConfirm(include)}
          >
            Duplicate
          </Button>
        </div>
      </div>
    </div>
  );
}
