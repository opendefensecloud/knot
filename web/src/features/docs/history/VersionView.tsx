import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";
import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";

import { Button } from "../../../components/ui/Button";
import { unwrapResult, type ApiError } from "../../../lib/api";
import { historyApi } from "../../../lib/history.api";
import { LIMIT_TOAST_KEY, limitErrorText, notifyFailure } from "../../../lib/limitErrors";
import type { HistoryEntry, HistoryView } from "../../../lib/validators";
import { useUi } from "../../../stores/ui";
import { docsApi } from "../docs.api";
import { duplicateErrorText } from "../DuplicateDialog";
import { markDocEditMode } from "../editMode";
import { copy } from "./copy";
import { HistoryDocViewer } from "./HistoryDocViewer";
import { entryLine, formatDateTime, showsChanges } from "./timeline";

/** A view and the `ended_at` its query was fetched for: while a placeholder
 *  stands in, `endedAt` is the older version's, so it names what is on
 *  screen. */
type ViewData = { view: HistoryView; endedAt: string };

/** What an `unwrapResult` error carries; a rejection of `apiFetch` (parse or
 *  network error, abort) has neither. */
type Failure = { status?: number; code?: string };

/** A 503 is retried every 5 s (the `Retry-After` the server always sends;
 *  `apiFetch` drops headers), three times. */
const BUSY_RETRIES = 3;
const BUSY_RETRY_DELAY_MS = 5_000;

/** Why the version can't be shown, from the view's final error. */
function viewErrorText(error: unknown): string {
  const { status, code } = (error ?? {}) as Failure;
  if (code === "history.rebuilding") return copy.rebuilding;
  if (status === 409) return copy.tooLarge;
  if (status === 503) return copy.busy;
  if (status === 404) return copy.notFound;
  return copy.cantShow;
}

/** The toast of a finished Restore here, by what it did. */
const RESTORED_TOASTS = {
  restored: ["info", copy.restored],
  fallback: ["warn", copy.restoredFallback],
  unchanged: ["info", copy.restoreUnchanged],
} as const;

/**
 * The version pane: the page as it was right after `entry`, read-only, under
 * the page's current title, with a bar holding the entry's line, the
 * "Show changes" switch, "Restore here" and "Restore as new page".
 *
 * The view's key carries the entry's `ended_at`, so an open session the poll
 * extended is fetched again; until it arrives the same entry's previous
 * version stays on screen (`aria-busy`, Restore disabled), while another
 * entry never shows the previous one's text. A superseded request is
 * aborted, and the server drops its render.
 *
 * "Restore as new page" shows beside "Restore here" only when the sidebar's
 * list (`["docs"]`) holds the page with a visible parent and not as a
 * template, as the sidebar offers Duplicate; while the list loads, failed
 * or lacks the page, it stays hidden, and a page too large to restore here
 * is then not sent to it.
 */
export function VersionView({
  docId,
  entry,
  showChanges,
  onShowChanges,
  canRestore,
  onBack,
  onRestored,
  focusBack,
}: {
  docId: string;
  entry: HistoryEntry;
  showChanges: boolean;
  onShowChanges: (on: boolean) => void;
  canRestore: boolean;
  onBack: () => void;
  onRestored: () => void;
  /** Below `md`, where the drawer shows this pane instead of the timeline:
   *  focus Back on mount and for every other entry. */
  focusBack: boolean;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const notify = useUi((s) => s.notify);
  const kindShowsChanges = showsChanges(entry.kind);
  // A baseline or erase entry is asked for, and cached, once, as changes=0.
  const changes = showChanges && kindShowsChanges;

  const view = useQuery({
    queryKey: ["history", docId, "view", entry.id, entry.ended_at, changes] as const,
    queryFn: async ({ signal, queryKey }): Promise<ViewData> => ({
      view: unwrapResult(await historyApi.view(docId, entry.id, changes, signal)),
      endedAt: queryKey[4],
    }),
    staleTime: 60_000,
    // Superseded versions (a large one is several MiB) leave the cache soon.
    gcTime: 30_000,
    // A poll or a toggle keeps the same entry's last version on screen.
    placeholderData: (prev, prevQuery) => (prevQuery?.queryKey[3] === entry.id ? prev : undefined),
    retry: (failures, error) => (error as Failure).status === 503 && failures < BUSY_RETRIES,
    retryDelay: BUSY_RETRY_DELAY_MS,
  });
  const data = view.data;
  const placeholder = view.isPlaceholderData;
  const retryingBusy =
    view.isFetching && view.failureCount > 0 && (view.failureReason as Failure | null)?.status === 503;

  // The viewer is remounted per version the *data* belongs to.
  const viewerKey = data ? `${entry.id}:${data.endedAt}:${data.view.changes}` : null;
  const [badContentKey, setBadContentKey] = useState<string | null>(null);
  const onContentError = useCallback(() => setBadContentKey(viewerKey), [viewerKey]);
  const contentError = viewerKey !== null && badContentKey === viewerKey;

  const errorText = view.isError ? viewErrorText(view.error) : contentError ? copy.cantShow : null;
  const loading = view.isPending && !retryingBusy;

  // Judged on real data only: a placeholder never counts, and a baseline or
  // erase entry is never "unavailable" (its switch is simply disabled).
  const unavailable = kindShowsChanges && showChanges && !placeholder && !!data && !data.view.changes;
  const switchOn = showChanges && kindShowsChanges && !unavailable;

  const titleQuery = useQuery({ queryKey: ["doc", docId], queryFn: () => docsApi.get(docId) });
  const title = titleQuery.data && "ok" in titleQuery.data ? titleQuery.data.ok.title : "";

  // Wording builds date formatters: once per entry object (once per poll).
  const line = useMemo(() => entryLine(entry, new Date()), [entry]);

  const backRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (focusBack) backRef.current?.focus();
  }, [focusBack, entry.id]);

  const switchLabelId = useId();

  // The sidebar's own list and cache: the page's row says whether a copy
  // can go beside it. `docsApi.list` resolves an error rather than
  // rejecting, so a settled query alone is not enough.
  const docsList = useQuery({ queryKey: ["docs"], queryFn: () => docsApi.list() });
  const listed = docsList.data && "ok" in docsList.data ? docsList.data.ok.find((d) => d.id === docId) : undefined;
  const copyable = listed !== undefined && !listed.parent_hidden && !listed.is_template;

  const showRestore = canRestore && data !== undefined && data.view.can_restore;
  const showAsNew = showRestore && copyable;

  /** Says why either restore failed when the version's history answered
   *  it; false when the error is the restore's own. */
  function historyFailed(error: ApiError, entryId: number): boolean {
    const { status, code } = error;
    if (status === 409 && code === "history.moved") {
      // The entry moved on since this version loaded: show the new one.
      notify("warn", copy.restoreMoved);
      void qc.invalidateQueries({ queryKey: ["history", docId, "entries"] });
      void qc.invalidateQueries({ queryKey: ["history", docId, "view", entryId] });
    } else if (status === 409 && code === "history.rebuilding") {
      notify("error", copy.rebuilding);
    } else if (status === 404 && code === "history.not_found") {
      // A rebuild replaced the entry: refresh the list and the selection.
      notify("error", copy.notFound);
      void qc.invalidateQueries({ queryKey: ["history", docId] });
    } else if (status === 503 && code === "history.busy") {
      // Nothing was replayed or sent; the reader may try again.
      notify("error", copy.busy);
    } else if (status === 409 && code === "history.too_large") {
      notify("error", copy.restoreVersionTooLarge);
    } else {
      return false;
    }
    return true;
  }

  const restore = useMutation({
    // The result as it is: `unwrapResult` would drop `details`, where a
    // rate limit's wait lives.
    mutationFn: (v: { entryId: number; endedAt: string }) => historyApi.restoreEntry(docId, v.entryId, v.endedAt),
    onSuccess: (r, v) => {
      if (!("error" in r)) {
        const [kind, text] = RESTORED_TOASTS[r.ok.outcome];
        notify(kind, text);
        void qc.invalidateQueries({ queryKey: ["doc", docId] });
        // The restore is credited to the restorer; show them in the byline.
        void qc.invalidateQueries({ queryKey: ["contributors", docId] });
        void qc.invalidateQueries({ queryKey: ["history", docId] });
        onRestored();
        return;
      }
      if (historyFailed(r.error, v.entryId)) return;
      const { status, code } = r.error;
      if (status === 408 || status === 504) {
        restoreUnknown();
      } else if (status === 409 && code === "history.restore_busy") {
        notify("error", copy.restoreBusy);
      } else if (status === 409 && code === "restore.too_large") {
        // Only where the copy is offered does the toast point to it.
        notify("error", showAsNew ? copy.restoreTooLarge : copy.restoreTooLargeNoCopy);
      } else {
        // 500 `history.restore_failed` (nothing verified) among the rest.
        notifyFailure(notify, r.error, copy.restoreFailed);
      }
    },
    onError: (e) => {
      const { status, code } = e as Failure;
      if (status === undefined) restoreUnknown();
      else notifyFailure(notify, { status, code: code ?? "" }, copy.restoreFailed);
    },
  });

  /** No answer, or the router's or the room's timeout: the replace may
   *  still have applied. */
  function restoreUnknown() {
    notify("warn", copy.restoreUnknown);
    void qc.invalidateQueries({ queryKey: ["doc", docId] });
    void qc.invalidateQueries({ queryKey: ["history", docId] });
  }

  // Set on the click, cleared when the answer is handled: `isPending`
  // arrives a render too late to stop a double click making two pages.
  const copyStarted = useRef(false);
  const restoreAsNew = useMutation({
    mutationFn: (v: { entryId: number; label: string; endedAt: string }) =>
      historyApi.restoreAsNew(docId, v.entryId, { version_label: v.label, ended_at: v.endedAt }),
    onSuccess: (r, v) => {
      if (!("error" in r)) {
        notify("info", copy.restoredAsNew);
        void qc.invalidateQueries({ queryKey: ["docs"] });
        markDocEditMode(r.ok.id);
        onRestored();
        void navigate(`/doc/${r.ok.id}`);
        return;
      }
      if (historyFailed(r.error, v.entryId)) return;
      const { status, code } = r.error;
      if (limitErrorText(r.error) !== null) {
        notify("warn", duplicateErrorText(r.error), { key: LIMIT_TOAST_KEY });
      } else if (status === 413 || code === "doc.duplicate_busy") {
        notify("error", duplicateErrorText(r.error));
      } else if (status === 403) {
        notify("error", copy.restoreAsNewForbidden);
      } else if (status === 408 || status === 504) {
        copyUnknown();
      } else {
        // 409 `doc.duplicate_conflict` and `doc.sort_key_conflict` among
        // the rest, as the sidebar's Duplicate says them.
        notify("error", copy.restoreAsNewFailed);
      }
    },
    onError: () => copyUnknown(),
    onSettled: () => {
      copyStarted.current = false;
    },
  });

  /** No answer, or the router's timeout: the page may exist all the same. */
  function copyUnknown() {
    notify("warn", copy.restoreAsNewUnknown);
    void qc.invalidateQueries({ queryKey: ["docs"] });
  }

  const pending = restore.isPending || restoreAsNew.isPending;

  function onRestore() {
    if (!data || placeholder || pending || copyStarted.current) return;
    if (!window.confirm(copy.restoreConfirm)) return;
    // The version on screen, which is `entry.ended_at` whenever the button
    // is enabled.
    restore.mutate({ entryId: entry.id, endedAt: data.endedAt });
  }

  function onRestoreAsNew() {
    if (!data || placeholder || pending || copyStarted.current) return;
    copyStarted.current = true;
    restoreAsNew.mutate({ entryId: entry.id, label: formatDateTime(entry.ended_at), endedAt: data.endedAt });
  }


  return (
    <div
      data-testid="history-version"
      aria-busy={view.isPending || placeholder ? "true" : undefined}
      className="flex flex-1 min-h-0 flex-col"
    >
      <div
        data-testid="history-version-bar"
        className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2 border-b border-border"
      >
        <Button
          ref={backRef}
          type="button"
          size="sm"
          variant="ghost"
          data-testid="history-back"
          onClick={onBack}
          className="md:hidden"
        >
          <ArrowLeft size={14} aria-hidden />
          {copy.back}
        </Button>
        <p className="m-0 min-w-0 flex-1 text-[13px] leading-5 text-fg">{line}</p>
        <div className="flex items-center gap-2">
          <span id={switchLabelId} className="text-[13px] text-fg">
            {copy.showChanges}
          </span>
          <button
            type="button"
            role="switch"
            data-testid="history-show-changes"
            aria-checked={switchOn}
            aria-labelledby={switchLabelId}
            disabled={!kindShowsChanges || unavailable}
            onClick={() => onShowChanges(!showChanges)}
            className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full border border-border transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:opacity-50 disabled:cursor-not-allowed ${
              switchOn ? "bg-accent" : "bg-muted"
            }`}
          >
            <span
              aria-hidden
              className={`inline-block h-3.5 w-3.5 rounded-full bg-surface shadow transition-transform ${
                switchOn ? "translate-x-4" : "translate-x-0.5"
              }`}
            />
          </button>
        </div>
        {showRestore && (
          <Button
            type="button"
            size="sm"
            variant="primary"
            data-testid="history-restore"
            disabled={placeholder || pending}
            onClick={onRestore}
          >
            {restore.isPending ? copy.restoring : copy.restoreHere}
          </Button>
        )}
        {showAsNew && (
          <Button
            type="button"
            size="sm"
            variant="secondary"
            data-testid="history-restore-as-new"
            disabled={placeholder || pending}
            onClick={onRestoreAsNew}
          >
            {restoreAsNew.isPending ? copy.restoringAsNew : copy.restoreAsNew}
          </Button>
        )}
      </div>
      <div aria-live="polite" className="px-4 empty:hidden grid gap-1 py-2 text-[13px] text-fg-muted">
        {unavailable && (
          <p data-testid="history-changes-unavailable" className="m-0">
            {copy.changesUnavailable}
          </p>
        )}
        {loading && (
          <p data-testid="history-version-loading" className="m-0">
            {copy.loadingVersion}
          </p>
        )}
        {retryingBusy && (
          <p data-testid="history-version-busy" className="m-0">
            {copy.busyRetrying}
          </p>
        )}
        {errorText !== null && (
          <p data-testid="history-version-error" className="m-0">
            {errorText}
          </p>
        )}
      </div>
      <div data-testid="history-version-scroll" className="flex-1 min-h-0 overflow-auto px-6 pb-8 pt-4">
        {!titleQuery.isPending && (
          <h1 data-testid="history-version-title" className="m-0 font-body text-[30px] font-bold text-fg">
            {title === "" ? copy.untitled : title}
          </h1>
        )}
        {data !== undefined && !view.isError && (
          <div data-testid="history-version-doc" className="mt-4">
            <HistoryDocViewer key={viewerKey} doc={data.view.doc} onContentError={onContentError} />
          </div>
        )}
      </div>
    </div>
  );
}
