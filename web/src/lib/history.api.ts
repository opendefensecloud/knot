import { apiFetch, type ApiError, type ApiResult } from "./api";
import { readCookie } from "./csrf";

export type SnapshotMeta = {
  snapshot_seq: number;
  byte_size: number;
  created_at: string;
};

export const historyApi = {
  async list(docId: string): Promise<ApiResult<SnapshotMeta[]>> {
    return apiFetch<SnapshotMeta[]>(`/api/docs/${encodeURIComponent(docId)}/history`);
  },

  /** Server returns text/markdown, not JSON — bypass apiFetch's JSON path. */
  async preview(docId: string, seq: number): Promise<ApiResult<string>> {
    const res = await fetch(
      `/api/docs/${encodeURIComponent(docId)}/history/${seq}/markdown`,
      { credentials: "include" },
    );
    const text = await res.text();
    if (!res.ok) {
      try {
        const env = JSON.parse(text) as { error?: Partial<ApiError> };
        return {
          error: {
            code: env.error?.code ?? "http_error",
            message: env.error?.message ?? `HTTP ${res.status}`,
            details: env.error?.details ?? {},
            status: res.status,
          },
        };
      } catch {
        return {
          error: {
            code: "http_error",
            message: `HTTP ${res.status}`,
            details: {},
            status: res.status,
          },
        };
      }
    }
    return { ok: text };
  },

  /** Live markdown export from the active room (no snapshot lookup). */
  async exportMarkdown(docId: string): Promise<ApiResult<string>> {
    const res = await fetch(
      `/api/docs/${encodeURIComponent(docId)}/markdown`,
      { credentials: "include" },
    );
    const text = await res.text();
    if (!res.ok) {
      try {
        const env = JSON.parse(text) as { error?: Partial<ApiError> };
        return {
          error: {
            code: env.error?.code ?? "http_error",
            message: env.error?.message ?? `HTTP ${res.status}`,
            details: env.error?.details ?? {},
            status: res.status,
          },
        };
      } catch {
        return {
          error: { code: "http_error", message: `HTTP ${res.status}`, details: {}, status: res.status },
        };
      }
    }
    return { ok: text };
  },

  async restore(docId: string, seq: number): Promise<ApiResult<void>> {
    const headers: Record<string, string> = {};
    const csrf = readCookie("csrf");
    if (csrf) headers["X-CSRF-Token"] = csrf;
    const res = await fetch(
      `/api/docs/${encodeURIComponent(docId)}/history/${seq}/restore`,
      { method: "POST", credentials: "include", headers },
    );
    if (!res.ok) {
      const text = await res.text();
      try {
        const env = JSON.parse(text) as { error?: Partial<ApiError> };
        return {
          error: {
            code: env.error?.code ?? "http_error",
            message: env.error?.message ?? `HTTP ${res.status}`,
            details: env.error?.details ?? {},
            status: res.status,
          },
        };
      } catch {
        return {
          error: { code: "http_error", message: `HTTP ${res.status}`, details: {}, status: res.status },
        };
      }
    }
    return { ok: undefined };
  },
};
