import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";

import type { Member } from "../../lib/validators";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useUi } from "../../stores/ui";
import { ApiKeysSection } from "../apiKeys/ApiKeysSection";

import { memberMatches } from "./memberSearch";
import { workspaceApi } from "./workspace.api";

// Case-insensitive substring match on email, display name and role, so
// "anna", "@example.org" and "owner" all narrow the list.
export function filterMembers(members: Member[], query: string): Member[] {
  const q = query.trim().toLowerCase();
  if (!q) return members;
  return members.filter((m) => memberMatches(m, q) || m.role.includes(q));
}

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
  const [inviteBot, setInviteBot] = useState(false);
  const [query, setQuery] = useState("");
  const [keysFor, setKeysFor] = useState<string | null>(null);

  const invite = useMutation({
    mutationFn: async () =>
      inviteBot
        ? workspaceApi.invite(inviteEmail, inviteRole === "owner" ? "editor" : inviteRole, undefined, inviteName || undefined, "bot")
        : workspaceApi.invite(inviteEmail, inviteRole, invitePassword || undefined, inviteName || undefined),
    onSuccess: async (r) => {
      if ("error" in r) {
        const msg =
          r.error.code === "workspace.user_not_found"
            ? "User not found. Add a password to create the account."
            : r.error.code === "auth.weak_password"
              ? "Password must be at least 8 characters."
              : r.error.code === "bot.owner_forbidden"
                ? "A bot cannot be an owner."
                : r.error.code === "workspace.email_taken"
                  ? "That email already belongs to an account. Use a new email for a bot."
                  : "Invite failed.";
        notify("error", msg);
        return;
      }
      setInviteEmail("");
      setInvitePassword("");
      setInviteName("");
      setInviteBot(false);
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

  const all = members.data.ok;
  const shown = filterMembers(all, query);

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
              {!inviteBot && <option value="owner">Owner</option>}
            </select>
            {!inviteBot && (
              <input
                data-testid="invite-password"
                type="password"
                value={invitePassword}
                onChange={(e) => setInvitePassword(e.target.value)}
                placeholder="Initial password (optional)"
                minLength={8}
                className={`${inputCls} flex-1 min-w-[180px]`}
              />
            )}
            <label className="inline-flex items-center gap-1.5 text-sm text-fg">
              <input
                data-testid="invite-kind-bot"
                type="checkbox"
                checked={inviteBot}
                onChange={(e) => {
                  setInviteBot(e.target.checked);
                  if (e.target.checked && inviteRole === "owner") setInviteRole("editor");
                }}
              />
              Bot account
            </label>
            <button
              data-testid="invite-submit"
              type="submit"
              className="h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity"
            >
              Invite
            </button>
          </form>
          {inviteBot && (
            <p data-testid="invite-bot-hint" className="mt-2 text-[13px] text-fg-muted">
              A bot has no password and logs in only with an API key.
            </p>
          )}
        </section>
      )}
      <div className="mb-3 flex items-center gap-3">
        <input
          data-testid="members-search"
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Escape") setQuery(""); }}
          placeholder="Search members by name, email or role…"
          aria-label="Search members"
          className={`${inputCls} flex-1`}
        />
        <span data-testid="members-count" className="text-[13px] text-fg-muted whitespace-nowrap" aria-live="polite">
          {query.trim() ? `${shown.length} of ${all.length}` : `${all.length} ${all.length === 1 ? "member" : "members"}`}
        </span>
      </div>
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
            {shown.length === 0 && (
              <tr className="border-t border-border">
                <td colSpan={isOwner ? 4 : 3} data-testid="members-empty" className="px-4 py-6 text-center text-fg-muted">
                  No members match “{query.trim()}”.
                </td>
              </tr>
            )}
            {shown.map((m) => (
              <Fragment key={m.user_id}>
                <tr data-testid={`member-${m.user_id}`} className="border-t border-border">
                  <td className="px-4 py-2 text-fg">{m.email}</td>
                  <td className="px-4 py-2 text-fg">
                    {m.display_name}
                    {m.kind === "bot" && (
                      <span data-testid="member-bot-badge" className="ml-2 inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-accent/15 text-accent">
                        Bot
                      </span>
                    )}
                  </td>
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
                        {m.kind !== "bot" && <option value="owner">Owner</option>}
                      </select>
                    ) : (
                      <span className="inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-muted text-fg-muted">{m.role}</span>
                    )}
                  </td>
                  {isOwner && (
                    <td className="px-4 py-2 whitespace-nowrap">
                      {m.kind === "bot" && (
                        <button
                          data-testid="member-api-keys"
                          aria-expanded={keysFor === m.user_id}
                          onClick={() => setKeysFor(keysFor === m.user_id ? null : m.user_id)}
                          className="h-8 px-2.5 rounded text-fg text-[13px] font-medium hover:bg-muted transition-colors"
                        >
                          API keys
                        </button>
                      )}
                      <button
                        onClick={() => {
                          const question = `Remove ${m.email}? They also lose every page-level grant, including access to restricted pages.`;
                          if (window.confirm(question)) remove.mutate(m.user_id);
                        }}
                        className="h-8 px-2.5 rounded text-destructive text-[13px] font-medium hover:bg-destructive/10 transition-colors"
                      >
                        Remove
                      </button>
                    </td>
                  )}
                </tr>
                {isOwner && m.kind === "bot" && keysFor === m.user_id && (
                  <tr data-testid={`member-api-keys-panel-${m.user_id}`} className="border-t border-border">
                    <td colSpan={4} className="px-4 py-3">
                      <ApiKeysSection targetUserId={m.user_id} />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
    </main>
  );
}
