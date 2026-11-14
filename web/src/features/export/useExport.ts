import { useCallback, useEffect, useRef, useState } from "react";

import type { ConnStatus } from "../../components/StatusDot";
import type { ApiError } from "../../lib/api";
import { LIMIT_TOAST_KEY, limitErrorText, type Notify } from "../../lib/limitErrors";
import { useUi } from "../../stores/ui";
import { AUDIT_UNAVAILABLE_TEXT, isAuditUnavailable } from "./auditRefusal";
import { exportApi } from "./export.api";
import { exportFileName, fallbackMessage, saveBlob } from "./exportFile";
import { renderDiagrams } from "./renderDiagrams";

export type ExportDeps = {
  exportApi: typeof exportApi;
  renderDiagrams: typeof renderDiagrams;
  exportFileName: typeof exportFileName;
  fallbackMessage: typeof fallbackMessage;
  saveBlob: typeof saveBlob;
  notify: Notify;
};

const SYNC_WAIT_MS = 5000;
const SYNC_POLL_MS = 100;
/** Failures worth one more try without the diagram pictures (a smaller,
 *  quicker request). Never a 503: neither a busy server nor an export the
 *  audit log couldn't record gets better without pictures. */
const RETRY_STATUSES = new Set([413, 408, 0]);

export function useExport(opts: {
  docId: string;
  title: string;
  status: ConnStatus;
  pendingBytes: number;
  deps?: Partial<ExportDeps>;
}): { exporting: boolean; run: (format: "docx" | "odt") => Promise<void> } {
  const { docId, title, status, pendingBytes } = opts;
  const storeNotify = useUi((s) => s.notify);
  const deps: ExportDeps = {
    exportApi,
    renderDiagrams,
    exportFileName,
    fallbackMessage,
    saveBlob,
    notify: storeNotify,
    ...opts.deps,
  };

  const [exporting, setExporting] = useState(false);
  const busyRef = useRef(false);
  const syncedRef = useRef(false);
  const depsRef = useRef(deps);

  useEffect(() => {
    syncedRef.current = status === "connected" && pendingBytes === 0;
  }, [status, pendingBytes]);
  useEffect(() => {
    depsRef.current = deps;
  });

  const run = useCallback(
    async (format: "docx" | "odt"): Promise<void> => {
      if (busyRef.current) return;
      busyRef.current = true;
      setExporting(true);
      const d = depsRef.current;
      try {
        // Give in-flight edits a moment to reach the server.
        let waited = 0;
        while (!syncedRef.current && waited < SYNC_WAIT_MS) {
          await new Promise<void>((r) => setTimeout(r, SYNC_POLL_MS));
          waited += SYNC_POLL_MS;
        }
        const unsynced = !syncedRef.current;

        const list = await d.exportApi.diagrams(docId);
        if ("error" in list) {
          fail(d, list.error);
          return;
        }
        const parts = await d.renderDiagrams(list.ok.diagrams, list.ok.limits);

        let res = await d.exportApi.file(docId, format, parts);
        if ("error" in res && RETRY_STATUSES.has(res.error.status)) {
          res = await d.exportApi.file(docId, format, new Map());
        }
        if ("error" in res) {
          fail(d, res.error);
          return;
        }
        d.saveBlob(res.ok.blob, d.exportFileName(title, format));
        const msg = d.fallbackMessage(res.ok.fallbacks, unsynced);
        if (msg !== null) d.notify("warn", msg);
      } catch {
        fail(d);
      } finally {
        busyRef.current = false;
        setExporting(false);
      }
    },
    [docId, title],
  );

  return { exporting, run };
}

function fail(d: ExportDeps, error?: ApiError): void {
  const limitText = error ? limitErrorText(error) : null;
  if (limitText !== null) d.notify("warn", limitText, { key: LIMIT_TOAST_KEY });
  else if (error?.code === "export.busy") d.notify("warn", "Export is busy, try again in a moment.");
  else if (isAuditUnavailable(error)) d.notify("error", AUDIT_UNAVAILABLE_TEXT);
  else d.notify("error", "Couldn't export this page. Try again, or export as Markdown.");
}
