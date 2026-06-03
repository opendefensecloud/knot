import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";

import { useUi } from "../../stores/ui";
import { sharesApi, type Share } from "../../lib/shares.api";
import { grantsApi } from "../docs/grants.api";
import { workspaceApi } from "../workspace/workspace.api";

function toLocalInput(iso: string): string {
  // Convert ISO string to the `YYYY-MM-DDTHH:mm` format expected by
  // <input type="datetime-local">.
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export default function PermissionsDialog() {
  const { id } = useParams<{ id: string }>();
  const nav = useNavigate();
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);

  const grants = useQuery({
    queryKey: ["grants", id],
    queryFn: () => grantsApi.list(id!),
    enabled: Boolean(id),
  });
  const members = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
  });

  const shares = useQuery({
    queryKey: ["shares", id],
    queryFn: () => sharesApi.list(id!),
    enabled: Boolean(id),
  });
  const publicLink: Share | null =
    shares.data && "ok" in shares.data && shares.data.ok.length > 0
      ? shares.data.ok[0]!
      : null;

  // Track localExpiry as [shareId, value] so resetting on share change doesn't
  // require an effect (avoids react-hooks/set-state-in-effect lint error).
  const serverExpiry = publicLink?.expires_at ? toLocalInput(publicLink.expires_at) : "";
  const [expiryState, setExpiryState] = useState<{ shareId: string | null; value: string }>({
    shareId: null,
    value: "",
  });
  // Derived: if the tracked shareId changed, use the server value instead.
  const localExpiry =
    expiryState.shareId === (publicLink?.id ?? null)
      ? expiryState.value
      : serverExpiry;
  const setLocalExpiry = (v: string) =>
    setExpiryState({ shareId: publicLink?.id ?? null, value: v });

  const onEnable = async () => {
    const r = await sharesApi.create(id!, null);
    if ("error" in r) { notify("error", "Couldn't create share link"); return; }
    await qc.invalidateQueries({ queryKey: ["shares", id] });
  };
  const onRevoke = async () => {
    if (!publicLink) return;
    const r = await sharesApi.revoke(id!, publicLink.id);
    if ("error" in r) { notify("error", "Couldn't revoke share link"); return; }
    await qc.invalidateQueries({ queryKey: ["shares", id] });
  };
  const updateExpiry = async () => {
    if (!publicLink) return;
    // v0.1: revoke + recreate with new expiry. Simpler than a PATCH.
    await sharesApi.revoke(id!, publicLink.id);
    const iso = localExpiry ? new Date(localExpiry).toISOString() : null;
    const r = await sharesApi.create(id!, iso);
    if ("error" in r) { notify("error", "Couldn't update expiry"); return; }
    await qc.invalidateQueries({ queryKey: ["shares", id] });
  };

  const [addUser, setAddUser] = useState("");
  const [addRole, setAddRole] = useState<"owner" | "editor" | "viewer">("viewer");
  const [addInherit, setAddInherit] = useState(true);

  const add = useMutation({
    mutationFn: async () =>
      grantsApi.put(id!, `user:${addUser}`, addRole, addInherit),
    onSuccess: async (r) => {
      if ("error" in r) notify("error", "Couldn't add grant");
      else {
        setAddUser("");
        await qc.invalidateQueries({ queryKey: ["grants", id] });
      }
    },
  });
  const remove = useMutation({
    mutationFn: async (principal: string) => grantsApi.remove(id!, principal),
    onSuccess: async (r) => {
      if ("error" in r) notify("error", "Couldn't remove grant");
      else await qc.invalidateQueries({ queryKey: ["grants", id] });
    },
  });

  if (!id) return null;

  return (
    <div
      role="dialog"
      data-testid="permissions-dialog"
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.4)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        zIndex: 30,
      }}
      onClick={() => { void nav(-1); }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          background: "white",
          padding: 24,
          minWidth: 420,
          borderRadius: 6,
          maxHeight: "80vh",
          overflow: "auto",
        }}
      >
        <h2>Permissions</h2>

        <section style={{ marginBottom: 24, padding: 12, border: "1px solid #e5e5e5", borderRadius: 6 }}>
          <h3 style={{ marginTop: 0 }}>Public link</h3>

          {publicLink ? (
            <>
              <p style={{ color: "#555", fontSize: 14 }}>
                Anyone with this URL can read the document.
              </p>
              <div style={{ display: "flex", gap: 8, marginBottom: 12 }}>
                <input
                  data-testid="share-url"
                  readOnly
                  value={publicLink.url}
                  style={{ flex: 1, padding: 6, fontFamily: "ui-monospace, monospace" }}
                />
                <button
                  data-testid="share-copy"
                  type="button"
                  onClick={() => {
                    void navigator.clipboard.writeText(publicLink.url).then(() => {
                      notify("info", "Copied!");
                    });
                  }}
                >Copy</button>
              </div>
              <label style={{ display: "block", marginBottom: 8 }}>
                Expires:{" "}
                <input
                  data-testid="share-expiry"
                  type="datetime-local"
                  value={localExpiry}
                  onChange={(e) => setLocalExpiry(e.target.value)}
                />
                <button
                  data-testid="share-save-expiry"
                  type="button"
                  disabled={localExpiry === (publicLink.expires_at ? toLocalInput(publicLink.expires_at) : "")}
                  onClick={() => void updateExpiry()}
                  style={{ marginLeft: 8 }}
                >Save</button>
              </label>
              <p style={{ color: "#888", fontSize: 12 }}>
                {publicLink.expires_at
                  ? `Expires ${new Date(publicLink.expires_at).toLocaleString()}`
                  : "No expiry"}
                {" · "}Created {new Date(publicLink.created_at).toLocaleString()}
              </p>
              <button
                data-testid="share-revoke"
                type="button"
                onClick={() => void onRevoke()}
                style={{ color: "#b00020" }}
              >Revoke</button>
            </>
          ) : (
            <>
              <p style={{ color: "#555", fontSize: 14 }}>
                Off — only people with workspace access can view this document.
              </p>
              <button
                data-testid="share-enable"
                type="button"
                onClick={() => void onEnable()}
              >Enable public link</button>
            </>
          )}
        </section>

        <p style={{ color: "#666", marginBottom: 16 }}>Explicit grants on this document.</p>
        <table style={{ width: "100%", borderCollapse: "collapse", marginBottom: 16 }}>
          <thead>
            <tr>
              <th style={{ textAlign: "left", padding: 6 }}>Principal</th>
              <th style={{ textAlign: "left", padding: 6 }}>Role</th>
              <th style={{ textAlign: "left", padding: 6 }}>Inherits</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {grants.data && "ok" in grants.data && grants.data.ok.map((g) => (
              <tr key={g.principal} data-testid={`grant-${g.principal}`}>
                <td style={{ padding: 6 }}>{g.principal}</td>
                <td style={{ padding: 6 }}>{g.role}</td>
                <td style={{ padding: 6 }}>{g.inherit ? "yes" : "no"}</td>
                <td style={{ padding: 6 }}>
                  <button onClick={() => remove.mutate(g.principal)}>Remove</button>
                </td>
              </tr>
            ))}
            {grants.data && "ok" in grants.data && grants.data.ok.length === 0 && (
              <tr>
                <td colSpan={4} style={{ padding: 6, color: "#888" }}>
                  No explicit grants. Effective role comes from workspace + ancestor inherits.
                </td>
              </tr>
            )}
          </tbody>
        </table>

        <h3>Add</h3>
        <div style={{ display: "flex", gap: 8, marginBottom: 12 }}>
          <select
            data-testid="grant-user"
            value={addUser}
            onChange={(e) => setAddUser(e.target.value)}
          >
            <option value="">Choose…</option>
            {members.data && "ok" in members.data && members.data.ok.map((m) => (
              <option key={m.user_id} value={m.user_id}>
                {m.display_name} ({m.email})
              </option>
            ))}
          </select>
          <select
            data-testid="grant-role"
            value={addRole}
            onChange={(e) => setAddRole(e.target.value as typeof addRole)}
          >
            <option value="viewer">Viewer</option>
            <option value="editor">Editor</option>
            <option value="owner">Owner</option>
          </select>
          <label style={{ display: "flex", alignItems: "center", gap: 4 }}>
            <input
              type="checkbox"
              checked={addInherit}
              onChange={(e) => setAddInherit(e.target.checked)}
            />
            Inherit
          </label>
          <button
            data-testid="grant-add"
            disabled={!addUser}
            onClick={() => add.mutate()}
          >
            Add
          </button>
        </div>

        <div style={{ display: "flex", justifyContent: "flex-end" }}>
          <button onClick={() => { void nav(-1); }}>Close</button>
        </div>
      </div>
    </div>
  );
}
