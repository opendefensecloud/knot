import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";

import { useSession } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

import { API_KEY_SCOPES, type ApiKey, type ApiKeyScope, apiKeysApi, apiKeysQueryKey } from "./apiKeys.api";

const SCOPE_LABEL: Record<ApiKeyScope, string> = {
  read: "Read",
  create: "Create",
  update: "Update",
  delete: "Delete",
};

const EXPIRY_DAYS = [30, 90, 180, 365] as const;
const NEVER = "never";

export function apiKeyErrorMessage(code: string): string {
  switch (code) {
    case "api_key.ttl_too_short": return "Keys must be valid for at least 30 days.";
    case "api_key.ttl_too_long": return "That expiry is longer than your account allows.";
    case "api_key.scopes_invalid": return "Pick at least one scope.";
    case "api_key.name_invalid": return "Name must be 1–100 characters.";
    case "api_key.session_required": return "API keys can only be managed from a signed-in browser session.";
    case "api_key.foreign_forbidden": return "You can't manage keys for this user.";
    case "api_key.not_found": return "That key no longer exists.";
    case "api_key.limit_reached": return "This account already has 50 active keys. Revoke one before creating another.";
    default: return "Something went wrong with the API key request.";
  }
}

function fmtDate(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleDateString();
}

type Props = {
  /** When set, the caller (a workspace owner) manages this bot's keys. */
  targetUserId?: string;
};

export function ApiKeysSection({ targetUserId }: Props) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const session = useSession();
  const queryKey = apiKeysQueryKey(targetUserId);
  const keys = useQuery({ queryKey, queryFn: () => apiKeysApi.list(targetUserId) });

  // Bots may hold non-expiring keys; for oneself the session states the cap.
  const maxTtl = targetUserId
    ? null
    : session.data && "ok" in session.data ? session.data.ok.max_api_key_ttl_days : null;
  const expiryOptions = EXPIRY_DAYS.filter((d) => maxTtl === null || d <= maxTtl);
  const neverAllowed = maxTtl === null;

  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<Set<ApiKeyScope>>(() => new Set(["read"]));
  const [expiry, setExpiry] = useState<string>("90");
  const [formError, setFormError] = useState<string | null>(null);
  const [confirmId, setConfirmId] = useState<string | null>(null);
  // The plaintext key lives only in this component state, never in a cache.
  const [revealed, setRevealed] = useState<{ name: string; key: string } | null>(null);

  const effectiveExpiry =
    expiry === NEVER ? (neverAllowed ? NEVER : String(expiryOptions.at(-1) ?? 30))
      : expiryOptions.some((d) => String(d) === expiry) ? expiry
      : String(expiryOptions.at(-1) ?? 30);

  const create = useMutation({
    // Returns only the error (if any): the created key must not end up in
    // the mutation cache, so the secret is handed straight to local state.
    mutationFn: async () => {
      const r = await apiKeysApi.create({
        name,
        scopes: API_KEY_SCOPES.filter((s) => scopes.has(s)),
        expiresInDays: effectiveExpiry === NEVER ? null : Number(effectiveExpiry),
        userId: targetUserId,
      });
      if ("error" in r) return { error: r.error.code };
      setRevealed({ name: r.ok.name, key: r.ok.key });
      return { error: null };
    },
    onMutate: () => setFormError(null),
    onSuccess: async (r) => {
      if (r.error) {
        setFormError(apiKeyErrorMessage(r.error));
        return;
      }
      setName("");
      setScopes(new Set(["read"]));
      await qc.invalidateQueries({ queryKey });
    },
    onError: () => setFormError(apiKeyErrorMessage("")),
  });

  const revoke = useMutation({
    mutationFn: (id: string) => apiKeysApi.revoke(id, targetUserId),
    onSuccess: async (r) => {
      setConfirmId(null);
      if ("error" in r) notify("error", apiKeyErrorMessage(r.error.code));
      await qc.invalidateQueries({ queryKey });
    },
  });

  function toggleScope(s: ApiKeyScope, on: boolean) {
    setScopes((prev) => {
      const next = new Set(prev);
      if (on) next.add(s); else next.delete(s);
      return next;
    });
  }

  const list: ApiKey[] = keys.data && "ok" in keys.data ? keys.data.ok : [];
  const listError = keys.data && "error" in keys.data ? apiKeyErrorMessage(keys.data.error.code) : null;
  const canSubmit = name.trim().length > 0 && scopes.size > 0 && !create.isPending;

  return (
    <div>
      <p className="text-[13px] text-fg-muted m-0 mb-3">
        {targetUserId
          ? "Keys this bot uses to call the API."
          : "Keys let scripts and agents call the API as you. Each key is limited to the scopes you pick."}
      </p>

      {listError && <p role="alert" className="text-destructive text-[13px] m-0 mb-3">{listError}</p>}

      {list.length > 0 && (
        <table className="w-full text-sm mb-4">
          <thead>
            <tr className="text-left text-[12px] text-fg-muted">
              <th className="font-medium py-1">Name</th>
              <th className="font-medium py-1">Scopes</th>
              <th className="font-medium py-1">Created</th>
              <th className="font-medium py-1">Expires</th>
              <th className="font-medium py-1">Last used</th>
              <th className="py-1"><span className="sr-only">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {list.map((k) => (
              <tr key={k.id} data-testid="api-key-row" className="border-t border-border">
                <td className="py-1.5 text-fg">{k.name}</td>
                <td className="py-1.5">
                  <span className="flex flex-wrap gap-1">
                    {k.scopes.map((s) => (
                      <span key={s} className="px-1.5 py-0.5 rounded bg-muted text-fg text-[11px]">{s}</span>
                    ))}
                  </span>
                </td>
                <td className="py-1.5 text-fg-muted">{fmtDate(k.created_at)}</td>
                <td className="py-1.5 text-fg-muted">{k.expires_at ? fmtDate(k.expires_at) : "Never"}</td>
                <td className="py-1.5 text-fg-muted">{k.last_used_at ? fmtDate(k.last_used_at) : "Never"}</td>
                <td className="py-1.5 text-right">
                  {k.revoked_at ? (
                    <span className="text-[12px] text-fg-muted">Revoked</span>
                  ) : confirmId === k.id ? (
                    <span className="inline-flex gap-1">
                      <button
                        type="button"
                        data-testid="api-key-revoke-confirm"
                        disabled={revoke.isPending}
                        onClick={() => revoke.mutate(k.id)}
                        className="h-7 px-2 rounded bg-destructive text-white text-[12px] font-medium disabled:opacity-50"
                      >
                        Confirm revoke
                      </button>
                      <button
                        type="button"
                        onClick={() => setConfirmId(null)}
                        className="h-7 px-2 rounded border border-border text-fg text-[12px]"
                      >
                        Cancel
                      </button>
                    </span>
                  ) : (
                    <button
                      type="button"
                      data-testid="api-key-revoke"
                      onClick={() => setConfirmId(k.id)}
                      className="h-7 px-2 rounded border border-border text-fg text-[12px] hover:bg-muted"
                    >
                      Revoke
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <form
        data-testid="api-key-create"
        onSubmit={(e) => { e.preventDefault(); if (canSubmit) create.mutate(); }}
        className="grid gap-3 max-w-sm"
      >
        <input
          data-testid="api-key-name"
          aria-label="Key name"
          placeholder="Key name"
          value={name}
          maxLength={100}
          onChange={(e) => setName(e.target.value)}
          required
          className="h-9 px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm"
        />
        <fieldset className="flex flex-wrap gap-3 border-0 p-0 m-0">
          <legend className="text-[13px] text-fg-muted mb-1">Scopes</legend>
          {API_KEY_SCOPES.map((s) => (
            <label key={s} className="inline-flex items-center gap-1.5 text-sm text-fg">
              <input
                type="checkbox"
                data-testid={`api-key-scope-${s}`}
                checked={scopes.has(s)}
                onChange={(e) => toggleScope(s, e.target.checked)}
              />
              {SCOPE_LABEL[s]}
            </label>
          ))}
        </fieldset>
        <label className="grid gap-1 text-[13px] text-fg-muted">
          Expires after
          <select
            data-testid="api-key-expiry"
            value={effectiveExpiry}
            onChange={(e) => setExpiry(e.target.value)}
            className="h-9 px-2 rounded border border-border bg-bg text-fg text-sm"
          >
            {expiryOptions.map((d) => <option key={d} value={String(d)}>{d} days</option>)}
            {neverAllowed && <option value={NEVER}>Never</option>}
          </select>
        </label>
        {formError && <p data-testid="api-key-error" role="alert" className="text-destructive text-[13px] m-0">{formError}</p>}
        <button
          data-testid="api-key-submit"
          type="submit"
          disabled={!canSubmit}
          className="justify-self-start h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {create.isPending ? "Creating…" : "Create key"}
        </button>
      </form>

      {revealed && <RevealDialog name={revealed.name} secret={revealed.key} onClose={() => setRevealed(null)} />}
    </div>
  );
}

function RevealDialog({ name, secret, onClose }: { name: string; secret: string; onClose: () => void }) {
  const notify = useUi((s) => s.notify);
  const titleId = useId();
  return (
    <div className="fixed inset-0 z-50 bg-black/40 backdrop-blur-sm flex items-center justify-center p-4">
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid="api-key-reveal"
        className="w-[440px] max-w-full grid gap-3 p-4 rounded-lg bg-surface border border-border shadow-xl"
        onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); onClose(); } }}
      >
        <h2 id={titleId} className="m-0 text-[15px] font-semibold text-fg">Key “{name}” created</h2>
        <p role="alert" className="m-0 text-[13px] text-fg">
          Copy it now: this key is shown once and can't be retrieved again.
        </p>
        <input
          readOnly
          aria-label="API key"
          value={secret}
          onFocus={(e) => e.target.select()}
          className="h-9 px-3 rounded border border-border bg-bg text-fg font-mono text-[12px]"
        />
        <div className="flex justify-end gap-2">
          <button
            type="button"
            data-testid="api-key-copy"
            onClick={() => {
              void navigator.clipboard.writeText(secret).then(
                () => notify("info", "Copied key"),
                () => notify("error", "Couldn't copy, select the key and copy it manually"),
              );
            }}
            className="h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted"
          >
            Copy
          </button>
          <button
            type="button"
            autoFocus
            onClick={onClose}
            className="h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90"
          >
            Done
          </button>
        </div>
      </div>
    </div>
  );
}
