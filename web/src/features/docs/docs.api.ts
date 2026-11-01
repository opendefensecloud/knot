import * as v from "valibot";

import { apiFetch, type ApiResult } from "../../lib/api";
import {
  ArchiveEntry,
  Doc,
  DocAccessInfo,
  DocContributors,
  DocWithRole,
  parse,
} from "../../lib/validators";

export type DocCreate = { title?: string; parent_id?: string; after_id?: string };
export type DocPatch = { title?: string; icon?: string };
export type DocMove = { parent_id?: string | null; after_id?: string; before_id?: string };
export type DocDuplicate = { include_subpages: boolean };

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
  /** Creator plus everyone who changed the page content. Readable by any
   *  role that can open the doc. */
  async contributors(id: string) {
    const r = await apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}/contributors`);
    if ("error" in r) return r;
    return { ok: parse(DocContributors, r.ok) };
  },
  /** Who can see the doc and through which grant (any reader). */
  async access(id: string): Promise<ApiResult<DocAccessInfo>> {
    const r = await apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}/access`);
    if ("error" in r) return r;
    return { ok: parse(DocAccessInfo, r.ok) };
  },
  /** Make the doc a restriction boundary, or stop it being one (owners).
   *  Restricting also makes the caller an owner of the doc and revokes the
   *  public links in its subtree. */
  async setRestricted(id: string, restricted: boolean): Promise<ApiResult<Doc>> {
    const r = await apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}/restricted`, {
      method: "PUT",
      body: { restricted },
    });
    if ("error" in r) return r;
    return { ok: parse(Doc, r.ok) };
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
  /** Copy a doc right after it, with its subpages when asked (editors and
   *  owners). 413 `doc.duplicate_too_large` above the server's caps, 503
   *  `doc.duplicate_busy` while it is making as many copies as it will. */
  async duplicate(id: string, body: DocDuplicate): Promise<ApiResult<Doc>> {
    const r = await apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}/duplicate`, {
      method: "POST",
      body,
    });
    if ("error" in r) return r;
    return { ok: parse(Doc, r.ok) };
  },
  /** Move a doc and its subpages to the archive (editors and owners). */
  archive(id: string) {
    return apiFetch<void>(`/api/docs/${encodeURIComponent(id)}`, { method: "DELETE" });
  },
  restore(id: string) {
    return apiFetch<void>(`/api/docs/${encodeURIComponent(id)}/restore`, { method: "POST" });
  },
  /** Delete an archived doc and its subpages for good (owners). */
  purge(id: string) {
    return apiFetch<void>(`/api/docs/${encodeURIComponent(id)}/purge`, { method: "POST" });
  },
  /** Docs moved to the archive, newest first (editors and owners). */
  async listArchive() {
    const r = await apiFetch<unknown>("/api/workspace/archive");
    if ("error" in r) return r;
    return { ok: parse(v.array(ArchiveEntry), r.ok) };
  },
  setTemplate(id: string, isTemplate: boolean) {
    return apiFetch<unknown>(`/api/docs/${encodeURIComponent(id)}/template`, {
      method: "POST",
      body: { is_template: isTemplate },
    });
  },
  async listTemplates() {
    const r = await apiFetch<unknown>("/api/workspace/templates");
    if ("error" in r) return r;
    return { ok: parse(v.array(Doc), r.ok) };
  },
  createFromTemplate(templateId: string, body: { title?: string; parent_id?: string }) {
    return apiFetch<unknown>(`/api/docs/from-template/${encodeURIComponent(templateId)}`, {
      method: "POST",
      body,
    });
  },
  /**
   * Import Markdown into an existing doc. `replace` swaps the body;
   * `append` is the server's default and merges into whatever is already
   * there. Returns 204 with no body, so `ok` is `undefined`.
   */
  importMarkdown(id: string, markdown: string, mode: "replace" | "append" = "replace") {
    return apiFetch<void>(`/api/docs/${encodeURIComponent(id)}/markdown?mode=${mode}`, {
      method: "POST",
      body: markdown,
      contentType: "text/markdown; charset=utf-8",
    });
  },
};
