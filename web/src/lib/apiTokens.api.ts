import * as v from "valibot";

import { apiFetch } from "./api";
import {
  ApiToken,
  CreatedApiToken,
  type ApiTokenScope,
  parse,
} from "./validators";

/** Personal API tokens (bearer auth for bots/scripts). Session-authed. */
export const apiTokensApi = {
  async list() {
    const r = await apiFetch<unknown>("/api/tokens");
    if ("error" in r) return r;
    return { ok: parse(v.array(ApiToken), r.ok) };
  },
  async create(name: string, scopes: ApiTokenScope[]) {
    const r = await apiFetch<unknown>("/api/tokens", {
      method: "POST",
      body: { name, scopes },
    });
    if ("error" in r) return r;
    return { ok: parse(CreatedApiToken, r.ok) };
  },
  revoke(id: string) {
    return apiFetch<void>(`/api/tokens/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
};
