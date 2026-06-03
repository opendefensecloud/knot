import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FileCode, History, MessageSquare, Share2 } from "lucide-react";
import { lazy, Suspense, useEffect, useState } from "react";
import { Link, Outlet, useNavigate, useParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { StatusDot, type ConnStatus } from "../../components/StatusDot";
import { IconButton } from "../../components/ui/IconButton";
import { useUi } from "../../stores/ui";

import { CommentSidebar } from "../comments/CommentSidebar";
import { MarkdownView } from "../editor/MarkdownView";
import { Breadcrumb } from "./Breadcrumb";
import { docsApi } from "./docs.api";
import { HistoryDrawer } from "./HistoryDrawer";

const KnotEditor = lazy(() =>
  import("../editor/KnotEditor").then((m) => ({ default: m.KnotEditor })),
);

function DocTitle({ id, initialTitle }: { id: string; initialTitle: string }) {
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
      onChange={(e) => setTitle(e.target.value)}
      onBlur={() => { if (title !== initialTitle) rename.mutate(title); }}
      placeholder="Untitled"
      className="w-full border-none bg-transparent text-[30px] font-bold text-fg placeholder:text-fg-muted/60 focus:outline-none focus:ring-0 px-0"
    />
  );
}

export default function DocPage() {
  const { id } = useParams<{ id: string }>();
  const nav = useNavigate();
  const { doc: effRole } = useEffectiveRole(id);
  const notify = useUi((s) => s.notify);
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [historyOpen, setHistoryOpen] = useState(false);
  const [mdView, setMdView] = useState(false);
  const commentSidebarOpen = useUi((s) => s.commentSidebarOpen);
  const openCommentSidebar = useUi((s) => s.openCommentSidebar);

  const doc = useQuery({
    queryKey: ["doc", id],
    queryFn: () => docsApi.get(id!),
    enabled: Boolean(id),
  });

  useEffect(() => {
    if (status === "unauthorised") {
      notify("error", "You no longer have access to this document.");
      void nav("/", { replace: true });
    }
  }, [status, notify, nav]);

  if (!id) return null;
  if (doc.isLoading) {
    return <div className="mx-auto max-w-[760px] px-6 py-8 text-fg-muted">Loading…</div>;
  }
  if (!doc.data || "error" in doc.data) {
    return <div className="mx-auto max-w-[760px] px-6 py-8 text-fg-muted">Document not found.</div>;
  }

  const meta = doc.data.ok;

  return (
    <section data-testid="doc-page" className="mx-auto max-w-[760px] px-6 py-8">
      <Breadcrumb items={[{ title: "Documents" }, { title: meta.title }]} />
      <div className="mt-3 flex items-start gap-3">
        <div className="flex-1 min-w-0">
          <DocTitle key={id} id={id} initialTitle={meta.title} />
        </div>
        <div className="flex items-center gap-1 pt-2 shrink-0">
          <StatusDot status={status} />
          {effRole === "owner" && (
            <Link
              to="permissions"
              data-testid="open-permissions"
              aria-label="Permissions"
              title="Permissions"
              className="inline-flex items-center justify-center h-9 w-9 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              <Share2 size={16} aria-hidden />
            </Link>
          )}
          {(effRole === "owner" || effRole === "editor") && (
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
          <IconButton
            data-testid="open-comments"
            label="Comments"
            onClick={openCommentSidebar}
          >
            <MessageSquare size={16} aria-hidden />
          </IconButton>
        </div>
      </div>
      <div className="mt-6">
        {mdView ? (
          <MarkdownView docId={id} />
        ) : (
          <Suspense fallback={<p className="text-fg-muted">Loading editor…</p>}>
            <KnotEditor docId={id} onStatus={setStatus} role={meta.effective_role} />
          </Suspense>
        )}
      </div>
      <Outlet />
      {historyOpen && id && (
        <HistoryDrawer docId={id} onClose={() => setHistoryOpen(false)} />
      )}
      {commentSidebarOpen && id && <CommentSidebar docId={id} />}
    </section>
  );
}
