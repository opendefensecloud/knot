import { useEffect, useLayoutEffect, useMemo, useRef, type RefObject } from "react";

import { Avatar } from "../../../components/ui/Avatar";
import { Button } from "../../../components/ui/Button";
import type { HistoryEntry } from "../../../lib/validators";
import { copy } from "./copy";
import { authorName, entryLine, entryNote, timelineRows, type TimelineRow } from "./timeline";
import type { useHistoryEntries } from "./useHistoryEntries";

type Item = Exclude<TimelineRow, { type: "entry" }> | { type: "entry"; entry: HistoryEntry; line: string; note: string | null };

/**
 * The drawer's timeline column: notes about the list's state, then day
 * headings and one button per entry, newest first, then the earlier
 * versions; more pages load on demand and when the end scrolls into view.
 */
export function TimelineColumn({
  history,
  selectedId,
  onSelect,
  scrollRoot,
}: {
  history: ReturnType<typeof useHistoryEntries>;
  selectedId: number | null;
  onSelect: (entry: HistoryEntry) => void;
  /** The column that scrolls; the end of the list is watched within it. */
  scrollRoot: RefObject<HTMLElement | null>;
}) {
  const {
    entries,
    first,
    isLoading,
    isError,
    hasData,
    dataUpdatedAt,
    refetch,
    fetchNextPage,
    hasNextPage,
    isFetchingNextPage,
  } = history;

  // Wording builds date formatters per entry: once per poll, not on every
  // selection or hover. "Now" is when the data last arrived: a poll that
  // brings nothing new keeps `entries` the same object but moves
  // `dataUpdatedAt`, so `Today` and `Yesterday` follow the clock past
  // midnight, at most one poll late.
  const items: Item[] = useMemo(() => {
    if (first === null) return [];
    const now = new Date(dataUpdatedAt);
    return timelineRows(entries, {
      now,
      historySince: first.history_since,
      historyComplete: first.history_complete,
      exhausted: !hasNextPage,
    }).map((row) => (row.type === "entry" ? { ...row, line: entryLine(row.entry, now), note: entryNote(row.entry) } : row));
  }, [entries, first, hasNextPage, dataUpdatedAt]);

  // `Load more` goes away with the last page. When it had focus, focus would
  // fall to <body>, outside the dialog's Tab trap and Escape handler, so it
  // moves to the last row, where the button was.
  const refocus = useRef(false);
  useEffect(() => {
    if (!refocus.current) return;
    refocus.current = false;
    const root = scrollRoot.current;
    const rows = root?.querySelectorAll<HTMLElement>("button[data-kind]");
    const target = rows && rows.length > 0 ? rows[rows.length - 1]! : root?.closest<HTMLElement>('[role="dialog"]');
    target?.focus();
  });

  const sentinelRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const sentinel = sentinelRef.current;
    if (!sentinel || !hasNextPage || typeof IntersectionObserver === "undefined") return;
    let live = true;
    const observer = new IntersectionObserver(
      (records) => {
        if (!live || isFetchingNextPage) return;
        if (records.some((r) => r.isIntersecting)) void fetchNextPage();
      },
      { root: scrollRoot.current, rootMargin: "0px 0px 200px 0px" },
    );
    observer.observe(sentinel);
    return () => {
      live = false;
      observer.disconnect();
    };
  }, [hasNextPage, isFetchingNextPage, fetchNextPage, scrollRoot]);

  return (
    <>
      <div aria-live="polite" className="px-3 py-2 empty:p-0 grid gap-2 text-[13px] text-fg-muted">
        {isLoading && <p className="m-0">{copy.loading}</p>}
        {isError && !hasData && (
          <div data-testid="history-error" className="grid gap-2 justify-items-start">
            <p className="m-0">{copy.loadError}</p>
            <Button type="button" size="sm" onClick={() => void refetch()}>
              {copy.retry}
            </Button>
          </div>
        )}
        {isError && hasData && (
          <p data-testid="history-refresh-error" className="m-0">
            {copy.refreshError}
          </p>
        )}
        {first?.catching_up && (
          <p data-testid="history-catching-up" className="m-0">
            {copy.catchingUp}
          </p>
        )}
        {first?.unavailable && (
          <p data-testid="history-unavailable" className="m-0">
            {copy.unavailable}
          </p>
        )}
        {hasData && entries.length === 0 && (
          <p data-testid="history-empty" className="m-0">
            {copy.empty}
          </p>
        )}
      </div>
      <ol className="list-none m-0 p-0">
        {items.map((item) => {
          switch (item.type) {
            case "day":
              return (
                <li key={item.key} data-testid="history-day">
                  <h3 className="m-0 px-3 pt-3 pb-1 text-[12px] font-semibold text-fg-muted">{item.label}</h3>
                </li>
              );
            case "earlier":
              return (
                <li key="earlier" data-testid="history-earlier">
                  <h3 className="m-0 px-3 pt-4 pb-1 text-[12px] font-semibold text-fg-muted">{copy.earlierSection}</h3>
                </li>
              );
            case "since":
              return (
                <li key="since" data-testid="history-since" className="px-3 py-3 text-[12px] text-fg-muted">
                  {item.label}
                </li>
              );
            case "entry":
              return (
                <li key={item.entry.id}>
                  <EntryRow
                    entry={item.entry}
                    line={item.line}
                    note={item.note}
                    selected={item.entry.id === selectedId}
                    onSelect={onSelect}
                  />
                </li>
              );
          }
        })}
      </ol>
      {hasNextPage && (
        <>
          <div ref={sentinelRef} data-testid="history-sentinel" aria-hidden className="h-px" />
          <LoadMore
            busy={isFetchingNextPage}
            onLoad={() => void fetchNextPage()}
            onRemovedWithFocus={() => {
              refocus.current = true;
            }}
          />
        </>
      )}
    </>
  );
}

/**
 * The `Load more` button. Not `disabled` while a page loads: that would drop
 * focus out of the dialog, past its Tab trap and Escape handler. When it is
 * removed while it has focus it says so, so the column can move focus.
 */
function LoadMore({
  busy,
  onLoad,
  onRemovedWithFocus,
}: {
  busy: boolean;
  onLoad: () => void;
  onRemovedWithFocus: () => void;
}) {
  const ref = useRef<HTMLButtonElement>(null);
  const removed = useRef(onRemovedWithFocus);
  useLayoutEffect(() => {
    removed.current = onRemovedWithFocus;
  });
  // A layout cleanup on unmount runs while the button is still in the
  // document, so it can still tell whether it has focus.
  useLayoutEffect(() => {
    const button = ref.current;
    return () => {
      if (button !== null && document.activeElement === button) removed.current();
    };
  }, []);
  return (
    <div className="px-3 py-3">
      <Button
        ref={ref}
        type="button"
        size="sm"
        data-testid="history-load-more"
        aria-busy={busy || undefined}
        onClick={() => {
          if (!busy) onLoad();
        }}
      >
        {copy.loadMore}
      </Button>
    </div>
  );
}

function EntryRow({
  entry,
  line,
  note,
  selected,
  onSelect,
}: {
  entry: HistoryEntry;
  line: string;
  note: string | null;
  selected: boolean;
  onSelect: (entry: HistoryEntry) => void;
}) {
  const { author } = entry;
  return (
    <button
      type="button"
      data-testid={`history-entry-${entry.id}`}
      data-kind={entry.kind}
      aria-current={selected ? "true" : undefined}
      onClick={() => onSelect(entry)}
      className={`flex w-full items-start gap-2 text-left px-3 py-2 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent ${
        selected ? "bg-muted text-fg" : "text-fg hover:bg-muted/60"
      }`}
    >
      <Avatar
        seed={author?.key ?? ""}
        name={authorName(author)}
        muted={author === null || author.key === null}
      />
      <span className="min-w-0 flex-1">
        <span className="block text-[13px] leading-5">{line}</span>
        {note !== null && (
          <span data-testid="history-entry-note" className="block text-[12px] text-fg-muted">
            {note}
          </span>
        )}
      </span>
    </button>
  );
}
