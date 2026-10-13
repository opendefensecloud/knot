import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Download, Eye, FileCode, History, LayoutTemplate, MessageSquare, Pencil, Share2 } from "lucide-react";
import { lazy, Suspense, useEffect, useRef, useState, type CSSProperties } from "react";
import { Link, Outlet, useNavigate, useParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { StatusDot, SyncStatus, type ConnStatus } from "../../components/StatusDot";
import { IconButton } from "../../components/ui/IconButton";
import { useUi } from "../../stores/ui";

import { CommentSidebar } from "../comments/CommentSidebar";
import { MarkdownView } from "../editor/MarkdownView";
import { purgeConfirmText } from "./archive";
import { ArchivedBanner } from "./ArchivedBanner";
import { Breadcrumb } from "./Breadcrumb";
import { DocByline } from "./DocByline";
import { docsApi } from "./docs.api";
import { editModeKey } from "./editMode";
import { HistoryDrawer } from "./HistoryDrawer";
import { DocWidthToggle } from "./DocWidthToggle";
import { ImportMarkdownButton } from "./ImportMarkdownButton";

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
        notify("error", "Couldn't rename");
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
  // ⌘E / Ctrl+E toggles edit mode for editor+. Viewers stay read-only.
  useEffect(() => {
    if (effRole !== "owner" && effRole !== "editor") return;
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "e") {
        e.preventDefault();
        setEditMode((v) => !v);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [effRole]);
  // ⌘⇧F / Ctrl+Shift+F toggles the layout width. Collision-free: the ⌘E
  // handler above bails on e.shiftKey, and no browser binds ⌘⇧F.
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

  const doc = useQuery({
    queryKey: ["doc", id],
    queryFn: () => docsApi.get(id!),
    enabled: Boolean(id),
  });
  const { refetch: refetchDoc } = doc;
  const docMeta = doc.data && "ok" in doc.data ? doc.data.ok : null;
  const archived = Boolean(docMeta?.archived_at);
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
      void nav("/", { replace: true });
    })();
    return () => {
      cancelled = true;
    };
  }, [status, notify, nav, refetchDoc]);

  if (!id) return null;
  if (doc.isLoading) {
    return <div className="doc-shell text-fg-muted">Loading…</div>;
  }
  if (!doc.data || "error" in doc.data) {
    return <div className="doc-shell text-fg-muted">Document not found.</div>;
  }

  const meta = doc.data.ok;
  // Archived docs are read-only for everyone until restored.
  const canEditDoc = !archived && (effRole === "owner" || effRole === "editor");
  const isDocOwner = !archived && effRole === "owner";

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
      notify("error", "Couldn't restore");
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
      notify("error", "Couldn't delete permanently");
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
      notify("error", next ? "Couldn't save as template" : "Couldn't unmark");
      return;
    }
    notify("info", next ? "Saved as template" : "Removed from templates");
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["templates"] });
    await doc.refetch();
  }

  return (
    <section
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
        <div className="mt-2 flex flex-wrap items-center gap-1">
          <SyncStatus sync={{ status, pendingBytes }} />
          {/* Keep the bare StatusDot mounted (invisible) so existing tests
              targeting `data-testid="status-dot"` still find it; SyncStatus
              is the user-facing affordance now. */}
          <span className="sr-only"><StatusDot status={status} /></span>
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
              data-testid="toggle-edit-mode"
              label={editMode ? "Stop editing (⌘E)" : "Edit (⌘E)"}
              active={editMode}
              onClick={() => setEditMode((v) => !v)}
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
          {/* The zip export reads live docs only; restore to export. */}
          {!archived && (
            <IconButton
              data-testid="doc-export"
              label="Export doc (shift-click for subtree)"
              onClick={(e) => {
                // Default: just this doc. Shift-click pulls the whole
                // subtree — a power-user shortcut without cluttering the
                // header with a separate button.
                const descendants = e.shiftKey;
                const a = document.createElement("a");
                a.href = `/api/docs/${id}/export?descendants=${descendants ? "true" : "false"}`;
                a.download = `${meta.title || "doc"}.zip`;
                a.click();
              }}
            >
              <Download size={16} aria-hidden />
            </IconButton>
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
              data-testid="open-comments"
              label="Comments"
              onClick={openCommentSidebar}
            >
              <MessageSquare size={16} aria-hidden />
            </IconButton>
          )}
        </div>
      </div>
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
            />
          </Suspense>
        )}
      </div>
      <Outlet />
      {historyOpen && id && !archived && (
        <HistoryDrawer docId={id} onClose={() => setHistoryOpen(false)} />
      )}
      {commentSidebarOpen && id && !archived && <CommentSidebar docId={id} />}
    </section>
  );
}
