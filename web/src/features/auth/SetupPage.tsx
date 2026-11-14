import { useQueryClient } from "@tanstack/react-query";
import React, { useState } from "react";
import { useNavigate } from "react-router-dom";

import { authApi } from "../../auth/session.api";
import { limitErrorText } from "../../lib/limitErrors";

export default function SetupPage() {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const nav = useNavigate();
  const qc = useQueryClient();

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const r = await authApi.setup(email, password, displayName);
    setBusy(false);
    if ("error" in r) {
      setError(
        limitErrorText(r.error)
          ?? (r.error.code === "auth.setup_closed"
            ? "Setup is already complete. Try signing in."
            : r.error.code === "auth.weak_password"
              ? "Password must be at least 8 characters."
              : "Setup failed."),
      );
      return;
    }
    await qc.invalidateQueries({ queryKey: ["session"] });
    await nav("/", { replace: true });
  }

  return (
    <main className="min-h-dvh flex items-center justify-center px-4 bg-bg">
      <div className="w-full max-w-md bg-surface border border-border rounded-lg shadow-sm p-6">
        <h1 className="text-xl font-semibold text-fg mb-1">First-run setup</h1>
        <p className="text-sm text-fg-muted mb-6">
          Create the workspace owner. This page closes after the first user is created.
        </p>
        <form data-testid="setup-form" onSubmit={(e) => { void onSubmit(e); }} className="space-y-4">
          <label className="block">
            <span className="block text-[13px] font-medium text-fg mb-1">Email</span>
            <input
              data-testid="setup-email"
              type="email"
              autoComplete="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              required
              className="h-9 w-full px-3 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent text-sm"
            />
          </label>
          <label className="block">
            <span className="block text-[13px] font-medium text-fg mb-1">Display name</span>
            <input
              data-testid="setup-display-name"
              value={displayName}
              onChange={(e) => setDisplayName(e.target.value)}
              required
              className="h-9 w-full px-3 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent text-sm"
            />
          </label>
          <label className="block">
            <span className="block text-[13px] font-medium text-fg mb-1">Password</span>
            <input
              data-testid="setup-password"
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              required
              minLength={8}
              className="h-9 w-full px-3 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent text-sm"
            />
            <span className="block text-[12px] text-fg-muted mt-1">At least 8 characters.</span>
          </label>
          {error && (
            <p data-testid="setup-error" role="alert" className="text-destructive text-[13px]">
              {error}
            </p>
          )}
          <button
            data-testid="setup-submit"
            type="submit"
            disabled={busy}
            className="w-full h-9 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {busy ? "Creating…" : "Create workspace"}
          </button>
        </form>
      </div>
    </main>
  );
}
