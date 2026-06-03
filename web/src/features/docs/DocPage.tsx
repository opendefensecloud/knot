import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { lazy, Suspense, useEffect, useState } from "react";
import { Link, Outlet, useNavigate, useParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { StatusDot, type ConnStatus } from "../../components/StatusDot";
import { useUi } from "../../stores/ui";

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
      style={{
        border: "none",
        fontSize: 24,
        fontWeight: 600,
        flex: 1,
        background: "transparent",
      }}
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
  if (doc.isLoading) return <div style={{ padding: 24 }}>Loading…</div>;
  if (!doc.data || "error" in doc.data) {
    return <div style={{ padding: 24 }}>Document not found.</div>;
  }

  const meta = doc.data.ok;

  return (
    <section data-testid="doc-page" style={{ padding: 24 }}>
      <header style={{ display: "flex", alignItems: "center", marginBottom: 12 }}>
        <StatusDot status={status} />
        <DocTitle key={id} id={id} initialTitle={meta.title} />
        {effRole === "owner" && (
          <Link
            to="permissions"
            data-testid="open-permissions"
            style={{ marginLeft: 12 }}
          >
            Permissions
          </Link>
        )}
        {(effRole === "owner" || effRole === "editor") && (
          <button
            type="button"
            data-testid="open-history"
            onClick={() => setHistoryOpen(true)}
            style={{
              marginLeft: 12,
              background: "none",
              border: "none",
              color: "#0050ff",
              cursor: "pointer",
              textDecoration: "underline",
            }}
          >
            History
          </button>
        )}
      </header>
      <Suspense fallback={<p>Loading editor…</p>}>
        <KnotEditor docId={id} onStatus={setStatus} role={meta.effective_role} />
      </Suspense>
      <Outlet />
      {historyOpen && id && (
        <HistoryDrawer docId={id} onClose={() => setHistoryOpen(false)} />
      )}
    </section>
  );
}
