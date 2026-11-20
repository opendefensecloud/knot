import * as v from "valibot";

import { apiFetch } from "../../lib/api";
import { parse } from "../../lib/validators";

export const API_KEY_SCOPES = ["read", "create", "update", "delete"] as const;
export type ApiKeyScope = (typeof API_KEY_SCOPES)[number];

const apiKeySchema = v.object({
  id: v.string(),
  name: v.string(),
  scopes: v.array(v.string()),
  created_by: v.string(),
  created_at: v.string(),
  expires_at: v.nullable(v.string()),
  last_used_at: v.nullable(v.string()),
  revoked_at: v.nullable(v.string()),
  root_doc_id: v.fallback(v.nullable(v.string()), null),
  /** null when the key has no root or the viewer can't read it. */
  root_title: v.fallback(v.nullable(v.string()), null),
});
export type ApiKey = v.InferOutput<typeof apiKeySchema>;

const createdApiKeySchema = v.object({
  id: v.string(),
  key: v.string(),
  name: v.string(),
  scopes: v.array(v.string()),
  expires_at: v.nullable(v.string()),
  root_doc_id: v.fallback(v.nullable(v.string()), null),
});
/** Carries the plaintext key. Never put this into the query cache or a log. */
export type CreatedApiKey = v.InferOutput<typeof createdApiKeySchema>;

export type CreateApiKeyInput = {
  name: string;
  scopes: ApiKeyScope[];
  /** null = never expires (only when neither the key's user nor its minter is capped). */
  expiresInDays: number | null;
  userId?: string;
  /** Limit the key to this page and its subpages (S10). */
  rootDocId?: string | null;
};

/** Prefix of every key list's query key: invalidating it refreshes them all. */
export const API_KEYS_ROOT_KEY = ["api-keys"] as const;
export const apiKeysQueryKey = (userId?: string) => [...API_KEYS_ROOT_KEY, userId ?? "self"] as const;

function userQuery(userId?: string) {
  return userId ? `?user_id=${encodeURIComponent(userId)}` : "";
}

export const apiKeysApi = {
  async list(userId?: string) {
    const r = await apiFetch<unknown>(`/api/auth/api-keys${userQuery(userId)}`);
    if ("error" in r) return r;
    return { ok: parse(v.array(apiKeySchema), r.ok) };
  },
  async create(input: CreateApiKeyInput) {
    const body: Record<string, unknown> = {
      name: input.name.trim(),
      scopes: input.scopes,
      expires_in_days: input.expiresInDays,
    };
    if (input.userId) body.user_id = input.userId;
    if (input.rootDocId) body.root_doc_id = input.rootDocId;
    const r = await apiFetch<unknown>("/api/auth/api-keys", { method: "POST", body });
    if ("error" in r) return r;
    return { ok: parse(createdApiKeySchema, r.ok) };
  },
  revoke(id: string, userId?: string) {
    return apiFetch<void>(`/api/auth/api-keys/${encodeURIComponent(id)}${userQuery(userId)}`, {
      method: "DELETE",
    });
  },
};
