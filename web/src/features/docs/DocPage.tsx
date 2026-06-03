import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { lazy, Suspense, useEffect, useState } from "react";
import { Link, Outlet, useNavigate, useParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { StatusDot, type ConnStatus } from "../../components/StatusDot";
import { useUi } from "../../stores/ui";

import { docsApi } from "./docs.api";

const KnotEditor = lazy(() =>
  import("../editor/KnotEditor").then((m) => ({ default: m.KnotEditor })),
);

export default function DocPage() {
  const { id } = useParams<{ id: string }>();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { doc: effRole } = useEffectiveRole(id);
  const notify = useUi((s) => s.notify);
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [title, setTitle] = useState("");

  const doc = useQuery({
    queryKey: ["doc", id],
    queryFn: () => docsApi.get(id!),
    enabled: Boolean(id),
  });

  const rename = useMutation({
    mutationFn: async (title: string) => docsApi.patch(id!, { title }),
    onSuccess: async (r) => {
      if ("error" in r) {
        notify("error", "Couldn't rename");
        return;
      }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      await qc.invalidateQueries({ queryKey: ["doc", id] });
    },
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
  if (title === "" && meta.title) {
    setTitle(meta.title);
  }

  return (
    <section data-testid="doc-page" style={{ padding: 24 }}>
      <header style={{ display: "flex", alignItems: "center", marginBottom: 12 }}>
        <StatusDot status={status} />
        <input
          data-testid="doc-title"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          onBlur={() => { if (title !== meta.title) rename.mutate(title); }}
          style={{
            border: "none",
            fontSize: 24,
            fontWeight: 600,
            flex: 1,
            background: "transparent",
          }}
        />
        {effRole === "owner" && (
          <Link
            to="permissions"
            data-testid="open-permissions"
            style={{ marginLeft: 12 }}
          >
            Permissions
          </Link>
        )}
      </header>
      <Suspense fallback={<p>Loading editor…</p>}>
        <KnotEditor docId={id} onStatus={setStatus} role={meta.effective_role} />
      </Suspense>
      <Outlet />
    </section>
  );
}
