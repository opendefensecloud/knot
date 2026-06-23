import type { Doc } from "../../lib/validators";

export type TreeNode = Doc & { children: TreeNode[] };

/** Build a tree from a flat doc list. Sorts siblings by sort_key
 *  (LexoRank-style, lexicographic). Orphans (parent_id missing) become
 *  top-level. TOTAL: no doc can ever be dropped from the output, even if
 *  parent cycles exist. Cycle members are promoted to roots. */
export function buildTree(docs: Doc[]): TreeNode[] {
  const byId = new Map<string, TreeNode>();
  docs.forEach((d) => byId.set(d.id, { ...d, children: [] }));
  const roots: TreeNode[] = [];
  byId.forEach((node) => {
    const pid = node.parent_id;
    if (pid && byId.has(pid)) byId.get(pid)!.children.push(node);
    else roots.push(node);
  });
  // Safety net: every node must be reachable from a real root. Cycle members
  // are unreachable; promote them to roots (and unlink from their cyclic
  // parent) so no document can ever vanish from the tree.
  const reachable = new Set<string>();
  const stack = [...roots];
  while (stack.length) {
    const n = stack.pop()!;
    if (reachable.has(n.id)) continue;
    reachable.add(n.id);
    n.children.forEach((c) => stack.push(c));
  }
  byId.forEach((node) => {
    if (reachable.has(node.id)) return;
    const pid = node.parent_id;
    if (pid && byId.has(pid)) {
      const sib = byId.get(pid)!.children;
      const i = sib.indexOf(node);
      if (i >= 0) sib.splice(i, 1);
    }
    // eslint-disable-next-line no-console
    console.warn(`buildTree: promoted unreachable node ${node.id} (parent cycle?)`);
    roots.push(node);
  });
  const cmp = (a: TreeNode, b: TreeNode) =>
    a.sort_key < b.sort_key ? -1 : a.sort_key > b.sort_key ? 1 : 0;
  function sortRec(nodes: TreeNode[]) {
    nodes.sort(cmp);
    nodes.forEach((n) => sortRec(n.children));
  }
  sortRec(roots);
  return roots;
}

/** Decide drop action from the dragged item's vertical center relative to the
 *  row it is over. Top quarter → before, bottom quarter → after, else into. */
export function dropIntent(
  activeCenterY: number,
  rect: { top: number; height: number },
): "before" | "after" | "into" {
  const rel = (activeCenterY - rect.top) / rect.height;
  if (rel < 0.25) return "before";
  if (rel > 0.75) return "after";
  return "into";
}

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

/** Optimistically apply a move to the flat doc list. Sets the moved doc's
 *  parent and a *provisional* sort_key (copied from the destination neighbor)
 *  and repositions it in the array so buildTree's stable sort lands it in the
 *  intended slot. The server's authoritative sort_key replaces this on the
 *  next refetch (onSettled). */
export function applyOptimisticMove(
  docs: Doc[],
  movedId: string,
  args: { parent_id?: string | null; before_id?: string; after_id?: string },
): Doc[] {
  const moved = docs.find((d) => d.id === movedId);
  if (!moved) return docs;
  const parent = args.parent_id ?? null;
  const rest = docs.filter((d) => d.id !== movedId);

  let updated: Doc;
  let insertAt: number;
  if (args.before_id) {
    const i = rest.findIndex((d) => d.id === args.before_id);
    updated = { ...moved, parent_id: parent, sort_key: i >= 0 ? rest[i]!.sort_key : moved.sort_key };
    insertAt = i >= 0 ? i : rest.length;
  } else if (args.after_id) {
    const i = rest.findIndex((d) => d.id === args.after_id);
    updated = { ...moved, parent_id: parent, sort_key: i >= 0 ? rest[i]!.sort_key : moved.sort_key };
    insertAt = i >= 0 ? i + 1 : rest.length;
  } else {
    const sibs = rest.filter((d) => (d.parent_id ?? null) === parent);
    const last = sibs[sibs.length - 1];
    updated = { ...moved, parent_id: parent, sort_key: last ? last.sort_key : moved.sort_key };
    insertAt = last ? rest.findIndex((d) => d.id === last.id) + 1 : rest.length;
  }
  const out = [...rest];
  out.splice(insertAt, 0, updated);
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
