import * as v from "valibot";

import { apiFetch } from "../lib/api";
import {
  type AuthConfig,
  AuthConfig as AuthConfigSchema,
  parse,
  Session as BaseSessionSchema,
} from "../lib/validators";

// The session also says how the user signed in and the longest API-key
// lifetime the server allows them (null = unlimited). Fallbacks keep older
// servers and fixtures parsing; the server enforces the cap regardless.
const SessionSchema = v.object({
  ...BaseSessionSchema.entries,
  auth_kind: v.fallback(v.picklist(["local", "oidc", "bot"]), "local"),
  max_api_key_ttl_days: v.fallback(v.nullable(v.number()), null),
});
export type Session = v.InferOutput<typeof SessionSchema>;

export const authApi = {
  async session() {
    const r = await apiFetch<unknown>("/auth/session");
    if ("error" in r) return r;
    return { ok: parse(SessionSchema, r.ok) satisfies Session };
  },
  async config() {
    const r = await apiFetch<unknown>("/auth/config");
    if ("error" in r) return r;
    return { ok: parse(AuthConfigSchema, r.ok) satisfies AuthConfig };
  },
  async login(email: string, password: string) {
    return apiFetch<void>("/auth/login", {
      method: "POST",
      body: { email, password },
    });
  },
  async logout() {
    return apiFetch<void>("/auth/logout", { method: "POST" });
  },
  async setup(email: string, password: string, display_name: string) {
    return apiFetch<{ user_id: string; workspace_id: string }>("/auth/setup", {
      method: "POST",
      body: { email, password, display_name },
    });
  },
  async changePassword(current: string, next: string) {
    return apiFetch<void>("/auth/password", {
      method: "POST",
      body: { current, new: next },
    });
  },
};
