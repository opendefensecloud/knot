import { apiFetch, type ApiResult } from "./api";

export type ShortLink = { code: string; url: string };

export const shortLinksApi = {
  /** The page's short link, created on first use. Live pages only. */
  async getOrCreate(docId: string): Promise<ApiResult<ShortLink>> {
    return apiFetch<ShortLink>(`/api/docs/${encodeURIComponent(docId)}/short-link`, {
      method: "POST",
    });
  },
  /** The page's short link if it has one (404 `short_link.none` otherwise). */
  async get(docId: string): Promise<ApiResult<ShortLink>> {
    return apiFetch<ShortLink>(`/api/docs/${encodeURIComponent(docId)}/short-link`);
  },
  /** The page a code names, when the caller may read it. */
  async resolve(code: string): Promise<ApiResult<{ doc_id: string }>> {
    return apiFetch<{ doc_id: string }>(`/api/short-links/${encodeURIComponent(code)}`);
  },
};
