import * as v from "valibot";

import { apiFetch } from "../../lib/api";
import { parse } from "../../lib/validators";

export const API_KEY_SCOPES = ["read", "create", "update", "delete"] as const;
export type ApiKeyScope = (typeof API_KEY_SCOPES)[number];

const apiKeySchema = v.object({
  id: v.string(),
  name: v.string(),
  scopes: v.array(v.string()),
  created_at: v.string(),
  expires_at: v.nullable(v.string()),
  last_used_at: v.nullable(v.string()),
  revoked_at: v.nullable(v.string()),
});
export type ApiKey = v.InferOutput<typeof apiKeySchema>;

const createdApiKeySchema = v.object({
  id: v.string(),
  key: v.string(),
  name: v.string(),
  scopes: v.array(v.string()),
  expires_at: v.nullable(v.string()),
});
/** Carries the plaintext key. Never put this into the query cache or a log. */
export type CreatedApiKey = v.InferOutput<typeof createdApiKeySchema>;

export type CreateApiKeyInput = {
  name: string;
  scopes: ApiKeyScope[];
  /** null = never expires (bots / unrestricted users only). */
  expiresInDays: number | null;
  userId?: string;
};

export const apiKeysQueryKey = (userId?: string) => ["api-keys", userId ?? "self"] as const;

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
