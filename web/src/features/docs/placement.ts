export type Placement = "nested" | "sibling";

/** Map a placement choice + the currently-open doc to the new doc's parent_id.
 *  `current` is null when no doc is open. "nested" files under the current doc;
 *  "sibling" files alongside it (same parent; null/top-level if current is top-level). */
export function placementParent(
  p: Placement,
  current: { id: string; parent_id: string | null } | null,
): string | null {
  if (!current) return null;
  return p === "nested" ? current.id : current.parent_id;
}
