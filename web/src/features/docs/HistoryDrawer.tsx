import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { historyApi, type SnapshotMeta } from "../../lib/history.api";
import { useUi } from "../../stores/ui";

export function HistoryDrawer({ docId, onClose }: { docId: string; onClose: () => void }) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null);

  const list = useQuery({
    queryKey: ["history", docId],
    queryFn: () => historyApi.list(docId),
  });

  const preview = useQuery({
    queryKey: ["history", docId, selectedSeq],
    queryFn: () => historyApi.preview(docId, selectedSeq!),
    enabled: selectedSeq != null,
  });

  const restore = useMutation({
    mutationFn: async (seq: number) => historyApi.restore(docId, seq),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Restore failed"); return; }
      notify("info", "Restored — your editor will refresh shortly.");
      await qc.invalidateQueries({ queryKey: ["doc", docId] });
      onClose();
    },
  });

  const snaps: SnapshotMeta[] =
    list.data && "ok" in list.data ? list.data.ok : [];
  const previewText: string =
    preview.data && "ok" in preview.data ? preview.data.ok : "";

  return (
    <div
      role="dialog"
      data-testid="history-drawer"
      style={{
        position: "fixed", top: 0, right: 0, bottom: 0,
        width: 720, maxWidth: "100vw",
        background: "white",
        borderLeft: "1px solid #e5e5e5",
        boxShadow: "-4px 0 12px rgba(0,0,0,0.1)",
        zIndex: 40,
        display: "flex", flexDirection: "column",
      }}
    >
      <header style={{ display: "flex", alignItems: "center", padding: 12, borderBottom: "1px solid #e5e5e5" }}>
        <h2 style={{ margin: 0, flex: 1 }}>History</h2>
        <button type="button" data-testid="history-close" onClick={onClose}>Close</button>
      </header>
      <div style={{ display: "flex", flex: 1, minHeight: 0 }}>
        <ul
          data-testid="history-list"
          style={{
            listStyle: "none", margin: 0, padding: 0,
            width: 260, borderRight: "1px solid #e5e5e5",
            overflow: "auto",
          }}
        >
          {list.isLoading && <li style={{ padding: 12, color: "#888" }}>Loading…</li>}
          {!list.isLoading && snaps.length === 0 && (
            <li style={{ padding: 12, color: "#888" }}>No snapshots yet.</li>
          )}
          {snaps.map((s) => (
            <li key={s.snapshot_seq}>
              <button
                type="button"
                data-testid={`history-snap-${s.snapshot_seq}`}
                onClick={() => setSelectedSeq(s.snapshot_seq)}
                style={{
                  display: "block", width: "100%", textAlign: "left",
                  padding: "8px 12px", border: "none",
                  background: selectedSeq === s.snapshot_seq ? "#e5e5ff" : "transparent",
                  cursor: "pointer",
                  borderBottom: "1px solid #f0f0f0",
                }}
              >
                <div style={{ fontWeight: 600 }}>{new Date(s.created_at).toLocaleString()}</div>
                <div style={{ color: "#888", fontSize: 12 }}>
                  seq {s.snapshot_seq} · {Math.max(1, Math.round(s.byte_size / 1024))} KB
                </div>
              </button>
            </li>
          ))}
        </ul>
        <div style={{ flex: 1, padding: 12, overflow: "auto", display: "flex", flexDirection: "column" }}>
          {selectedSeq == null ? (
            <p style={{ color: "#888" }}>Select a snapshot to preview.</p>
          ) : (
            <>
              <p style={{ color: "#666", fontSize: 13, marginTop: 0 }}>
                Restoring replaces the current content with this snapshot.
                Formatting outside the canonical schema (e.g. raw HTML) is not preserved.
              </p>
              <pre
                data-testid="history-preview"
                style={{
                  background: "#fafafa",
                  padding: 12,
                  borderRadius: 4,
                  overflow: "auto",
                  flex: 1,
                  fontFamily: "ui-monospace, monospace",
                  fontSize: 13,
                  whiteSpace: "pre-wrap",
                }}
              >{previewText}</pre>
              <button
                type="button"
                data-testid="history-restore"
                disabled={restore.isPending}
                onClick={() => {
                  if (!window.confirm("Replace the current content with this snapshot?")) return;
                  restore.mutate(selectedSeq);
                }}
                style={{ marginTop: 12, padding: 8 }}
              >
                {restore.isPending ? "Restoring…" : "Restore this snapshot"}
              </button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
