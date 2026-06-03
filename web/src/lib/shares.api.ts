import { apiFetch, type ApiResult } from "./api";

export type Share = {
  id: string;
  token: string;
  url: string;
  expires_at: string | null;
  created_at: string;
};

export const sharesApi = {
  async list(docId: string): Promise<ApiResult<Share[]>> {
    return apiFetch<Share[]>(`/api/docs/${encodeURIComponent(docId)}/shares`);
  },
  async create(docId: string, expiresAt: string | null): Promise<ApiResult<Share>> {
    return apiFetch<Share>(`/api/docs/${encodeURIComponent(docId)}/shares`, {
      method: "POST",
      body: { expires_at: expiresAt },
    });
  },
  async revoke(docId: string, shareId: string): Promise<ApiResult<void>> {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(docId)}/shares/${encodeURIComponent(shareId)}`,
      { method: "DELETE" },
    );
  },
};
