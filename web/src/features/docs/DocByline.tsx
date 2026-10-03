import { useQuery } from "@tanstack/react-query";
import { useEffect, useId, useRef, useState } from "react";

import { Avatar } from "../../components/ui/Avatar";
import { relTime } from "../../lib/relTime";
import type { DocContributor } from "../../lib/validators";

import { docsApi } from "./docs.api";

const DATE_OPTS: Intl.DateTimeFormatOptions = { day: "numeric", month: "short", year: "numeric" };
const MAX_AVATARS = 3;
// `contributors_since` is the documents row's DEFAULT now() and `created_at`
// is stamped by the same insert, so for a doc created after tracking began the
// two agree to within a statement. Anything further apart means the doc
// predates tracking and its early edits were never attributed.
const TRACKING_GAP_MS = 60_000;

function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, DATE_OPTS);
}

/**
 * "Created by Alice · 3 Mar 2026 · (avatars) 4 contributors", under the title.
 *
 * Shown to every role that can open the page. It is supporting information,
 * so it never toasts: while loading it only holds its line (so the action row
 * below does not jump when it arrives) and on error it renders nothing.
 */
export function DocByline({ docId }: { docId: string }) {
  const [open, setOpen] = useState(false);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const popoverId = useId();

  const q = useQuery({
    queryKey: ["contributors", docId],
    queryFn: () => docsApi.contributors(docId),
  });

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      // The button is excluded so its own click can toggle the popover shut;
      // otherwise mousedown would close it and the click would reopen it.
      if (popoverRef.current?.contains(t) || buttonRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
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

  // Same height as a line holding the sm avatars, so the loaded byline
  // replaces the placeholder without shifting anything below it.
  const lineCls = "relative mt-1 min-h-5 flex flex-wrap items-center gap-x-1.5 text-[13px] text-fg-muted";

  if (q.isPending) return <div data-testid="doc-byline" aria-hidden className={lineCls} />;
  if (!q.data || "error" in q.data) return null;

  const { created_by, created_at, contributors_since, contributors } = q.data.ok;
  const n = contributors.length;
  const showFootnote =
    new Date(contributors_since).getTime() - new Date(created_at).getTime() > TRACKING_GAP_MS;

  function toggle() {
    if (!open) void q.refetch();
    setOpen((v) => !v);
  }

  return (
    <div data-testid="doc-byline" className={lineCls}>
      <span data-testid="doc-byline-creator">
        Created by {created_by.display_name} · {formatDate(created_at)}
      </span>
      {n > 0 && (
        <>
          <span aria-hidden>·</span>
          <button
            ref={buttonRef}
            type="button"
            data-testid="doc-contributors-button"
            aria-haspopup="dialog"
            aria-expanded={open}
            aria-controls={popoverId}
            onClick={toggle}
            className="inline-flex items-center gap-1.5 rounded px-1 -mx-1 hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
          >
            <span className="inline-flex items-center">
              {contributors.slice(0, MAX_AVATARS).map((c, i) => (
                <span
                  key={c.user_id}
                  // Overlap slightly; the ring in the page colour keeps each
                  // initial legible where the next one covers it.
                  className={`inline-flex rounded-full ring-2 ring-bg ${i > 0 ? "-ml-1.5" : ""}`}
                >
                  <Avatar name={c.display_name} seed={c.user_id} />
                </span>
              ))}
            </span>
            <span>
              {n} {n === 1 ? "contributor" : "contributors"}
            </span>
          </button>
        </>
      )}
      {open && n > 0 && (
        <div
          ref={popoverRef}
          id={popoverId}
          role="dialog"
          aria-label="Contributors"
          data-testid="doc-contributors-popover"
          // Anchored to the byline, not the button: the byline spans the
          // column, so `max-w-full` keeps the popover on-screen at phone
          // width wherever the button happened to wrap to.
          className="absolute left-0 top-full z-30 mt-1 w-72 max-w-full rounded-md border border-border bg-surface shadow-lg py-1"
        >
          <ul className="list-none m-0 p-0 max-h-72 overflow-auto">
            {contributors.map((c) => (
              <ContributorRow key={c.user_id} c={c} />
            ))}
          </ul>
          {showFootnote && (
            <p className="m-0 px-3 pt-1.5 pb-1 border-t border-border text-[12px] text-fg-muted">
              Edits before {formatDate(contributors_since)} aren&apos;t listed.
            </p>
          )}
        </div>
      )}
    </div>
  );
}

function ContributorRow({ c }: { c: DocContributor }) {
  return (
    <li
      data-testid={`doc-contributor-${c.user_id}`}
      className="flex items-center gap-2 px-3 py-1.5 text-[13px]"
    >
      <Avatar name={c.display_name} seed={c.user_id} />
      <span className="flex-1 min-w-0 truncate text-fg">{c.display_name}</span>
      <span className="shrink-0 text-fg-muted" title={new Date(c.last_edited_at).toLocaleString()}>
        edited {relTime(c.last_edited_at)}
      </span>
    </li>
  );
}
