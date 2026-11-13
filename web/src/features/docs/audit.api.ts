/**
 * GET /api/docs/{id}/audit: a page's audit log for its editors and owners,
 * newest first, a page at a time. The first page (no cursor) carries the
 * export stats; later pages have `stats: null`.
 */
import { apiFetch, type ApiResult } from "../../lib/api";
import { DocAuditPage, parse } from "../../lib/validators";

export type AuditCategory = "all" | "exports" | "access" | "changes";

export type AuditListOptions = {
  category?: AuditCategory;
  /**
   * The (opaque) id of the last event already shown, the previous page's
   * `next_cursor`; the page holds older ones.
   */
  cursor?: string;
  limit?: number;
};

export const auditApi = {
  async list(docId: string, options: AuditListOptions = {}): Promise<ApiResult<DocAuditPage>> {
    const queryParams = new URLSearchParams();
    if (options.category !== undefined) queryParams.set("category", options.category);
    if (options.cursor !== undefined) queryParams.set("cursor", options.cursor);
    if (options.limit !== undefined) queryParams.set("limit", String(options.limit));
    const queryString = queryParams.toString();
    const path = `/api/docs/${encodeURIComponent(docId)}/audit${queryString ? `?${queryString}` : ""}`;
    const response = await apiFetch<unknown>(path);
    if ("error" in response) return response;
    return { ok: parse(DocAuditPage, response.ok) };
  },
};
