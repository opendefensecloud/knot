import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";

import { useUi } from "../../stores/ui";
import { sharesApi, type Share } from "../../lib/shares.api";
import type { Member } from "../../lib/validators";
import { grantsApi } from "../docs/grants.api";
import { workspaceApi } from "../workspace/workspace.api";

function toLocalInput(iso: string): string {
  // Convert ISO string to the `YYYY-MM-DDTHH:mm` format expected by
  // <input type="datetime-local">.
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function PrincipalLabel({
  principal,
  member,
  membersLoaded,
}: {
  principal: string;
  member: Member | undefined;
  membersLoaded: boolean;
}) {
  if (member) {
    return (
      <>
        <div className="text-fg">{member.display_name}</div>
        <div className="text-fg-muted text-[12px]">{member.email}</div>
      </>
    );
  }
  // Hold off until the member list is in, so ids don't flash before names.
  if (!membersLoaded) return <span className="text-fg-muted">…</span>;
  // A user grant can outlive the membership; keep the id so it stays removable
  // and identifiable.
  return (
    <>
      {principal.startsWith("user:") && <div className="text-fg-muted">Former member</div>}
      <div className="text-fg-muted font-mono text-[12px]">{principal}</div>
    </>
  );
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
  // Grants name users by `user:<uuid>`; resolve that against the member list
  // so the table shows people, not ids.
  const memberById = new Map<string, Member>(
    members.data && "ok" in members.data ? members.data.ok.map((m) => [m.user_id, m]) : [],
  );

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

  const inputCls = "h-9 px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm";
  const selectCls = "h-9 px-2 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent text-sm";
  const btnPrimaryCls = "h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed";
  const btnSecondaryCls = "h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted transition-colors";

  return (
    <div
      role="dialog"
      data-testid="permissions-dialog"
      className="fixed inset-0 z-30 bg-black/40 backdrop-blur-sm flex items-center justify-center p-4"
      onClick={() => { void nav(-1); }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-xl bg-surface border border-border rounded-lg shadow-2xl max-h-[85vh] overflow-auto p-6"
      >
        <h2 className="text-xl font-semibold text-fg mb-4">Permissions</h2>

        <section className="mb-6 px-4 py-3 border border-border rounded">
          <h3 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mt-0 mb-2">Public link</h3>
          {publicLink ? (
            <>
              <p className="text-fg-muted text-[13px] m-0 mb-3">
                Anyone with this URL can read the document.
              </p>
              <div className="flex gap-2 mb-3">
                <input
                  data-testid="share-url"
                  readOnly
                  value={publicLink.url}
                  className={`${inputCls} flex-1 font-mono`}
                />
                <button
                  data-testid="share-copy"
                  type="button"
                  onClick={() => {
                    void navigator.clipboard.writeText(publicLink.url).then(() => notify("info", "Copied!"));
                  }}
                  className={btnSecondaryCls}
                >
                  Copy
                </button>
              </div>
              <label className="flex items-center gap-2 mb-2 text-[13px] text-fg">
                Expires:
                <input
                  data-testid="share-expiry"
                  type="datetime-local"
                  value={localExpiry}
                  onChange={(e) => setLocalExpiry(e.target.value)}
                  className={selectCls}
                />
                <button
                  data-testid="share-save-expiry"
                  type="button"
                  disabled={localExpiry === (publicLink.expires_at ? toLocalInput(publicLink.expires_at) : "")}
                  onClick={() => void updateExpiry()}
                  className={btnSecondaryCls}
                >
                  Save
                </button>
              </label>
              <p className="text-fg-muted text-[12px] m-0 mb-2">
                {publicLink.expires_at
                  ? `Expires ${new Date(publicLink.expires_at).toLocaleString()}`
                  : "No expiry"}
                {" · "}Created {new Date(publicLink.created_at).toLocaleString()}
              </p>
              <button
                data-testid="share-revoke"
                type="button"
                onClick={() => void onRevoke()}
                className="h-8 px-2.5 rounded text-destructive text-[13px] font-medium hover:bg-destructive/10 transition-colors"
              >
                Revoke
              </button>
            </>
          ) : (
            <>
              <p className="text-fg-muted text-[13px] m-0 mb-3">
                Off — only people with workspace access can view this document.
              </p>
              <button
                data-testid="share-enable"
                type="button"
                onClick={() => void onEnable()}
                className={btnPrimaryCls}
              >
                Enable public link
              </button>
            </>
          )}
        </section>

        <p className="text-fg-muted text-[13px] mb-3">Explicit grants on this document.</p>
        <div className="bg-bg border border-border rounded overflow-hidden mb-4">
          <table className="w-full border-collapse text-sm">
            <thead>
              <tr className="bg-muted/60">
                <th className="text-left px-3 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider">Principal</th>
                <th className="text-left px-3 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider">Role</th>
                <th className="text-left px-3 py-2 text-fg-muted font-medium text-[12px] uppercase tracking-wider">Inherits</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {grants.data && "ok" in grants.data && grants.data.ok.map((g) => (
                <tr key={g.principal} data-testid={`grant-${g.principal}`} className="border-t border-border">
                  <td className="px-3 py-2">
                    <PrincipalLabel
                      principal={g.principal}
                      member={memberById.get(g.principal.replace(/^user:/, ""))}
                      membersLoaded={!members.isPending}
                    />
                  </td>
                  <td className="px-3 py-2 text-fg">{g.role}</td>
                  <td className="px-3 py-2 text-fg-muted">{g.inherit ? "yes" : "no"}</td>
                  <td className="px-3 py-2 whitespace-nowrap">
                    <button
                      onClick={() => remove.mutate(g.principal)}
                      className="h-8 px-2.5 rounded text-destructive text-[13px] font-medium hover:bg-destructive/10 transition-colors"
                    >
                      Remove
                    </button>
                  </td>
                </tr>
              ))}
              {grants.data && "ok" in grants.data && grants.data.ok.length === 0 && (
                <tr>
                  <td colSpan={4} className="px-3 py-3 text-fg-muted text-[13px]">
                    No explicit grants. Effective role comes from workspace + ancestor inherits.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>

        <h3 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-2">Add</h3>
        <div className="flex flex-wrap gap-2 mb-4 items-center">
          <select
            data-testid="grant-user"
            value={addUser}
            onChange={(e) => setAddUser(e.target.value)}
            className={`${selectCls} flex-1 min-w-[180px]`}
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
            className={selectCls}
          >
            <option value="viewer">Viewer</option>
            <option value="editor">Editor</option>
            <option value="owner">Owner</option>
          </select>
          <label className="flex items-center gap-1.5 text-[13px] text-fg">
            <input
              type="checkbox"
              checked={addInherit}
              onChange={(e) => setAddInherit(e.target.checked)}
              className="accent-accent"
            />
            Inherit
          </label>
          <button
            data-testid="grant-add"
            disabled={!addUser}
            onClick={() => add.mutate()}
            className={btnPrimaryCls}
          >
            Add
          </button>
        </div>

        <div className="flex justify-end pt-2 border-t border-border">
          <button onClick={() => { void nav(-1); }} className={btnSecondaryCls}>Close</button>
        </div>
      </div>
    </div>
  );
}
