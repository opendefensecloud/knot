import { apiFetch, type ApiResult } from "./api";

/** Derived on the server from the composer's texts: a non-empty range with
 *  replacement text is `replace`, with none `delete`; a caret is `insert`. */
export type SuggestionKind = "replace" | "insert" | "delete";

/** `applying` is the short-lived server claim between an owner's accept and
 *  the room applying the change; clients treat it like `pending`. */
export type SuggestionStatus = "pending" | "applying" | "accepted" | "rejected" | "withdrawn";

/** A threaded reply on a suggestion card. */
export type SuggestionReply = {
  id: string;
  suggestion_id: string;
  author_id: string;
  body: string;
  created_at: string;
  updated_at: string;
};

/** A proposed change to a doc range, anchored by Yjs relative positions. */
export type Suggestion = {
  id: string;
  doc_id: string;
  author_id: string;
  kind: SuggestionKind;
  position_y: string;       // base64-encoded Y.RelativePosition (range start)
  position_y_end: string;   // base64-encoded Y.RelativePosition (range end; equals start for insert)
  original_text: string;    // exact text of the range when suggested; empty for insert
  context_before: string;   // up to 40 chars before the range, for insertion cards
  proposed_text: string;    // empty for delete
  reason: string | null;
  status: SuggestionStatus;
  decided_by: string | null;
  decided_at: string | null;
  created_at: string;
  updated_at: string;
};

/** A suggestion with its replies, as `list` returns it. */
export type SuggestionWithReplies = Suggestion & { replies: SuggestionReply[] };

/** The body of `POST /suggestions`. The server checks that `kind` matches the
 *  texts (see `SuggestionKind`) and answers 400 `suggestion.invalid` if not. */
export type NewSuggestionBody = Pick<
  Suggestion,
  "kind" | "position_y" | "position_y_end" | "original_text" | "context_before" | "proposed_text"
> & { reason?: string };

const base = (docId: string) => `/api/docs/${encodeURIComponent(docId)}/suggestions`;
const one = (docId: string, sid: string) => `${base(docId)}/${encodeURIComponent(sid)}`;

/** REST client for `/api/docs/{id}/suggestions`. React Query keys are
 *  `["suggestions", docId, includeResolved]`; invalidate the prefix
 *  `["suggestions", docId]` after a mutation or a `topic: "suggestions"`
 *  realtime frame. */
export const suggestionsApi = {
  /** Pending (and `applying`) suggestions with their replies; resolved ones
   *  too when `includeResolved` is set. */
  async list(docId: string, includeResolved = false): Promise<ApiResult<SuggestionWithReplies[]>> {
    const params = new URLSearchParams();
    if (includeResolved) params.set("include_resolved", "true");
    const qs = params.toString();
    return apiFetch<SuggestionWithReplies[]>(`${base(docId)}${qs ? `?${qs}` : ""}`);
  },

  /** Editor+. Returns the created `Suggestion` (201). */
  async create(docId: string, body: NewSuggestionBody): Promise<ApiResult<Suggestion>> {
    return apiFetch<Suggestion>(base(docId), { method: "POST", body });
  },

  /** Owner only. `seenText` is the "Now" text the owner's card showed (empty
   *  for an insert); the server refuses with 409 `suggestion.changed` if the
   *  range no longer holds exactly that text. */
  async accept(docId: string, sid: string, seenText: string): Promise<ApiResult<Suggestion>> {
    return apiFetch<Suggestion>(`${one(docId, sid)}/accept`, {
      method: "POST",
      body: { seen_text: seenText },
    });
  },

  /** Owner only. */
  async reject(docId: string, sid: string): Promise<ApiResult<Suggestion>> {
    return apiFetch<Suggestion>(`${one(docId, sid)}/reject`, { method: "POST" });
  },

  /** The suggestion's author only. */
  async withdraw(docId: string, sid: string): Promise<ApiResult<Suggestion>> {
    return apiFetch<Suggestion>(`${one(docId, sid)}/withdraw`, { method: "POST" });
  },

  /** Editor+. `mentions` are user ids to notify. Returns 201 `SuggestionReply`. */
  async reply(
    docId: string,
    sid: string,
    body: string,
    mentions: string[] = [],
  ): Promise<ApiResult<SuggestionReply>> {
    return apiFetch<SuggestionReply>(`${one(docId, sid)}/replies`, {
      method: "POST",
      body: { body, mentions },
    });
  },

  /** The reply's author only. */
  async editReply(
    docId: string,
    sid: string,
    replyId: string,
    body: string,
  ): Promise<ApiResult<SuggestionReply>> {
    return apiFetch<SuggestionReply>(`${one(docId, sid)}/replies/${encodeURIComponent(replyId)}`, {
      method: "PATCH",
      body: { body },
    });
  },

  /** The reply's author or an effective Owner. */
  async removeReply(docId: string, sid: string, replyId: string): Promise<ApiResult<void>> {
    return apiFetch<void>(`${one(docId, sid)}/replies/${encodeURIComponent(replyId)}`, {
      method: "DELETE",
    });
  },
};
