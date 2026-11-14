import { apiFetch, type ApiError, type ApiResult } from "./api";
import { HistoryPage, HistoryView, parse } from "./validators";

function historyPath(docId: string): string {
  return `/api/docs/${encodeURIComponent(docId)}/history`;
}

export const historyApi = {
  /** One page of the page's timeline; `cursor` is the previous page's
   *  `next_cursor`, null for the first page. */
  async entries(docId: string, cursor: string | null): Promise<ApiResult<HistoryPage>> {
    const query = cursor === null ? "" : `?cursor=${encodeURIComponent(cursor)}`;
    return apiFetch<HistoryPage>(`${historyPath(docId)}/entries${query}`, {
      parser: (data) => parse(HistoryPage, data),
    });
  },

  /** The page as it was right after the entry, with the entry's own changes
   *  marked when `changes` (and the server could). `signal` aborts the
   *  request, and the server then drops the render. */
  async view(
    docId: string,
    entryId: number,
    changes: boolean,
    signal?: AbortSignal,
  ): Promise<ApiResult<HistoryView>> {
    return apiFetch<HistoryView>(`${historyPath(docId)}/entries/${entryId}/view?changes=${changes ? 1 : 0}`, {
      parser: (data) => parse(HistoryView, data),
      signal,
    });
  },

  /** Replaces the page's content with the entry's version. `endedAt` is the
   *  `ended_at` of the version the reader saw: the server answers 409
   *  `history.moved` when the entry has moved on since. */
  async restoreEntry(docId: string, entryId: number, endedAt: string): Promise<ApiResult<void>> {
    const res = await apiFetch<unknown>(
      `${historyPath(docId)}/entries/${entryId}/restore?ended_at=${encodeURIComponent(endedAt)}`,
      { method: "POST" },
    );
    return "error" in res ? res : { ok: undefined };
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
};
