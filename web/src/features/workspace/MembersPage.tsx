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
  const [inviteName, setInviteName] = useState("");

  const invite = useMutation({
    mutationFn: async () =>
      workspaceApi.invite(inviteEmail, inviteRole, invitePassword || undefined, inviteName || undefined),
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
      setInviteName("");
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

  if (members.isLoading) return <main className="mx-auto max-w-[760px] px-6 py-8 text-fg-muted">Loading…</main>;
  if (!members.data || "error" in members.data) {
    return <main className="mx-auto max-w-[760px] px-6 py-8 text-fg-muted">Failed to load members.</main>;
  }

  const inputCls = "h-9 px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm";
  const selectCls = "h-9 px-2 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent text-sm";

  return (
    <main className="mx-auto max-w-[760px] px-6 py-8">
      <h1 className="text-2xl font-semibold text-fg mb-6">Members</h1>
      {isOwner && (
        <section className="mb-6 bg-surface border border-border rounded-lg px-5 py-4">
          <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-3">Invite</h2>
          <form
            data-testid="invite-form"
            onSubmit={(e) => { e.preventDefault(); invite.mutate(); }}
            className="flex flex-wrap gap-2"
          >
            <input
              data-testid="invite-email"
              type="email"
              value={inviteEmail}
              onChange={(e) => setInviteEmail(e.target.value)}
              placeholder="Email"
              required
              className={`${inputCls} flex-1 min-w-[180px]`}
            />
            <input
              data-testid="invite-display-name"
              type="text"
              value={inviteName}
              onChange={(e) => setInviteName(e.target.value)}
              placeholder="Display name (optional)"
              className={`${inputCls} flex-1 min-w-[160px]`}
            />
            <select
              data-testid="invite-role"
              value={inviteRole}
              onChange={(e) => setInviteRole(e.target.value as typeof inviteRole)}
              className={selectCls}
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
              className={`${inputCls} flex-1 min-w-[180px]`}
            />
            <button
              data-testid="invite-submit"
              type="submit"
              className="h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity"
            >
              Invite
            </button>
          </form>
        </section>
      )}
      <div className="bg-surface border border-border rounded-lg overflow-hidden">
        <table data-testid="members-table" className="w-full border-collapse text-sm">
          <thead>
            <tr className="bg-muted/60">
              <th className="text-left px-4 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider">Email</th>
              <th className="text-left px-4 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider">Name</th>
              <th className="text-left px-4 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider">Role</th>
              {isOwner && <th className="px-4 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider w-px whitespace-nowrap">Actions</th>}
            </tr>
          </thead>
          <tbody>
            {members.data.ok.map((m) => (
              <tr key={m.user_id} data-testid={`member-${m.user_id}`} className="border-t border-border">
                <td className="px-4 py-2 text-fg">{m.email}</td>
                <td className="px-4 py-2 text-fg">{m.display_name}</td>
                <td className="px-4 py-2">
                  {isOwner ? (
                    <select
                      value={m.role}
                      onChange={(e) =>
                        setRole.mutate({ userId: m.user_id, role: e.target.value as typeof inviteRole })
                      }
                      className={selectCls}
                    >
                      <option value="viewer">Viewer</option>
                      <option value="editor">Editor</option>
                      <option value="owner">Owner</option>
                    </select>
                  ) : (
                    <span className="inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-muted text-fg-muted">{m.role}</span>
                  )}
                </td>
                {isOwner && (
                  <td className="px-4 py-2 whitespace-nowrap">
                    <button
                      onClick={() => {
                        if (window.confirm(`Remove ${m.email}?`)) remove.mutate(m.user_id);
                      }}
                      className="h-8 px-2.5 rounded text-destructive text-[13px] font-medium hover:bg-destructive/10 transition-colors"
                    >
                      Remove
                    </button>
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </main>
  );
}
