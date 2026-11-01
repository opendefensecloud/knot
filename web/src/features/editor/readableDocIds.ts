/**
 * The pages the user can open, for the locked style on doc links. No list
 * the client has names every one of them: GET /api/docs leaves out
 * templates and archived pages, which have lists of their own. So the set
 * is the three lists together, fetched once per editor, not once per link.
 */

import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";

import { useSession } from "../../auth/SessionContext";
import type { ApiError } from "../../lib/api";
import { docsApi } from "../docs/docs.api";

/** A list query's answer: its items, or why it failed. */
type Listed<T> = { ok: T[] } | { error: ApiError } | undefined;

/** The ids of the pages the user can open, from the doc list, the template
 *  list and the archive (an archived page with the subpages that went with
 *  it); null while one of them is still on its way, so no link is locked on
 *  a guess. A list that failed (a viewer has no archive) adds nothing. */
export function combineReadableDocIds(
  lists: ReadonlyArray<Listed<{ id: string; subpage_ids?: string[] }> | "skipped">,
): ReadonlySet<string> | null {
  if (lists.some((list) => list === undefined)) return null;
  const ids = new Set<string>();
  for (const list of lists) {
    if (list === "skipped" || list === undefined || "error" in list) continue;
    for (const item of list.ok) {
      ids.add(item.id);
      for (const subpageId of item.subpage_ids ?? []) ids.add(subpageId);
    }
  }
  return ids;
}

/** The pages the user can open, for `setReadableDocIds`. The doc list is the
 *  sidebar's cache entry; the archive is asked for only by workspace editors
 *  and owners, as viewers never see archived pages. */
export function useReadableDocIds(): ReadonlySet<string> | null {
  const session = useSession();
  const workspaceRole = session.data && "ok" in session.data ? session.data.ok.role : null;
  const archiveVisible = workspaceRole === "owner" || workspaceRole === "editor";

  const docList = useQuery({ queryKey: ["docs"], queryFn: () => docsApi.list(), staleTime: 30_000 });
  const templateList = useQuery({
    queryKey: ["templates"],
    queryFn: () => docsApi.listTemplates(),
    staleTime: 30_000,
  });
  const archiveList = useQuery({
    queryKey: ["archive"],
    queryFn: () => docsApi.listArchive(),
    staleTime: 30_000,
    enabled: archiveVisible,
  });

  return useMemo(
    () =>
      combineReadableDocIds([
        docList.data,
        templateList.data,
        archiveVisible ? archiveList.data : "skipped",
      ]),
    [docList.data, templateList.data, archiveList.data, archiveVisible],
  );
}
