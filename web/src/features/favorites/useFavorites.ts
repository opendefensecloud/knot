import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useMemo } from "react";

import { ApiRequestError } from "../../lib/api";
import { LIMIT_TOAST_KEY, notifyFailure } from "../../lib/limitErrors";
import type { Favorite } from "../../lib/validators";
import { useUi } from "../../stores/ui";

import { favoritesApi } from "./favorites.api";

export const FAVORITES_KEY = ["favorites"] as const;

/** Stars and unstars share one scope, so they reach the server one at a
 *  time, in the order they were made. */
const FAVORITES_SCOPE = "favorites";

/** As fresh as the tree: refetched after the user's own changes. */
const STALE_MS = 30_000;

type Change = { id: string; title: string; icon: string | null; star: boolean };

/** What a change replaced: the entry it removed, and where it stood. */
type Undo = { removed: Favorite | undefined; index: number };

/** The text for a refused star or unstar, or null to leave it to
 *  `notifyFailure` (rate limits) or its fallback. */
function limitReachedText(error: unknown): string | null {
  if (!(error instanceof ApiRequestError) || error.apiError.code !== "favorite.limit_reached") return null;
  const limit = error.apiError.details.limit;
  return typeof limit === "number"
    ? `You have ${limit} favorites, the most allowed. Remove one to add another.`
    : "You have as many favorites as allowed. Remove one to add another.";
}

/**
 * The user's favorite pages, in server order (oldest star first), and a
 * `toggle` that stars or unstars a page.
 *
 * `toggle` is optimistic: the page moves in or out of the list at once,
 * and goes back where it was if the server refuses. It does nothing until
 * the list has loaded (`ready`), so a star never claims a state the server
 * hasn't shown.
 */
export function useFavorites(): {
  favorites: Favorite[] | undefined;
  favoriteIds: ReadonlySet<string>;
  ready: boolean;
  isFavorite(id: string): boolean;
  toggle(doc: { id: string; title: string; icon: string | null }): void;
} {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const query = useQuery({ queryKey: FAVORITES_KEY, queryFn: () => favoritesApi.list(), staleTime: STALE_MS });
  const favorites = query.data;

  const { mutate } = useMutation({
    mutationKey: FAVORITES_KEY,
    scope: { id: FAVORITES_SCOPE },
    mutationFn: (c: Change) => (c.star ? favoritesApi.star(c.id) : favoritesApi.unstar(c.id)),
    onMutate: async (c): Promise<Undo> => {
      await qc.cancelQueries({ queryKey: FAVORITES_KEY });
      const list = qc.getQueryData<Favorite[]>(FAVORITES_KEY) ?? [];
      const index = list.findIndex((f) => f.doc_id === c.id);
      if (c.star) {
        if (index < 0) {
          const added: Favorite = {
            doc_id: c.id,
            title: c.title,
            icon: c.icon,
            favorited_at: new Date().toISOString(),
          };
          qc.setQueryData<Favorite[]>(FAVORITES_KEY, [...list, added]);
        }
        return { removed: undefined, index };
      }
      qc.setQueryData<Favorite[]>(FAVORITES_KEY, list.filter((f) => f.doc_id !== c.id));
      return { removed: list[index], index };
    },
    onError: (error, c, undo) => {
      // Undo only this change, not the whole list: changes queued after it
      // have put their own optimistic updates on top.
      const list = qc.getQueryData<Favorite[]>(FAVORITES_KEY);
      if (list && undo) {
        const present = list.some((f) => f.doc_id === c.id);
        if (c.star && undo.index < 0 && present) {
          qc.setQueryData<Favorite[]>(FAVORITES_KEY, list.filter((f) => f.doc_id !== c.id));
        } else if (!c.star && undo.removed && !present) {
          const next = [...list];
          next.splice(Math.min(undo.index, next.length), 0, undo.removed);
          qc.setQueryData<Favorite[]>(FAVORITES_KEY, next);
        }
      }
      const limitText = limitReachedText(error);
      if (limitText !== null) {
        notify("warn", limitText, { key: LIMIT_TOAST_KEY });
        return;
      }
      notifyFailure(
        notify,
        error instanceof ApiRequestError ? error.apiError : null,
        c.star ? "Couldn't add to favorites" : "Couldn't remove from favorites",
      );
    },
    onSettled: () => {
      // After the last queued change only: a list fetched in between would
      // drop the optimistic updates of the changes still on their way.
      if (qc.isMutating({ mutationKey: FAVORITES_KEY }) === 1) {
        void qc.invalidateQueries({ queryKey: FAVORITES_KEY });
      }
    },
  });

  const favoriteIds = useMemo<ReadonlySet<string>>(
    () => new Set(favorites?.map((f) => f.doc_id) ?? []),
    [favorites],
  );
  const isFavorite = useCallback((id: string) => favoriteIds.has(id), [favoriteIds]);

  const toggle = useCallback(
    (doc: { id: string; title: string; icon: string | null }) => {
      // Read from the cache, not the last render: a second click can come
      // before the first one's optimistic update has rendered.
      const list = qc.getQueryData<Favorite[]>(FAVORITES_KEY);
      if (!list) return;
      const star = !list.some((f) => f.doc_id === doc.id);
      mutate({ id: doc.id, title: doc.title, icon: doc.icon, star });
    },
    [qc, mutate],
  );

  return { favorites, favoriteIds, ready: favorites !== undefined, isFavorite, toggle };
}
