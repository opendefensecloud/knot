import { type ApiError, type ApiResult, apiFetch } from "../../lib/api";
import { readCookie } from "../../lib/csrf";
import { DiagramList, parse } from "../../lib/validators";
import { type Fallbacks, parseFallbacks } from "./exportFile";
import type { DiagramLimits, DiagramRef } from "./renderDiagrams";

export type { Fallbacks };

function failure(code: string, message: string, status: number, details = {}): { error: ApiError } {
  return { error: { code, message, details, status } };
}

export const exportApi = {
  async diagrams(docId: string): Promise<ApiResult<{ limits: DiagramLimits; diagrams: DiagramRef[] }>> {
    const r = await apiFetch<unknown>(`/api/docs/${encodeURIComponent(docId)}/export/diagrams`);
    if ("error" in r) return r;
    return { ok: parse(DiagramList, r.ok) };
  },

  /** apiFetch cannot return a blob or response headers, so this is a plain fetch. */
  async file(
    docId: string,
    format: "docx" | "odt",
    parts: Map<string, Blob>,
  ): Promise<ApiResult<{ blob: Blob; fallbacks: Fallbacks }>> {
    const fd = new FormData();
    for (const [key, blob] of parts) fd.append(key, blob, `${key}.png`);
    const headers: Record<string, string> = {};
    const csrf = readCookie("csrf");
    if (csrf) headers["X-CSRF-Token"] = csrf;
    let res: Response;
    try {
      res = await fetch(`/api/docs/${encodeURIComponent(docId)}/export/${format}`, {
        method: "POST",
        credentials: "include",
        headers,
        body: fd,
      });
    } catch {
      return failure("network", "Network error", 0);
    }
    if (!res.ok) {
      const text = await res.text().catch(() => "");
      try {
        const env = JSON.parse(text) as { error?: Partial<ApiError> };
        return failure(
          env.error?.code ?? "http_error",
          env.error?.message ?? `HTTP ${res.status}`,
          res.status,
          env.error?.details ?? {},
        );
      } catch {
        return failure("http_error", `HTTP ${res.status}`, res.status);
      }
    }
    let blob: Blob;
    try {
      blob = await res.blob();
    } catch {
      return failure("network", "Network error", 0);
    }
    return { ok: { blob, fallbacks: parseFallbacks(res.headers.get("X-Knot-Export-Fallbacks")) } };
  },
};
