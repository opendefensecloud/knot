import { apiFetch, type ApiError, type ApiResult } from "./api";
import { Doc, HistoryPage, HistoryView, parse, RestoreAnswer } from "./validators";

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

  /** Brings the page back to the entry's version and says how. `endedAt`
   *  is the `ended_at` of the version the reader saw: the server answers
   *  409 `history.moved` when the entry has moved on since. */
  async restoreEntry(docId: string, entryId: number, endedAt: string): Promise<ApiResult<RestoreAnswer>> {
    return apiFetch<RestoreAnswer>(
      `${historyPath(docId)}/entries/${entryId}/restore?ended_at=${encodeURIComponent(endedAt)}`,
      { method: "POST", parser: (data) => parse(RestoreAnswer, data) },
    );
  },

  /** Copies the entry's version into a new page beside the page, through
   *  Duplicate, and answers that page as Duplicate does. `version_label`
   *  ends the new title (`… (version of <label>)`); `ended_at`, as for
   *  `restoreEntry`, makes a moved entry a 409 `history.moved`. */
  async restoreAsNew(
    docId: string,
    entryId: number,
    body: { version_label: string; ended_at: string },
  ): Promise<ApiResult<Doc>> {
    const r = await apiFetch<unknown>(`${historyPath(docId)}/entries/${entryId}/restore-as-new`, {
      method: "POST",
      body,
    });
    if ("error" in r) return r;
    return { ok: parse(Doc, r.ok) };
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
