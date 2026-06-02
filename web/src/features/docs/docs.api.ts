import * as v from "valibot";

import { apiFetch } from "../../lib/api";
import { Doc, DocWithRole, parse } from "../../lib/validators";

export type DocCreate = { title?: string; parent_id?: string; after_id?: string };
export type DocPatch = { title?: string; icon?: string };
export type DocMove = { parent_id?: string | null; after_id?: string; before_id?: string };

export const docsApi = {
  async list() {
    const r = await apiFetch<unknown>("/api/docs");
    if ("error" in r) return r;
    return { ok: parse(v.array(Doc), r.ok) };
  },
  async get(id: string) {
    const r = await apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}`);
    if ("error" in r) return r;
    return { ok: parse(DocWithRole, r.ok) };
  },
  create(body: DocCreate) {
    return apiFetch<unknown>("/api/docs", { method: "POST", body });
  },
  patch(id: string, body: DocPatch) {
    return apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}`, { method: "PATCH", body });
  },
  move(id: string, body: DocMove) {
    return apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}/move`, {
      method: "POST",
      body,
    });
  },
  archive(id: string) {
    return apiFetch<void>(`/api/docs/${encodeURIComponent(id)}`, { method: "DELETE" });
  },
  restore(id: string) {
    return apiFetch<void>(`/api/docs/${encodeURIComponent(id)}/restore`, { method: "POST" });
  },
};
