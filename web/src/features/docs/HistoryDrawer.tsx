import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useState } from "react";

import { IconButton } from "../../components/ui/IconButton";
import { Button } from "../../components/ui/Button";
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
      className="fixed right-0 top-0 h-dvh w-[720px] max-w-full z-40 bg-surface border-l border-border shadow-xl flex flex-col"
    >
      <header className="flex items-center gap-2 px-4 py-3 border-b border-border">
        <h2 className="m-0 flex-1 text-base font-semibold text-fg">History</h2>
        <IconButton
          data-testid="history-close"
          label="Close"
          size="sm"
          onClick={onClose}
        >
          <X size={14} aria-hidden />
        </IconButton>
      </header>
      <div className="flex flex-1 min-h-0">
        <ul
          data-testid="history-list"
          className="list-none m-0 p-0 w-[260px] border-r border-border overflow-auto"
        >
          {list.isLoading && <li className="px-3 py-3 text-fg-muted text-sm">Loading…</li>}
          {!list.isLoading && snaps.length === 0 && (
            <li className="px-3 py-3 text-fg-muted text-sm">No snapshots yet.</li>
          )}
          {snaps.map((s) => (
            <li key={s.snapshot_seq}>
              <button
                type="button"
                data-testid={`history-snap-${s.snapshot_seq}`}
                onClick={() => setSelectedSeq(s.snapshot_seq)}
                className={`block w-full text-left px-3 py-2 border-b border-border transition-colors ${
                  selectedSeq === s.snapshot_seq ? "bg-muted text-fg" : "text-fg hover:bg-muted/60"
                }`}
              >
                <div className="font-semibold text-[13px]">{new Date(s.created_at).toLocaleString()}</div>
                <div className="text-fg-muted text-[12px] mt-0.5">
                  seq {s.snapshot_seq} · {Math.max(1, Math.round(s.byte_size / 1024))} KB
                </div>
              </button>
            </li>
          ))}
        </ul>
        <div className="flex-1 p-4 overflow-auto flex flex-col">
          {selectedSeq == null ? (
            <p className="text-fg-muted text-sm">Select a snapshot to preview.</p>
          ) : (
            <>
              <p className="text-fg-muted text-[13px] mt-0 mb-3">
                Restoring replaces the current content with this snapshot.
                Formatting outside the canonical schema (e.g. raw HTML) is not preserved.
              </p>
              <pre
                data-testid="history-preview"
                className="bg-muted text-fg px-3 py-3 rounded overflow-auto flex-1 font-mono text-[13px] whitespace-pre-wrap"
              >{previewText}</pre>
              <div className="mt-3">
                <Button
                  type="button"
                  variant="primary"
                  data-testid="history-restore"
                  disabled={restore.isPending}
                  onClick={() => {
                    if (!window.confirm("Replace the current content with this snapshot?")) return;
                    restore.mutate(selectedSeq);
                  }}
                >
                  {restore.isPending ? "Restoring…" : "Restore this snapshot"}
                </Button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
