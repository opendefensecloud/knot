import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { useState } from "react";

import { FoldHorizontal, UnfoldHorizontal } from "lucide-react";

import { authApi } from "../../auth/session.api";
import { useSession } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

import { readApiError } from "../../lib/api";
import { readCookie } from "../../lib/csrf";
import { limitErrorText, notifyFailure } from "../../lib/limitErrors";
import { downloadFile } from "../export/downloadFile";
import { SkinPicker } from "./SkinPicker";
import { workspaceApi } from "./workspace.api";

/** The server's import caps, `MAX_IMPORT_PAGES` and `MAX_IMPORT_BYTES` in
 *  `crates/knot-server/src/routes/api/export_import.rs`, for a 413 whose
 *  details don't name them. */
const DEFAULT_MAX_IMPORT_PAGES = 10_000;
const DEFAULT_MAX_IMPORT_BYTES = 512 * 1024 * 1024;

const BYTES_PER_MEBIBYTE = 1024 * 1024;

function positiveNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : null;
}

/** Shown on a 413 `import.too_large`: the caps from its details
 *  (`max_pages`, `max_bytes` and, when present, `max_markdown_bytes` and
 *  `max_references`), else the server's defaults. */
function importTooLargeText(details: Record<string, unknown>): string {
  const maxPages = positiveNumber(details.max_pages) ?? DEFAULT_MAX_IMPORT_PAGES;
  const maxBytes = positiveNumber(details.max_bytes) ?? DEFAULT_MAX_IMPORT_BYTES;
  const maxMarkdownBytes = positiveNumber(details.max_markdown_bytes);
  const maxReferences = positiveNumber(details.max_references);
  const asMebibytes = (bytes: number) =>
    (bytes / BYTES_PER_MEBIBYTE).toLocaleString("en-US", { maximumFractionDigits: 1 });
  const caps = [`${maxPages.toLocaleString("en-US")} pages`, `${asMebibytes(maxBytes)} MiB of files`];
  if (maxMarkdownBytes !== null) caps.push(`${asMebibytes(maxMarkdownBytes)} MiB of page text`);
  if (maxReferences !== null) caps.push(`${maxReferences.toLocaleString("en-US")} file references`);
  const lastCap = caps.pop();
  return `This archive is too large to import (over ${caps.join(", ")} or ${lastCap}).`;
}

export default function SettingsPage() {
  const ws = useQuery({ queryKey: ["workspace"], queryFn: () => workspaceApi.get() });
  const session = useSession();
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const docWidth = useUi((s) => s.docWidth);
  const toggleDocWidth = useUi((s) => s.toggleDocWidth);

  const [pwCurrent, setPwCurrent] = useState("");
  const [pwNew, setPwNew] = useState("");
  const [pwError, setPwError] = useState<string | null>(null);
  const [pwOk, setPwOk] = useState(false);
  const [exportingWorkspace, setExportingWorkspace] = useState(false);

  async function exportWorkspace() {
    setExportingWorkspace(true);
    try {
      await downloadFile({
        path: "/api/workspace/export",
        fileName: "knot-workspace-export.zip",
        notify,
        failureText: "Couldn't export the workspace.",
      });
    } finally {
      setExportingWorkspace(false);
    }
  }

  const changePw = useMutation({
    mutationFn: async () => authApi.changePassword(pwCurrent, pwNew),
    onMutate: () => { setPwError(null); setPwOk(false); },
    onSuccess: (r) => {
      if ("error" in r) {
        setPwError(
          limitErrorText(r.error)
            ?? (r.error.code === "auth.invalid_credentials" ? "Current password is wrong."
            : r.error.code === "auth.weak_password" ? "New password must be at least 8 characters."
            : r.error.code === "auth.password_reuse" ? "New password must differ from current."
            : "Couldn't change password."),
        );
        return;
      }
      setPwCurrent(""); setPwNew(""); setPwOk(true);
    },
  });

  async function logout() {
    const r = await authApi.logout();
    if ("error" in r) {
      notify("error", "Logout failed");
      return;
    }
    qc.removeQueries();
    await nav("/login", { replace: true });
  }

  return (
    <main className="mx-auto max-w-[760px] px-6 py-8">
      <h1 className="text-2xl font-semibold text-fg mb-6">Settings</h1>
      <div className="space-y-6">
        {ws.data && "ok" in ws.data && (
          <section data-testid="workspace-info" className="bg-surface border border-border rounded-lg px-5 py-4">
            <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-2">Workspace</h2>
            <p className="text-sm text-fg m-0">{ws.data.ok.name} <span className="text-fg-muted">({ws.data.ok.slug})</span></p>
          </section>
        )}
        {session.data && "ok" in session.data && (
          <section data-testid="user-info" className="bg-surface border border-border rounded-lg px-5 py-4">
            <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-2">Account</h2>
            <p className="text-sm text-fg m-0">{session.data.ok.display_name} <span className="text-fg-muted">({session.data.ok.email})</span></p>
            <p className="text-[13px] text-fg-muted mt-1">Role: <span className="text-fg">{session.data.ok.role}</span></p>
          </section>
        )}
        <section data-testid="appearance" className="bg-surface border border-border rounded-lg px-5 py-4">
          <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-3">Appearance</h2>
          <SkinPicker />
          <div className="flex items-center justify-between gap-3 mt-4 pt-4 border-t border-border">
            <div>
              <p className="text-sm text-fg m-0">Document width</p>
              <p className="text-[13px] text-fg-muted m-0 mt-0.5">
                Currently: {docWidth === "wide" ? "Wide" : "Fixed"} · applies on screens wider than 1024px
              </p>
            </div>
            <button
              type="button"
              data-testid="doc-width-toggle"
              onClick={toggleDocWidth}
              className="inline-flex items-center gap-2 h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted transition-colors"
            >
              {docWidth === "wide" ? (
                <>
                  <FoldHorizontal size={14} aria-hidden /> Switch to fixed
                </>
              ) : (
                <>
                  <UnfoldHorizontal size={14} aria-hidden /> Switch to wide
                </>
              )}
            </button>
          </div>
        </section>
        {session.data && "ok" in session.data && session.data.ok.role === "owner" && (
          <section data-testid="workspace-export-import" className="bg-surface border border-border rounded-lg px-5 py-4">
            <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-3">Backup</h2>
            <p className="text-[13px] text-fg-muted m-0 mb-3">
              Export the whole workspace as a zip — markdown bodies, attachments, board previews. Import a previously-exported zip to seed a new instance. Reach out to support if you need to migrate boards' edit history (v1 imports recreate boards with their preview only).
            </p>
            <div className="flex gap-2">
              <button
                type="button"
                data-testid="workspace-export"
                disabled={exportingWorkspace}
                onClick={() => { void exportWorkspace(); }}
                className="inline-flex items-center h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {exportingWorkspace ? "Exporting…" : "Export workspace"}
              </button>
              <label
                data-testid="workspace-import"
                className="inline-flex items-center h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity cursor-pointer"
              >
                Import zip…
                <input
                  type="file"
                  accept=".zip,application/zip"
                  className="hidden"
                  onChange={(e) => {
                    const file = e.target.files?.[0];
                    if (!file) return;
                    e.target.value = "";
                    void (async () => {
                      const csrf = readCookie("csrf") ?? "";
                      try {
                        const res = await fetch("/api/workspace/import", {
                          method: "POST",
                          credentials: "include",
                          headers: { "X-CSRF-Token": csrf, "Content-Type": "application/zip" },
                          body: file,
                        });
                        if (!res.ok) {
                          const error = await readApiError(res);
                          notifyFailure(
                            notify,
                            error,
                            error.code === "import.too_large" ? importTooLargeText(error.details) : `Import failed (${res.status}).`,
                          );
                          return;
                        }
                        const body = (await res.json()) as { imported_docs?: number };
                        notify("info", `Imported ${body.imported_docs ?? 0} documents.`);
                        await qc.invalidateQueries({ queryKey: ["docs"] });
                      } catch (err) {
                        notify("error", `Import failed: ${String(err)}`);
                      }
                    })();
                  }}
                />
              </label>
            </div>
            <p data-testid="workspace-export-audit-note" className="text-[12px] text-fg-muted m-0 mt-2">
              Exports are recorded in each page&apos;s audit log.
            </p>
          </section>
        )}
        <section data-testid="change-password" className="bg-surface border border-border rounded-lg px-5 py-4">
          <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-3">Change password</h2>
          <form
            onSubmit={(e) => { e.preventDefault(); changePw.mutate(); }}
            className="grid gap-3 max-w-sm"
          >
            <input
              data-testid="pw-current"
              type="password"
              autoComplete="current-password"
              placeholder="Current password"
              value={pwCurrent}
              onChange={(e) => setPwCurrent(e.target.value)}
              required
              className="h-9 px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm"
            />
            <input
              data-testid="pw-new"
              type="password"
              autoComplete="new-password"
              placeholder="New password (≥ 8 chars)"
              value={pwNew}
              onChange={(e) => setPwNew(e.target.value)}
              required
              minLength={8}
              className="h-9 px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm"
            />
            {pwError && <p data-testid="pw-error" role="alert" className="text-destructive text-[13px] m-0">{pwError}</p>}
            {pwOk && <p data-testid="pw-ok" className="text-emerald-600 text-[13px] m-0">Password updated.</p>}
            <button
              data-testid="pw-submit"
              type="submit"
              disabled={changePw.isPending}
              className="justify-self-start h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {changePw.isPending ? "Updating…" : "Update password"}
            </button>
          </form>
        </section>
        <section className="bg-surface border border-border rounded-lg px-5 py-4">
          <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-3">Session</h2>
          <button
            data-testid="logout"
            onClick={() => { void logout(); }}
            className="h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted transition-colors"
          >
            Sign out
          </button>
        </section>
      </div>
    </main>
  );
}
