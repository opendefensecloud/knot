import * as v from "valibot";

import { apiFetch } from "../../lib/api";
import { Member, Workspace, parse } from "../../lib/validators";

export const workspaceApi = {
  async get() {
    const r = await apiFetch<unknown>("/api/workspace");
    if ("error" in r) return r;
    return { ok: parse(Workspace, r.ok) };
  },
  async listMembers() {
    const r = await apiFetch<unknown>("/api/workspace/members");
    if ("error" in r) return r;
    return { ok: parse(v.array(Member), r.ok) };
  },
  invite(
    email: string,
    role: "owner" | "editor" | "viewer",
    password?: string,
    displayName?: string,
    kind?: "human" | "bot",
  ) {
    const body: Record<string, unknown> = { email, role };
    if (password) body.password = password;
    const name = displayName?.trim();
    if (name) body.display_name = name;
    // Human is the server default; only a bot invite carries the field.
    if (kind === "bot") body.kind = "bot";
    return apiFetch<void>("/api/workspace/members", { method: "POST", body });
  },
  setRole(userId: string, role: "owner" | "editor" | "viewer") {
    return apiFetch<void>(`/api/workspace/members/${encodeURIComponent(userId)}`, {
      method: "PATCH",
      body: { role },
    });
  },
  remove(userId: string) {
    return apiFetch<void>(`/api/workspace/members/${encodeURIComponent(userId)}`, {
      method: "DELETE",
    });
  },
};
