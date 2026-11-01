// Test-only: main's drop helpers, kept as the reference for tree.test.ts's
// compatibility table ("projectDrop matches today's moveArgs") and its
// stand-in server. No production code imports this file.

import type { Doc } from "../../lib/validators";

/** All descendant ids of `rootId` in the flat list (excludes root). Cycle-safe. */
export function descendantIds(docs: Doc[], rootId: string): Set<string> {
  const kids = new Map<string, string[]>();
  docs.forEach((d) => {
    if (d.parent_id) {
      const a = kids.get(d.parent_id) ?? [];
      a.push(d.id);
      kids.set(d.parent_id, a);
    }
  });
  const out = new Set<string>();
  const stack = [...(kids.get(rootId) ?? [])];
  while (stack.length) {
    const id = stack.pop()!;
    if (out.has(id)) continue;
    out.add(id);
    (kids.get(id) ?? []).forEach((c) => stack.push(c));
  }
  return out;
}

/** Map a drop target + drop position to the args expected by
 *  POST /api/docs/:id/move. */
export function moveArgs(
  target: Doc | null,
  position: "before" | "after" | "into",
): { parent_id?: string | null; before_id?: string; after_id?: string } {
  if (!target) return { parent_id: null };
  switch (position) {
    case "before": return { parent_id: target.parent_id, before_id: target.id };
    case "after":  return { parent_id: target.parent_id, after_id: target.id };
    case "into":   return { parent_id: target.id };
  }
}
