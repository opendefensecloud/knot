import { useQueryClient } from "@tanstack/react-query";
import React, { useState } from "react";
import { useNavigate } from "react-router-dom";

import { authApi } from "../../auth/session.api";

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
        r.error.code === "auth.setup_closed"
          ? "Setup is already complete. Try signing in."
          : r.error.code === "auth.weak_password"
            ? "Password must be at least 8 characters."
            : "Setup failed.",
      );
      return;
    }
    await qc.invalidateQueries({ queryKey: ["session"] });
    await nav("/", { replace: true });
  }

  return (
    <main
      style={{
        maxWidth: 420,
        margin: "10vh auto",
        padding: 24,
        fontFamily: "system-ui, sans-serif",
      }}
    >
      <h1 style={{ marginBottom: 24 }}>First-run setup</h1>
      <p style={{ marginBottom: 16, color: "#555" }}>
        Create the workspace owner. This page closes after the first user is
        created.
      </p>
      <form data-testid="setup-form" onSubmit={(e) => { void onSubmit(e); }}>
        <label style={{ display: "block", marginBottom: 12 }}>
          <span style={{ display: "block", marginBottom: 4 }}>Email</span>
          <input
            data-testid="setup-email"
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
            style={{ width: "100%", padding: 8 }}
          />
        </label>
        <label style={{ display: "block", marginBottom: 12 }}>
          <span style={{ display: "block", marginBottom: 4 }}>Display name</span>
          <input
            data-testid="setup-display-name"
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
            required
            style={{ width: "100%", padding: 8 }}
          />
        </label>
        <label style={{ display: "block", marginBottom: 16 }}>
          <span style={{ display: "block", marginBottom: 4 }}>Password</span>
          <input
            data-testid="setup-password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
            minLength={8}
            style={{ width: "100%", padding: 8 }}
          />
        </label>
        {error && (
          <p data-testid="setup-error" style={{ color: "#b00020", marginBottom: 12 }}>
            {error}
          </p>
        )}
        <button
          data-testid="setup-submit"
          type="submit"
          disabled={busy}
          style={{ width: "100%", padding: 10 }}
        >
          {busy ? "Creating…" : "Create workspace"}
        </button>
      </form>
    </main>
  );
}
