import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useUi } from "../../stores/ui";

import { workspaceApi } from "./workspace.api";

export default function MembersPage() {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const { workspace } = useEffectiveRole();
  const isOwner = workspace === "owner";

  const members = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
  });

  const [inviteEmail, setInviteEmail] = useState("");
  const [inviteRole, setInviteRole] = useState<"owner" | "editor" | "viewer">("editor");
  const [invitePassword, setInvitePassword] = useState("");

  const invite = useMutation({
    mutationFn: async () =>
      workspaceApi.invite(inviteEmail, inviteRole, invitePassword || undefined),
    onSuccess: async (r) => {
      if ("error" in r) {
        const msg =
          r.error.code === "workspace.user_not_found"
            ? "User not found. Add a password to create the account."
            : r.error.code === "auth.weak_password"
              ? "Password must be at least 8 characters."
              : "Invite failed.";
        notify("error", msg);
        return;
      }
      setInviteEmail("");
      setInvitePassword("");
      await qc.invalidateQueries({ queryKey: ["members"] });
    },
  });

  const setRole = useMutation({
    mutationFn: async (a: { userId: string; role: "owner" | "editor" | "viewer" }) =>
      workspaceApi.setRole(a.userId, a.role),
    onSuccess: async (r) => {
      if ("error" in r) notify("error", "Role change failed");
      else await qc.invalidateQueries({ queryKey: ["members"] });
    },
  });

  const remove = useMutation({
    mutationFn: async (userId: string) => workspaceApi.remove(userId),
    onSuccess: async (r) => {
      if ("error" in r) notify("error", "Remove failed");
      else await qc.invalidateQueries({ queryKey: ["members"] });
    },
  });

  if (members.isLoading) return <main style={{ padding: 24 }}>Loading…</main>;
  if (!members.data || "error" in members.data) {
    return <main style={{ padding: 24 }}>Failed to load members.</main>;
  }

  return (
    <main style={{ padding: 24, fontFamily: "system-ui, sans-serif" }}>
      <h1>Members</h1>
      {isOwner && (
        <section style={{ marginTop: 12, marginBottom: 24 }}>
          <h2>Invite</h2>
          <form
            data-testid="invite-form"
            onSubmit={(e) => { e.preventDefault(); invite.mutate(); }}
            style={{ display: "flex", gap: 8 }}
          >
            <input
              data-testid="invite-email"
              type="email"
              value={inviteEmail}
              onChange={(e) => setInviteEmail(e.target.value)}
              placeholder="Email"
              required
              style={{ padding: 6 }}
            />
            <select
              data-testid="invite-role"
              value={inviteRole}
              onChange={(e) => setInviteRole(e.target.value as typeof inviteRole)}
              style={{ padding: 6 }}
            >
              <option value="viewer">Viewer</option>
              <option value="editor">Editor</option>
              <option value="owner">Owner</option>
            </select>
            <input
              data-testid="invite-password"
              type="password"
              value={invitePassword}
              onChange={(e) => setInvitePassword(e.target.value)}
              placeholder="Initial password (optional)"
              minLength={8}
              style={{ padding: 6 }}
            />
            <button data-testid="invite-submit" type="submit" style={{ padding: "6px 12px" }}>
              Invite
            </button>
          </form>
        </section>
      )}
      <table data-testid="members-table" style={{ width: "100%", borderCollapse: "collapse" }}>
        <thead>
          <tr>
            <th style={{ textAlign: "left", padding: 8, borderBottom: "1px solid #e5e5e5" }}>Email</th>
            <th style={{ textAlign: "left", padding: 8, borderBottom: "1px solid #e5e5e5" }}>Name</th>
            <th style={{ textAlign: "left", padding: 8, borderBottom: "1px solid #e5e5e5" }}>Role</th>
            {isOwner && <th style={{ padding: 8, borderBottom: "1px solid #e5e5e5" }}>Actions</th>}
          </tr>
        </thead>
        <tbody>
          {members.data.ok.map((m) => (
            <tr key={m.user_id} data-testid={`member-${m.user_id}`}>
              <td style={{ padding: 8 }}>{m.email}</td>
              <td style={{ padding: 8 }}>{m.display_name}</td>
              <td style={{ padding: 8 }}>
                {isOwner ? (
                  <select
                    value={m.role}
                    onChange={(e) =>
                      setRole.mutate({ userId: m.user_id, role: e.target.value as typeof inviteRole })
                    }
                  >
                    <option value="viewer">Viewer</option>
                    <option value="editor">Editor</option>
                    <option value="owner">Owner</option>
                  </select>
                ) : (
                  m.role
                )}
              </td>
              {isOwner && (
                <td style={{ padding: 8 }}>
                  <button
                    onClick={() => {
                      if (window.confirm(`Remove ${m.email}?`)) remove.mutate(m.user_id);
                    }}
                  >
                    Remove
                  </button>
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
    </main>
  );
}
