/**
 * The user's favorite pages, `/api/me/favorites`:
 *
 * - `GET /api/me/favorites` — oldest star first, only pages the user can
 *   read that are neither archived nor templates.
 * - `PUT /api/me/favorites/{doc_id}` — star; idempotent. 403 `acl.no_grant`,
 *   404 `doc.not_found`, 409 `doc.archived`, 422 `favorite.not_a_page`,
 *   409 `favorite.limit_reached` (`details.limit`).
 * - `DELETE /api/me/favorites/{doc_id}` — unstar; idempotent, no ACL check.
 *
 * Each call throws an `ApiRequestError` on a refusal, so a query fails and
 * a mutation rolls back.
 */
import * as v from "valibot";

import { ApiRequestError, apiFetch } from "../../lib/api";
import { Favorite, parse } from "../../lib/validators";

const path = (docId: string) => `/api/me/favorites/${encodeURIComponent(docId)}`;

export const favoritesApi = {
  async list(): Promise<Favorite[]> {
    const r = await apiFetch<unknown>("/api/me/favorites");
    if ("error" in r) throw new ApiRequestError(r.error);
    return parse(v.array(Favorite), r.ok);
  },
  async star(docId: string): Promise<void> {
    const r = await apiFetch<unknown>(path(docId), { method: "PUT" });
    if ("error" in r) throw new ApiRequestError(r.error);
  },
  async unstar(docId: string): Promise<void> {
    const r = await apiFetch<unknown>(path(docId), { method: "DELETE" });
    if ("error" in r) throw new ApiRequestError(r.error);
  },
};
