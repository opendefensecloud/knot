/**
 * The pages the `[[` picker offers for a query: title matches from the
 * client's doc list, prefix matches first, each with its ancestor path so
 * same-named pages can be told apart. Pure, so it is tested on its own.
 */

/** A page as the doc list knows it. */
export type PageLinkPage = { id: string; title: string; parent_id: string | null };

/** A page the picker shows: its title and its ancestors' titles, root first. */
export type PageLinkCandidate = { id: string; title: string; path: string[] };

/** At most this many pages in the picker. */
export const PAGE_LINK_LIMIT = 8;

const UNTITLED = "Untitled";

const shownTitle = (title: string) => (title.trim() === "" ? UNTITLED : title);

/** The titles of `page`'s ancestors, root first. Stops at a parent the list
 *  does not have and at a cycle, so bad data never loops. */
function ancestorPath(page: PageLinkPage, byId: ReadonlyMap<string, PageLinkPage>): string[] {
  const path: string[] = [];
  const seen = new Set<string>([page.id]);
  let parentId = page.parent_id;
  while (parentId !== null && !seen.has(parentId)) {
    const parent = byId.get(parentId);
    if (!parent) break;
    seen.add(parentId);
    path.unshift(shownTitle(parent.title));
    parentId = parent.parent_id;
  }
  return path;
}

export function pageLinkCandidates(
  pages: readonly PageLinkPage[],
  query: string,
  currentDocId: string | null,
  limit: number = PAGE_LINK_LIMIT,
): PageLinkCandidate[] {
  const q = query.trim().toLowerCase();
  const byId = new Map(pages.map((p) => [p.id, p]));
  return pages
    .filter((p) => p.id !== currentDocId)
    .map((p) => ({ page: p, title: shownTitle(p.title) }))
    .map((c) => ({ ...c, at: c.title.toLowerCase().indexOf(q) }))
    .filter((c) => c.at >= 0)
    .sort((a, b) => Number(a.at !== 0) - Number(b.at !== 0) || a.title.localeCompare(b.title))
    .slice(0, limit)
    .map((c) => ({ id: c.page.id, title: c.title, path: ancestorPath(c.page, byId) }));
}
