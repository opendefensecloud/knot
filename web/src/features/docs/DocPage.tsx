import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Eye, FileCode, History, LayoutTemplate, Lock, MessageSquare, Pencil, Share2 } from "lucide-react";
import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { Link, Outlet, useNavigate, useParams, useSearchParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { StatusDot, SyncStatus, type ConnStatus } from "../../components/StatusDot";
import { IconButton } from "../../components/ui/IconButton";
import { useViewport } from "../../hooks/useViewport";
import type { ApiError } from "../../lib/api";
import type { Doc } from "../../lib/validators";
import { notifyFailure } from "../../lib/limitErrors";
import { useUi } from "../../stores/ui";

import { CommentSidebar } from "../comments/CommentSidebar";
import { MarkdownView } from "../editor/MarkdownView";
import type { KnotEditorHandle } from "../editor/readingCaret";
import { ExportMenu } from "../export/ExportMenu";
import { CopyLinkButton } from "./CopyLinkButton";
import { archiveErrorText, purgeConfirmText, purgeErrorText } from "./archive";
import { ArchivedBanner } from "./ArchivedBanner";
import { Breadcrumb } from "./Breadcrumb";
import { DocByline } from "./DocByline";
import { DocDockBar, DocDockControls } from "./DocDock";
import { docsApi } from "./docs.api";
import {
  editModeAriaKeyshortcuts,
  editModeKey,
  editModeShortcutLabel,
  isEditModeShortcut,
} from "./editMode";
import { HistoryDrawer } from "./HistoryDrawer";
import { DocWidthToggle } from "./DocWidthToggle";
import { ImportMarkdownButton } from "./ImportMarkdownButton";
import { captureReadingAnchor, findScroller, holdReadingPosition, type ReadingAnchor } from "./readingAnchor";
import { withoutSubtree } from "./tree";
import { useScrolledPast } from "./useScrolledPast";

// DocPage starts this download on mount (see the effect below) so the ~750 KB
// editor chunk overlaps the doc-metadata request instead of waiting for it.
// `import()` is cached: the preload and `lazy` share one fetch.
const loadKnotEditor = () => import("../editor/KnotEditor");
const KnotEditor = lazy(() =>
  loadKnotEditor().then((m) => ({ default: m.KnotEditor })),
);

export function DocTitle({
  id,
  initialTitle,
  editable,
}: {
  id: string;
  initialTitle: string;
  editable: boolean;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [title, setTitle] = useState(initialTitle);

  const rename = useMutation({
    mutationFn: async (next: string) => docsApi.patch(id, { title: next }),
    onSuccess: async (r) => {
      if ("error" in r) {
        notifyFailure(notify, r.error, "Couldn't rename");
        return;
      }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      await qc.invalidateQueries({ queryKey: ["doc", id] });
    },
  });

  return (
    <input
      data-testid="doc-title"
      value={title}
      readOnly={!editable}
      onChange={(e) => setTitle(e.target.value)}
      onBlur={() => { if (editable && title !== initialTitle) rename.mutate(title); }}
      placeholder="Untitled"
      className={`w-full border-none bg-transparent font-body text-[30px] font-bold text-fg placeholder:text-fg-muted/60 focus:outline-none focus:ring-0 px-0 ${
        editable ? "" : "cursor-default"
      }`}
    />
  );
}

export default function DocPage() {
  const { id } = useParams<{ id: string }>();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { doc: effRole } = useEffectiveRole(id);
  const notify = useUi((s) => s.notify);
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [pendingBytes, setPendingBytes] = useState(0);
  useEffect(() => {
    // A failed preload is not reported here: the `lazy` import meets the same
    // failure when the editor renders, and surfaces it there.
    loadKnotEditor().catch(() => {});
  }, []);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [mdView, setMdView] = useState(false);
  // View mode by default — viewers are always read-only anyway; editors and
  // owners get a per-tab toggle (persisted in window.sessionStorage by doc id) so a
  // page refresh keeps the chosen mode but a fresh tab starts safe.
  const editModeKeyOrNull = id ? editModeKey(id) : null;
  const [editMode, setEditMode] = useState<boolean>(() => {
    // localStorage override for automation: any non-empty value defaults
    // every doc to edit mode. Used by the e2e suite to avoid threading a
    // `toggle-edit-mode` click through every spec; harmless in production
    // because nothing in the app writes this key.
    try {
      if (window.localStorage.getItem("knot.editMode.defaultOn") === "1") return true;
    } catch { /* localStorage unavailable */ }
    if (!editModeKeyOrNull) return false;
    return window.sessionStorage.getItem(editModeKeyOrNull) === "1";
  });
  useEffect(() => {
    if (!editModeKeyOrNull) return;
    window.sessionStorage.setItem(editModeKeyOrNull, editMode ? "1" : "0");
  }, [editMode, editModeKeyOrNull]);
  // ⌘⇧F / Ctrl+Shift+F toggles the layout width. Collision-free: the edit
  // toggle's handler below (⌘E / Ctrl+E) bails on e.shiftKey, and no browser
  // binds ⌘⇧F.
  const toggleDocWidth = useUi((s) => s.toggleDocWidth);
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "f") {
        e.preventDefault();
        toggleDocWidth();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [toggleDocWidth]);
  const commentSidebarOpen = useUi((s) => s.commentSidebarOpen);
  const openCommentSidebar = useUi((s) => s.openCommentSidebar);
  const closeCommentSidebar = useUi((s) => s.closeCommentSidebar);
  const viewport = useViewport();

  const openThreadLink = useUi((s) => s.openThreadLink);
  const openSuggestionLink = useUi((s) => s.openSuggestionLink);
  const clearSuggestionFocus = useUi((s) => s.clearSuggestionFocus);
  // The header and dock buttons reopen the sidebar on the tab it last showed.
  const sidebarTab = useUi((s) => s.sidebarTab);
  const reopenCommentSidebar = useCallback(() => openCommentSidebar(sidebarTab), [openCommentSidebar, sidebarTab]);


  const doc = useQuery({
    queryKey: ["doc", id],
    queryFn: () => docsApi.get(id!),
    enabled: Boolean(id),
  });
  const { refetch: refetchDoc } = doc;
  const docMeta = doc.data && "ok" in doc.data ? doc.data.ok : null;
  const archived = Boolean(docMeta?.archived_at);

  // Arriving from a notification: /doc/:id?thread=<uuid>. The sidebar's
  // existing active-thread machinery (CommentThread.tsx) does the scrolling
  // and the focus ring — this just opens the sidebar and points it at the
  // thread, once the doc is known (an archived page has no comments
  // sidebar). The param is then dropped, so following the same link again
  // opens the thread again, and a reload doesn't.
  const [searchParams, setSearchParams] = useSearchParams();
  const threadParam = searchParams.get("thread");
  useEffect(() => {
    if (!threadParam || !docMeta) return;
    if (!archived) openThreadLink(threadParam);
    setSearchParams(
      (params) => {
        params.delete("thread");
        return params;
      },
      { replace: true },
    );
  }, [threadParam, docMeta, archived, openThreadLink, setSearchParams]);
  // The focused suggestion card and the last `?suggestion=` link name this
  // page's suggestions: leaving the page drops them, or coming back would
  // scroll to an old highlight and open Resolved again. Here rather than in
  // KnotEditor, and before the effect below: under StrictMode a component's
  // effects are run, cleaned up and run again when it mounts, and only here
  // does the link effect run again after this cleanup. An editor that mounts
  // after the link was opened (it loads lazily) would drop it for good.
  useEffect(() => () => { clearSuggestionFocus(); }, [id, clearSuggestionFocus]);
  // The same for a suggestion: /doc/:id?suggestion=<uuid> opens the
  // Suggestions tab on that card (SuggestionsPanel shows resolved ones when
  // it is not pending, and scrolls the page to its highlight).
  const suggestionParam = searchParams.get("suggestion");
  useEffect(() => {
    if (!suggestionParam || !docMeta) return;
    if (!archived) openSuggestionLink(suggestionParam);
    setSearchParams(
      (params) => {
        params.delete("suggestion");
        return params;
      },
      { replace: true },
    );
  }, [suggestionParam, docMeta, archived, openSuggestionLink, setSearchParams]);
  const isArchiveRoot = archived && docMeta?.archive_root_id === docMeta?.id;
  // Only for the subpage count in the "Delete permanently" prompt.
  const archiveList = useQuery({
    queryKey: ["archive"],
    queryFn: () => docsApi.listArchive(),
    enabled: isArchiveRoot && docMeta?.effective_role === "owner",
  });
  const [archiveBusy, setArchiveBusy] = useState(false);
  // Bumped to remount the editor, which reconnects its socket.
  const [editorEpoch, setEditorEpoch] = useState(0);
  // The archive state the editor is currently mounted for.
  const mountedArchived = useRef(archived);
  useEffect(() => {
    mountedArchived.current = archived;
  }, [archived]);
  // Set while this page purges its own doc, which also revokes its socket.
  const leaving = useRef(false);

  // ── Doc dock ─────────────────────────────────────────────────────────────
  // Once the action row scrolls out of <main>, Edit and Comments stay
  // reachable: in a slim sticky dock in view mode, at the end of the
  // formatting toolbar while editing. Toggling from there keeps the text the
  // reader is looking at in place and puts the caret where they were reading.
  const sectionRef = useRef<HTMLElement>(null);
  // State, not a ref: the row mounts only after the loading branch below.
  const [actionRowEl, setActionRowEl] = useState<HTMLDivElement | null>(null);
  const actionEditToggleRef = useRef<HTMLButtonElement>(null);
  const actionCommentsRef = useRef<HTMLButtonElement>(null);
  const dockEditToggleRef = useRef<HTMLButtonElement>(null);
  const dockCommentsRef = useRef<HTMLButtonElement>(null);
  const editorHandleRef = useRef<KnotEditorHandle | null>(null);
  const editModeRef = useRef(editMode);
  const condensedRef = useRef(false);
  const pendingToggleRef = useRef<{
    entering: boolean;
    anchor: ReadingAnchor | null;
    mayMoveFocus: boolean;
    refocusToggle: boolean;
  } | null>(null);
  const rescueFocusRef = useRef<"edit" | "comments" | null>(null);
  const [announcement, setAnnouncement] = useState("");

  const condensed = useScrolledPast(actionRowEl, (next) => {
    if (next) return;
    // The dock (or the toolbar slot) is about to go: remember whether it held
    // focus, while the focused button is still in the DOM.
    const active = document.activeElement as HTMLElement | null;
    const control = active?.closest<HTMLElement>("[data-doc-controls] [data-dock-control]");
    rescueFocusRef.current = (control?.dataset.dockControl as "edit" | "comments" | undefined) ?? null;
  });
  useLayoutEffect(() => {
    editModeRef.current = editMode;
    condensedRef.current = condensed;
  }, [editMode, condensed]);
  useLayoutEffect(() => {
    if (condensed) return;
    const which = rescueFocusRef.current;
    rescueFocusRef.current = null;
    if (which === "edit") actionEditToggleRef.current?.focus({ preventScroll: true });
    else if (which === "comments") actionCommentsRef.current?.focus({ preventScroll: true });
  }, [condensed]);

  // Everything is decided here, before the state changes: once the toolbar
  // has mounted or gone, the old layout and the old focus are lost.
  const toggleEditMode = useCallback(() => {
    const section = sectionRef.current;
    const scroller = section ? findScroller(section) : null;
    const prose = section?.querySelector("[data-testid='editor-host'] .ProseMirror");
    const active = document.activeElement as HTMLElement | null;
    const inProse = !!active?.closest(".ProseMirror");
    const inToolbar = !!active?.closest("[data-testid='editor-toolbar']");
    const unfocused = active === document.body || active === null;
    // Focus follows the toggle into the text unless it is somewhere the
    // reader is busy: typing outside the text (the title input, the comment
    // composer, a search field) or inside a dialog or menu (the comment rail,
    // the history drawer, a popover). A deny-list rather than an allow-list:
    // most pages are opened from a link in the tree, the breadcrumb or a
    // backlink, which keeps focus, and from there the shortcut must still
    // put the caret in the text (#86).
    const typingElsewhere =
      !inProse && !!active?.closest("input, textarea, select, [contenteditable]:not([contenteditable='false'])");
    const inOverlay = !!active?.closest("[role='dialog'], [role='menu'], [role='listbox'], [aria-modal='true']");
    const mayMoveFocus = !typingElsewhere && !inOverlay;
    const entering = !editModeRef.current;
    pendingToggleRef.current = {
      entering,
      // Only mid-document: at the top the header's toolbar slides in visibly
      // and there is no reading position to hold.
      anchor: condensedRef.current && section && scroller && prose
        ? captureReadingAnchor(scroller, prose, section)
        : null,
      mayMoveFocus,
      refocusToggle: !entering && (inProse || inToolbar || unfocused),
    };
    setEditMode(entering);
    setAnnouncement(entering ? "Editing" : "Viewing");
  }, []);
  const toggleEditModeRef = useRef(toggleEditMode);
  useEffect(() => {
    toggleEditModeRef.current = toggleEditMode;
  }, [toggleEditMode]);

  // Runs after the editor's own layout effects: setEditable has happened and
  // the toolbar is in (or out of) the DOM.
  useLayoutEffect(() => {
    const pending = pendingToggleRef.current;
    pendingToggleRef.current = null;
    if (!pending) return;
    const section = sectionRef.current;
    const scroller = section ? findScroller(section) : null;
    const cancel = pending.anchor && section && scroller
      ? holdReadingPosition(scroller, pending.anchor, { durationMs: 0, scope: section })
      : undefined;
    let revealFrame = 0;
    if (pending.entering && pending.mayMoveFocus) {
      // Entering edit mode puts the caret in the text, at the top of the page
      // as mid-document (#86). Without it, focus stayed on <body> or the
      // Edit button: the editor showed no caret, typing went nowhere, and
      // End/Home scrolled <main> instead of moving within a line. The caret
      // keeps a click made before the toggle, or lands on the first visible
      // line; the editor is focused without scrolling. On touch this opens
      // the on-screen keyboard, as the dock's Edit already does mid-document:
      // the user asked to edit.
      editorHandleRef.current?.placeCaretAtReadingLine();
      // Only mid-document is there a sticky band over the text to clear:
      // after the toolbar's own re-render for the new caret (a table adds a
      // row of buttons), make sure the caret's line is not under it. Gated
      // on the page being condensed, not on the anchor: when no reading
      // anchor could be captured the band is there all the same.
      if (pending.anchor || condensedRef.current) {
        revealFrame = window.requestAnimationFrame(() => {
          editorHandleRef.current?.revealCaretBelowBand();
        });
      }
    }
    if (!pending.entering && pending.refocusToggle) {
      // Mid-document, only the dock's toggle is on screen; without one (the
      // Markdown view has none) focus stays put rather than jumping to the
      // header off screen.
      if (condensedRef.current) dockEditToggleRef.current?.focus({ preventScroll: true });
      else actionEditToggleRef.current?.focus({ preventScroll: true });
    }
    return () => {
      cancel?.();
      window.cancelAnimationFrame(revealFrame);
    };
  }, [editMode]);

  // Opening or closing the comment rail can inset the page and re-wrap the
  // text over the .doc-shell transition: hold the reading block meanwhile.
  const railFirstRun = useRef(true);
  useLayoutEffect(() => {
    if (railFirstRun.current) {
      railFirstRun.current = false;
      return;
    }
    const section = sectionRef.current;
    const scroller = section ? findScroller(section) : null;
    const prose = section?.querySelector("[data-testid='editor-host'] .ProseMirror");
    if (!condensedRef.current || !section || !scroller || !prose) return;
    const anchor = captureReadingAnchor(scroller, prose, section);
    return anchor ? holdReadingPosition(scroller, anchor, { durationMs: 250 }) : undefined;
  }, [commentSidebarOpen]);

  // Mod+E toggles edit mode for editor+ on a live doc: ⌘E on a Mac, Ctrl+E
  // elsewhere (see isEditModeShortcut — a Mac's Ctrl+E is end-of-line and is
  // left to the editor). Viewers stay read-only. On an archived doc the
  // chord is still claimed, as it always was, so it never falls through to
  // the browser's own binding (Ctrl+E focuses the search bar) — it just does
  // nothing; held-down repeats and IME composition likewise.
  const claimsEditShortcut = effRole === "owner" || effRole === "editor";
  const canToggleEdit = !archived && claimsEditShortcut;
  useEffect(() => {
    if (!claimsEditShortcut) return;
    function onKey(e: KeyboardEvent) {
      if (!isEditModeShortcut(e)) return;
      e.preventDefault();
      if (!canToggleEdit || e.repeat || e.isComposing) return;
      toggleEditModeRef.current();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [claimsEditShortcut, canToggleEdit]);

  useEffect(() => {
    if (status !== "unauthorised" || leaving.current) return;
    // The server closes every socket on a doc (4403) whenever its access
    // changes: archive, restore, a grant. Look before leaving: if the doc is
    // still readable, stay and reconnect with whatever the server grants
    // now (read-only once archived). Only a doc we can no longer read sends
    // the user away.
    let cancelled = false;
    void (async () => {
      const fresh = await refetchDoc();
      if (cancelled || leaving.current) return;
      if (fresh.data && "ok" in fresh.data) {
        // A change of archive state remounts the editor through its key;
        // otherwise remount it here.
        if (Boolean(fresh.data.ok.archived_at) === mountedArchived.current) {
          setEditorEpoch((n) => n + 1);
        }
        return;
      }
      notify("error", "You no longer have access to this document.");
      // The sidebar may still list the page (a restrict hides it), and the
      // page's metadata must not linger in the cache. The landing page
      // opens the list's first page, so the page and its subpages leave the
      // cached list before it mounts, and the list is fetched again.
      qc.setQueryData<{ ok: Doc[] } | { error: ApiError }>(["docs"], (cached) =>
        cached && "ok" in cached ? { ok: withoutSubtree(cached.ok, id!) } : cached,
      );
      void qc.invalidateQueries({ queryKey: ["docs"] });
      qc.removeQueries({ queryKey: ["doc", id] });
      void nav("/", { replace: true });
    })();
    return () => {
      cancelled = true;
    };
  }, [status, notify, nav, refetchDoc, qc, id]);

  if (!id) return null;
  if (doc.isLoading) {
    return <div className="doc-shell text-fg-muted">Loading…</div>;
  }
  if (!doc.data || "error" in doc.data) {
    // The same for a missing page and one the user can't read: the server
    // answers both alike, and so does this.
    return (
      <div className="doc-shell text-fg-muted">
        This page doesn&apos;t exist or you don&apos;t have access to it.
      </div>
    );
  }

  const meta = doc.data.ok;
  // Archived docs are read-only for everyone until restored.
  const canEditDoc = !archived && (effRole === "owner" || effRole === "editor");
  const isDocOwner = !archived && effRole === "owner";
  const showEditInDock = canEditDoc && !mdView;
  const showCommentsInDock = !archived;
  const dockInToolbar = showEditInDock && editMode;
  const dockControls = (
    <DocDockControls
      showEdit={showEditInDock}
      editMode={editMode}
      onToggleEdit={toggleEditMode}
      showComments={showCommentsInDock}
      commentsOpen={commentSidebarOpen}
      onToggleComments={commentSidebarOpen ? closeCommentSidebar : reopenCommentSidebar}
      editToggleRef={dockEditToggleRef}
      commentsRef={dockCommentsRef}
      size={viewport === "mobile" ? "md" : "sm"}
    />
  );

  async function afterArchiveChange() {
    await qc.invalidateQueries({ queryKey: ["doc"] });
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["archive"] });
  }

  async function restoreFromArchive() {
    setArchiveBusy(true);
    const r = await docsApi.restore(id!);
    setArchiveBusy(false);
    if ("error" in r) {
      notifyFailure(
        notify,
        r.error,
        r.error.code === "doc.restore_restricted_parent_archived"
          ? archiveErrorText(r.error.code)
          : "Couldn't restore",
      );
      return;
    }
    notify("info", "Restored");
    await afterArchiveChange();
  }

  async function purgeFromArchive() {
    const entries = archiveList.data && "ok" in archiveList.data ? archiveList.data.ok : [];
    const subpages = entries.find((e) => e.id === id)?.subpage_count ?? 0;
    if (!window.confirm(purgeConfirmText(meta.title, subpages))) return;
    setArchiveBusy(true);
    leaving.current = true;
    const r = await docsApi.purge(id!);
    setArchiveBusy(false);
    if ("error" in r) {
      leaving.current = false;
      notifyFailure(notify, r.error, purgeErrorText(r.error.code, "Couldn't delete permanently"));
      return;
    }
    notify("info", "Deleted permanently");
    await qc.invalidateQueries({ queryKey: ["archive"] });
    await nav("/archive", { replace: true });
  }

  // Named rather than an inline `async` handler: `onClick` is typed to
  // return void, so handing it a promise leaves rejections unhandled.
  // Callers below fire it with `void`, the same idiom the editor uses for
  // its own async work.
  async function toggleTemplate() {
    const next = !meta.is_template;
    const r = await docsApi.setTemplate(id!, next);
    if ("error" in r) {
      notifyFailure(notify, r.error, next ? "Couldn't save as template" : "Couldn't unmark");
      return;
    }
    notify("info", next ? "Saved as template" : "Removed from templates");
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["templates"] });
    await doc.refetch();
  }

  return (
    <section
      ref={sectionRef}
      data-testid="doc-page"
      className="doc-shell"
      // Drives the comment-rail inset in layout.css (only where the content
      // column is 1020px or wider), so the rail reserves space instead of
      // covering live text.
      style={{ "--knot-rail-w": commentSidebarOpen ? "400px" : "0px" } as CSSProperties}
    >
      <div className="measure">
        <Breadcrumb items={[{ title: "Documents" }, { title: meta.title }]} />
      </div>
      {/* The title gets the whole column and the action row sits beneath it
          (issue #29). Sharing one line left the title only what the ~490px
          action row did not take — 208px of the 712px measure, about fifteen
          characters at 30px bold — and an <input> never wraps, so anything
          longer was cut off behind the tool icons. The action row still
          wraps internally so that on a phone, where its intrinsic 412px
          exceeds the 327px content box, no control lands off-screen. */}
      <div className="measure mt-3">
        <DocTitle key={id} id={id} initialTitle={meta.title}
                  editable={canEditDoc && editMode} />
        {/* Its own key: sibling keys must be unique, and sharing DocTitle's
            `id` left the previous doc's title input mounted on navigation. */}
        <DocByline key={`byline:${id}`} docId={id} />
        <div ref={setActionRowEl} data-testid="doc-actions" className="mt-2 flex flex-wrap items-center gap-1">
          <SyncStatus sync={{ status, pendingBytes }} />
          {/* Keep the bare StatusDot mounted (invisible) so existing tests
              targeting `data-testid="status-dot"` still find it; SyncStatus
              is the user-facing affordance now. */}
          <span className="sr-only"><StatusDot status={status} /></span>
          {meta.within_restricted && <RestrictedChip linksToPermissions={isDocOwner} />}
          <CopyLinkButton docId={id} archived={archived} />
          {isDocOwner && (
            <Link
              to="permissions"
              data-testid="open-permissions"
              aria-label="Share"
              title="Share & permissions"
              className="inline-flex items-center gap-1.5 h-9 px-3 rounded text-[13px] font-medium text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              <Share2 size={16} aria-hidden />
              <span>Share</span>
            </Link>
          )}
          <DocWidthToggle />
          {canEditDoc && (
            <IconButton
              ref={actionEditToggleRef}
              data-testid="toggle-edit-mode"
              label={editMode ? `Stop editing (${editModeShortcutLabel()})` : `Edit (${editModeShortcutLabel()})`}
              active={editMode}
              aria-keyshortcuts={editModeAriaKeyshortcuts()}
              onClick={toggleEditMode}
            >
              {editMode ? <Eye size={16} aria-hidden /> : <Pencil size={16} aria-hidden />}
            </IconButton>
          )}
          {canEditDoc && (
            <IconButton
              data-testid="open-history"
              label="History"
              onClick={() => setHistoryOpen(true)}
            >
              <History size={16} aria-hidden />
            </IconButton>
          )}
          <IconButton
            data-testid="toggle-markdown"
            label={mdView ? "Show editor" : "Show markdown"}
            active={mdView}
            onClick={() => setMdView((v) => !v)}
          >
            <FileCode size={16} aria-hidden />
          </IconButton>
          {canEditDoc && (
            <ImportMarkdownButton docId={id} docTitle={meta.title} />
          )}
          {/* Export reads live docs only; restore to export. */}
          {!archived && (
            <ExportMenu
              docId={id}
              title={meta.title}
              isTemplate={meta.is_template}
              status={status}
              pendingBytes={pendingBytes}
            />
          )}
          {isDocOwner && (
            <IconButton
              data-testid="toggle-template"
              label={meta.is_template ? "Remove from templates" : "Save as template"}
              active={meta.is_template}
              onClick={() => { void toggleTemplate(); }}
            >
              <LayoutTemplate size={16} aria-hidden />
            </IconButton>
          )}
          {!archived && (
            <IconButton
              ref={actionCommentsRef}
              data-testid="open-comments"
              label="Comments"
              onClick={reopenCommentSidebar}
            >
              <MessageSquare size={16} aria-hidden />
            </IconButton>
          )}
        </div>
      </div>
      {/* A direct child of the section, so it stays stuck for the whole doc. */}
      {condensed && !dockInToolbar && (showEditInDock || showCommentsInDock) && (
        <DocDockBar title={meta.title}>{dockControls}</DocDockBar>
      )}
      {archived && (
        <ArchivedBanner
          doc={meta}
          canPurge={effRole === "owner"}
          busy={archiveBusy}
          onRestore={() => void restoreFromArchive()}
          onPurge={() => void purgeFromArchive()}
        />
      )}
      <div className="mt-6">
        {mdView ? (
          <MarkdownView docId={id} />
        ) : (
          <Suspense fallback={<p className="measure text-fg-muted">Loading editor…</p>}>
            <KnotEditor
              // Remount on archive/restore so the socket reconnects with
              // the access the server now grants (read-only when archived).
              key={`${archived ? "archived" : "live"}:${editorEpoch}`}
              docId={id}
              onStatus={setStatus}
              onPendingBytes={setPendingBytes}
              role={archived ? "viewer" : meta.effective_role}
              editMode={!archived && editMode}
              toolbarTrailing={dockInToolbar ? dockControls : undefined}
              toolbarTrailingVisible={condensed}
              handleRef={editorHandleRef}
            />
          </Suspense>
        )}
      </div>
      <Outlet />
      {historyOpen && id && !archived && (
        <HistoryDrawer docId={id} onClose={() => setHistoryOpen(false)} />
      )}
      {commentSidebarOpen && id && !archived && <CommentSidebar docId={id} />}
      <span role="status" aria-live="polite" className="sr-only" data-testid="edit-mode-announcer">
        {announcement}
      </span>
    </section>
  );
}

const CHIP_CLASS =
  "inline-flex items-center gap-1 h-6 px-2 rounded-full bg-muted text-[12px] font-medium text-fg-muted";

/** Marks a page in a restricted subtree. Owners can open the permissions
 *  from it. */
function RestrictedChip({ linksToPermissions }: { linksToPermissions: boolean }) {
  const content = (
    <>
      <Lock size={12} aria-hidden />
      <span>Restricted</span>
    </>
  );
  if (linksToPermissions) {
    return (
      <Link
        to="permissions"
        data-testid="restricted-chip"
        title="Only people with access can see this page"
        className={`${CHIP_CLASS} no-underline hover:text-fg transition-colors`}
      >
        {content}
      </Link>
    );
  }
  return (
    <span data-testid="restricted-chip" title="Only people with access can see this page" className={CHIP_CLASS}>
      {content}
    </span>
  );
}
