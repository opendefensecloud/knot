import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, Copy, KeyRound } from "lucide-react";
import { useState } from "react";

import { apiTokensApi } from "../../lib/apiTokens.api";
import { relTime } from "../../lib/relTime";
import {
  API_TOKEN_SCOPES,
  type ApiToken,
  type ApiTokenScope,
} from "../../lib/validators";
import { useUi } from "../../stores/ui";

/** What each scope allows, in the words a bot author needs. */
const SCOPE_HINTS: Record<ApiTokenScope, string> = {
  read: "Look at pages, search, history, exports",
  create: "Create pages, duplicate pages, use templates",
  write: "Edit pages and markdown, move, restore, comments, tasks",
  delete: "Delete pages, purge from the archive",
};

/** Checked by default: what a documentation bot normally needs. */
const DEFAULT_SCOPES: ApiTokenScope[] = ["read", "create", "write"];

export function ApiTokensPanel() {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);

  const tokens = useQuery({
    queryKey: ["api-tokens"],
    queryFn: () => apiTokensApi.list(),
  });

  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<ApiTokenScope[]>(DEFAULT_SCOPES);
  const [created, setCreated] = useState<{
    name: string;
    secret: string;
  } | null>(null);
  const [copied, setCopied] = useState(false);

  const toggle = (scope: ApiTokenScope) =>
    setScopes((s) =>
      s.includes(scope) ? s.filter((x) => x !== scope) : [...s, scope],
    );

  const create = useMutation({
    mutationFn: () =>
      apiTokensApi.create(
        name.trim(),
        API_TOKEN_SCOPES.filter((s) => scopes.includes(s)),
      ),
    onSuccess: async (r) => {
      if ("error" in r) {
        notify(
          "error",
          r.error.code === "token.scopes"
            ? "Pick at least one permission."
            : r.error.code === "token.name"
              ? "Give the token a name of up to 100 characters."
              : "Couldn't create the token.",
        );
        return;
      }
      setCreated({ name: r.ok.name, secret: r.ok.token_secret });
      setCopied(false);
      setName("");
      await qc.invalidateQueries({ queryKey: ["api-tokens"] });
    },
  });

  const revoke = useMutation({
    mutationFn: (id: string) => apiTokensApi.revoke(id),
    onSuccess: async (r) => {
      if ("error" in r) {
        notify("error", "Couldn't revoke the token.");
        return;
      }
      notify("info", "Token revoked");
      await qc.invalidateQueries({ queryKey: ["api-tokens"] });
    },
  });

  function onRevoke(t: ApiToken) {
    if (
      !window.confirm(
        `Revoke “${t.name}”? Anything still using it stops working immediately.`,
      )
    )
      return;
    revoke.mutate(t.id);
  }

  async function copySecret() {
    if (!created) return;
    await navigator.clipboard.writeText(created.secret);
    setCopied(true);
    notify("info", "Copied!");
  }

  const canCreate =
    name.trim().length > 0 && scopes.length > 0 && !create.isPending;
  const list = tokens.data && "ok" in tokens.data ? tokens.data.ok : null;

  return (
    <section
      data-testid="api-tokens"
      className="bg-surface border border-border rounded-lg px-5 py-4"
    >
      <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mb-2 flex items-center gap-2">
        <KeyRound size={14} aria-hidden /> API tokens
      </h2>
      <p className="text-[13px] text-fg-muted m-0 mb-3">
        For bots and scripts. Send{" "}
        <code className="text-fg">Authorization: Bearer …</code> instead of a
        session cookie. A token acts as you — your workspace role and page
        access still apply.
      </p>

      {/* --- new token form --- */}
      <div className="space-y-3">
        <div>
          <label htmlFor="token-name" className="block text-sm text-fg mb-1">
            Name
          </label>
          <input
            id="token-name"
            data-testid="api-token-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="docs-bot"
            maxLength={100}
            className="w-full h-9 px-3 rounded border border-border bg-surface text-fg text-sm"
          />
        </div>
        <fieldset className="m-0 p-0 border-0">
          <legend className="text-sm text-fg mb-1">Permissions</legend>
          <div className="grid gap-1.5">
            {API_TOKEN_SCOPES.map((scope) => (
              <label
                key={scope}
                className="flex items-start gap-2 text-sm text-fg cursor-pointer"
              >
                <input
                  type="checkbox"
                  data-testid={`api-token-scope-${scope}`}
                  checked={scopes.includes(scope)}
                  onChange={() => toggle(scope)}
                  className="mt-0.5 h-4 w-4 shrink-0 accent-accent"
                />
                <span>
                  {scope}
                  <span className="block text-xs text-fg-muted">
                    {SCOPE_HINTS[scope]}
                  </span>
                </span>
              </label>
            ))}
          </div>
        </fieldset>
        <button
          type="button"
          data-testid="api-token-create"
          disabled={!canCreate}
          onClick={() => create.mutate()}
          className="inline-flex items-center h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
        >
          Create token
        </button>
        {scopes.length === 0 && (
          <p className="m-0 text-xs text-fg-muted">
            Pick at least one permission.
          </p>
        )}
      </div>

      {/* --- the secret, once --- */}
      {created && (
        <div
          data-testid="api-token-secret"
          className="mt-4 rounded border border-accent/40 bg-muted px-3 py-3 space-y-2"
        >
          <p className="m-0 text-sm text-fg">
            Token “{created.name}” created. Copy it now — it is shown only once.
          </p>
          <div className="flex items-center gap-2">
            <code className="flex-1 min-w-0 truncate rounded bg-surface border border-border px-2 py-1.5 text-xs text-fg">
              {created.secret}
            </code>
            <button
              type="button"
              data-testid="api-token-copy"
              onClick={() => void copySecret()}
              className="inline-flex items-center gap-1.5 h-8 px-2.5 rounded border border-border bg-surface text-fg text-xs font-medium hover:bg-muted transition-colors"
            >
              {copied ? (
                <Check size={13} aria-hidden />
              ) : (
                <Copy size={13} aria-hidden />
              )}
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          <p className="m-0 text-xs text-fg-muted">
            Try it:{" "}
            <code className="text-fg">
              curl -H &quot;Authorization: Bearer &lt;token&gt;&quot; /api/docs
            </code>
          </p>
          <button
            type="button"
            onClick={() => setCreated(null)}
            className="text-xs text-fg-muted underline underline-offset-2 hover:text-fg"
          >
            Done
          </button>
        </div>
      )}

      {/* --- existing tokens --- */}
      <div className="mt-4 pt-4 border-t border-border">
        {list === null ? (
          <p className="m-0 text-[13px] text-fg-muted">Loading…</p>
        ) : list.length === 0 ? (
          <p
            data-testid="api-tokens-empty"
            className="m-0 text-[13px] text-fg-muted"
          >
            No tokens yet.
          </p>
        ) : (
          <ul className="m-0 p-0 list-none space-y-2">
            {list.map((t) => {
              const revoked = t.revoked_at !== null;
              return (
                <li
                  key={t.id}
                  data-testid={`api-token-${t.id}`}
                  className="flex items-start justify-between gap-3"
                >
                  <div className="min-w-0">
                    <p className="m-0 text-sm text-fg">
                      {t.name}
                      {revoked && (
                        <span className="ml-2 text-xs text-fg-muted">
                          revoked
                        </span>
                      )}
                    </p>
                    <p className="m-0 text-xs text-fg-muted">
                      {t.scopes.join(", ")} · created {relTime(t.created_at)} ·{" "}
                      {t.last_used_at
                        ? `last used ${relTime(t.last_used_at)}`
                        : "never used"}
                    </p>
                  </div>
                  {!revoked && (
                    <button
                      type="button"
                      onClick={() => onRevoke(t)}
                      className="shrink-0 h-8 px-2.5 rounded border border-border bg-surface text-fg text-xs font-medium hover:bg-muted transition-colors"
                    >
                      Revoke
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </section>
  );
}
