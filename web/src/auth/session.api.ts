import { apiFetch } from "../lib/api";
import {
  type AuthConfig,
  AuthConfig as AuthConfigSchema,
  type Session,
  parse,
  Session as SessionSchema,
} from "../lib/validators";

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
