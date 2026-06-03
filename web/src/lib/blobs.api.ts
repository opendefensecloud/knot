import { type ApiError, type ApiResult } from "./api";
import { readCookie } from "./csrf";

export type BlobResponse = {
  id: string;
  doc_id: string;
  content_type: string;
  byte_size: number;
  url: string;
  original_name: string | null;
};

export const blobsApi = {
  async upload(docId: string, file: File): Promise<ApiResult<BlobResponse>> {
    const fd = new FormData();
    fd.append("file", file, file.name);
    const headers: Record<string, string> = {};
    const csrf = readCookie("csrf");
    if (csrf) headers["X-CSRF-Token"] = csrf;
    let res: Response;
    try {
      res = await fetch(`/api/docs/${encodeURIComponent(docId)}/blobs`, {
        method: "POST",
        credentials: "include",
        headers,
        body: fd,
      });
    } catch {
      return { error: { code: "network", message: "Network error", details: {}, status: 0 } };
    }
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
        return { error: { code: "http_error", message: `HTTP ${res.status}`, details: {}, status: res.status } };
      }
    }
    return { ok: JSON.parse(text) as BlobResponse };
  },
};
