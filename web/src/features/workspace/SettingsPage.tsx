import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { useState } from "react";

import { authApi } from "../../auth/session.api";
import { useSession } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

import { workspaceApi } from "./workspace.api";

export default function SettingsPage() {
  const ws = useQuery({ queryKey: ["workspace"], queryFn: () => workspaceApi.get() });
  const session = useSession();
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);

  const [pwCurrent, setPwCurrent] = useState("");
  const [pwNew, setPwNew] = useState("");
  const [pwError, setPwError] = useState<string | null>(null);
  const [pwOk, setPwOk] = useState(false);

  const changePw = useMutation({
    mutationFn: async () => authApi.changePassword(pwCurrent, pwNew),
    onMutate: () => { setPwError(null); setPwOk(false); },
    onSuccess: (r) => {
      if ("error" in r) {
        setPwError(
          r.error.code === "auth.invalid_credentials" ? "Current password is wrong."
            : r.error.code === "auth.weak_password" ? "New password must be at least 8 characters."
            : r.error.code === "auth.password_reuse" ? "New password must differ from current."
            : "Couldn't change password.",
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
        {session.data && "ok" in session.data && session.data.ok.role === "owner" && (
          <section data-testid="workspace-export-import" className="bg-surface border border-border rounded-lg px-5 py-4">
            <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-3">Backup</h2>
            <p className="text-[13px] text-fg-muted m-0 mb-3">
              Export the whole workspace as a zip — markdown bodies, attachments, board previews. Import a previously-exported zip to seed a new instance. Reach out to support if you need to migrate boards' edit history (v1 imports recreate boards with their preview only).
            </p>
            <div className="flex gap-2">
              <a
                data-testid="workspace-export"
                href="/api/workspace/export"
                download="knot-workspace-export.zip"
                className="inline-flex items-center h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted transition-colors"
              >
                Export workspace
              </a>
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
                      const csrf = document.cookie.split("; ").find((c) => c.startsWith("csrf="))?.split("=")[1] ?? "";
                      try {
                        const res = await fetch("/api/workspace/import", {
                          method: "POST",
                          credentials: "include",
                          headers: { "X-CSRF-Token": decodeURIComponent(csrf), "Content-Type": "application/zip" },
                          body: file,
                        });
                        if (!res.ok) {
                          notify("error", `Import failed (${res.status}).`);
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
