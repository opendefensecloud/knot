import type { Doc } from "../../lib/validators";

export type TreeNode = Doc & { children: TreeNode[] };

/** Sibling order: by code unit, which is the server's `COLLATE "C"`. */
function bySortKey(a: Doc, b: Doc): number {
  return a.sort_key < b.sort_key ? -1 : a.sort_key > b.sort_key ? 1 : 0;
}

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
    console.warn(`buildTree: promoted unreachable node ${node.id} (parent cycle?)`);
    roots.push(node);
  });
  function sortRec(nodes: TreeNode[]) {
    nodes.sort(bySortKey);
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

// ---- Projection model ------------------------------------------------------
//
// dnd-kit's SortableTree model: the sidebar as a flat list of visible rows.
// A drop picks a gap between two rows (or the middle of a row, "into"); the
// horizontal drag picks the depth, clamped by the rows around the gap. The
// result is a move the server accepts, or null when nothing would change.

/** Pixels per tree level, as in the rows' `paddingLeft: 4 + depth*12`. */
export const INDENT = 12;

/** A sidebar row as drawn. `parentId` is the *rendered* parent: a page whose
 *  real parent isn't listed (a template's child, an alive child of an
 *  archived page) is drawn at the top level with `parentId: null`. Real
 *  parents come from the doc list. */
export type FlatRow = {
  id: string;
  parentId: string | null;
  depth: number;
  title: string;
  descendantCount: number;
};

export type Zone = "above" | "below" | "into";

/** Body of POST /api/docs/:id/move. No anchor means last. */
export type MoveArgs = { parent_id: string | null; before_id?: string; after_id?: string };

/** A drop's outcome: the depth to draw the drop line at, the depths the gap
 *  allows, and the move to send. */
export type Projection = { depth: number; minDepth: number; maxDepth: number; args: MoveArgs };

/** Subpages (at any depth) under `node`: the number archive and Duplicate use. */
export function countDescendants(node: TreeNode): number {
  return node.children.reduce((sum, child) => sum + 1 + countDescendants(child), 0);
}

/** What a screen reader hears about a row: "level 2, 3 subpages". */
export function rowDescription(row: FlatRow): string {
  const level = `level ${row.depth + 1}`;
  const n = row.descendantCount;
  return n === 0 ? level : `${level}, ${n} ${n === 1 ? "subpage" : "subpages"}`;
}

/** The rows in on-screen order. The children of collapsed rows are left out,
 *  and so is the subtree of the row being dragged (`activeId`, which stays),
 *  so no drop can put a page inside itself. */
export function flattenVisible(
  tree: TreeNode[],
  collapsed: ReadonlySet<string>,
  activeId?: string,
): FlatRow[] {
  const out: FlatRow[] = [];
  const walk = (nodes: TreeNode[], parentId: string | null, depth: number) => {
    for (const n of nodes) {
      out.push({ id: n.id, parentId, depth, title: n.title, descendantCount: countDescendants(n) });
      if (n.id !== activeId && !collapsed.has(n.id)) walk(n.children, n.id, depth + 1);
    }
  };
  walk(tree, null, 0);
  return out;
}

/** The part of the hovered row the pointer is in. On another row the top and
 *  bottom quarters are the gaps above and below it, and the middle nests into
 *  it. One's own row has no "into": its halves both mean the gap it sits in. */
export function dropZone(
  pointerY: number,
  rect: { top: number; height: number },
  isOwnRow: boolean,
): Zone {
  const rel = (pointerY - rect.top) / rect.height;
  if (isOwnRow) return rel < 0.5 ? "above" : "below";
  if (rel < 0.25) return "above";
  if (rel > 0.75) return "below";
  return "into";
}

/** Where `rows[i]`'s visible subtree ends: the index after its last row. */
function blockEnd(rows: FlatRow[], i: number): number {
  const depth = rows[i]!.depth;
  let end = i + 1;
  while (end < rows.length && rows[end]!.depth > depth) end++;
  return end;
}

/** `rows` without the row `id` and its visible subtree. */
function withoutRow(rows: FlatRow[], id: string): FlatRow[] {
  const i = rows.findIndex((r) => r.id === id);
  return i < 0 ? rows : [...rows.slice(0, i), ...rows.slice(blockEnd(rows, i))];
}

/** The page being moved and the rows it can move among. */
type Moving = {
  /** The visible rows without the page and its subtree. */
  items: FlatRow[];
  page: FlatRow;
  /** The gap of `items` the page sits in. */
  home: number;
  docs: Doc[];
  realParent: Map<string, string | null>;
};

function moving(flat: FlatRow[], docs: Doc[], id: string): Moving | null {
  const home = flat.findIndex((r) => r.id === id);
  const page = flat[home];
  const realParent = new Map(docs.map((d) => [d.id, d.parent_id ?? null]));
  if (!page || !realParent.has(id)) return null;
  // The subtree goes too, in case `flat` wasn't flattened around `id`.
  return { items: withoutRow(flat, id), page, home, docs, realParent };
}

/** The rendered parent and previous sibling of a row put at `depth` in gap
 *  `g` of `items`: together they name its slot on screen. */
function slotAt(items: FlatRow[], g: number, depth: number) {
  let prev: string | null = null;
  for (let i = g - 1; i >= 0; i--) {
    const r = items[i]!;
    if (r.depth < depth) return { parent: r.id, prev };
    if (r.depth === depth) prev ??= r.id;
  }
  return { parent: null, prev };
}

/** Anchors come only from *real* siblings, pages whose real parent is
 *  `parent`; the server rejects any other (422 doc.bad_anchor). The nearest
 *  one before the gap, else the nearest after, else none (last). */
function anchored(m: Moving, g: number, depth: number, parent: string | null): MoveArgs {
  const { items } = m;
  const real = (r: FlatRow) => r.depth === depth && m.realParent.get(r.id) === parent;
  for (let i = g - 1; i >= 0 && items[i]!.depth >= depth; i--) {
    if (real(items[i]!)) return { parent_id: parent, after_id: items[i]!.id };
  }
  for (let i = g; i < items.length && items[i]!.depth >= depth; i++) {
    if (real(items[i]!)) return { parent_id: parent, before_id: items[i]!.id };
  }
  return { parent_id: parent };
}

/** The move keeps the page's real parent and its place among its real siblings. */
function isNoop(m: Moving, args: MoveArgs): boolean {
  if (m.realParent.get(m.page.id) !== args.parent_id) return false;
  const sibs = m.docs
    .filter((d) => (d.parent_id ?? null) === args.parent_id)
    .sort(bySortKey)
    .map((d) => d.id);
  const rest = sibs.filter((id) => id !== m.page.id);
  const at =
    args.after_id !== undefined ? rest.indexOf(args.after_id) + 1
    : args.before_id !== undefined ? rest.indexOf(args.before_id)
    : rest.length;
  return at === sibs.indexOf(m.page.id);
}

/** P11 for a drop in gap `g` of `m.items` at the `wanted` depth (clamped). */
function projectGap(m: Moving, g: number, wanted: number): Projection | null {
  const prev = m.items[g - 1];
  const next = m.items[g];
  const maxDepth = prev ? prev.depth + 1 : 0;
  const minDepth = next ? next.depth : 0;
  const depth = Math.min(maxDepth, Math.max(minDepth, wanted));
  // 1. Drop in place: the same slot on screen, whichever row is hovered.
  const slot = slotAt(m.items, g, depth);
  const home = slotAt(m.items, m.home, m.page.depth);
  if (slot.parent === home.parent && slot.prev === home.prev) return null;
  // 2-3. The projected parent and position, anchored on real siblings.
  const args = anchored(m, g, depth, slot.parent);
  // 4. Same real parent and position: nothing to send.
  return isNoop(m, args) ? null : { depth, minDepth, maxDepth, args };
}

/** Where dropping `activeId` would put it, or null when nothing would change
 *  (P11). `flat` is `flattenVisible(tree, collapsed, activeId)`, `docs` the
 *  doc list the tree was built from, `depthDelta` the horizontal drag in
 *  `INDENT`s. */
export function projectDrop(
  flat: FlatRow[],
  docs: Doc[],
  activeId: string,
  overId: string,
  zone: Zone,
  depthDelta: number,
): Projection | null {
  const m = moving(flat, docs, activeId);
  if (!m) return null;
  const wanted = m.page.depth + depthDelta;
  // On one's own row both halves are the gap the page sits in.
  if (overId === activeId) return zone === "into" ? null : projectGap(m, m.home, wanted);
  const oi = m.items.findIndex((r) => r.id === overId);
  const over = m.items[oi];
  if (!over) return null;
  if (zone !== "into") return projectGap(m, zone === "above" ? oi : oi + 1, wanted);
  // Into: the last child, wherever the pointer is horizontally.
  const args: MoveArgs = { parent_id: over.id };
  const depth = over.depth + 1;
  return isNoop(m, args) ? null : { depth, minDepth: depth, maxDepth: depth, args };
}

/** The menu and Alt+arrow moves of `id`, or null when it can't move that way.
 *  Up and down move the page (with its subtree) past its previous or next
 *  sibling, indent makes it the previous sibling's last child, and outdent
 *  puts it right after its parent. */
export function keyboardMove(
  flat: FlatRow[],
  docs: Doc[],
  id: string,
  cmd: "up" | "down" | "indent" | "outdent",
): MoveArgs | null {
  const m = moving(flat, docs, id);
  if (!m) return null;
  const { items, home } = m;
  const depth = m.page.depth;
  const at = (g: number, wanted: number) => projectGap(m, g, wanted)?.args ?? null;
  /** The nearest row at the page's depth from `i` on, within its parent. */
  const sibling = (i: number, step: 1 | -1): number => {
    for (; i >= 0 && i < items.length && items[i]!.depth >= depth; i += step) {
      if (items[i]!.depth === depth) return i;
    }
    return -1;
  };
  switch (cmd) {
    case "up": {
      const s = sibling(home - 1, -1);
      return s < 0 ? null : at(s, depth);
    }
    case "down": {
      const s = sibling(home, 1);
      return s < 0 ? null : at(blockEnd(items, s), depth);
    }
    case "indent":
      return sibling(home - 1, -1) < 0 ? null : at(home, depth + 1);
    case "outdent": {
      let p = home - 1;
      while (p >= 0 && items[p]!.depth >= depth) p--;
      return p < 0 ? null : at(blockEnd(items, p), depth - 1);
    }
  }
}

/** The live-region announcement for a move, by title:
 *  "Moved “B” into “A”, position 2 of 3". `flat` holds the rows before the
 *  move. The position is left out when the new parent's children are hidden
 *  (it is collapsed), since they can't be counted. */
export function describeMove(flat: FlatRow[], id: string, args: MoveArgs): string {
  const head = `Moved “${flat.find((r) => r.id === id)?.title ?? ""}”`;
  const items = withoutRow(flat, id);
  const pi = args.parent_id === null ? -1 : items.findIndex((r) => r.id === args.parent_id);
  const parent = items[pi];
  if (args.parent_id !== null && !parent) return head;
  const where = parent ? `${head} into “${parent.title}”` : `${head} to the top level`;
  const depth = parent ? parent.depth + 1 : 0;
  const sibs: string[] = [];
  for (let i = pi + 1; i < items.length && items[i]!.depth >= depth; i++) {
    if (items[i]!.depth === depth) sibs.push(items[i]!.id);
  }
  // A parent with subpages but no visible row under it is collapsed. (In
  // `flat` the moved page itself still counts as visible.)
  const below = parent ? flat[flat.indexOf(parent) + 1] : undefined;
  const hidden =
    parent !== undefined && parent.descendantCount > 0 && !(below && below.depth > parent.depth);
  const anchor = args.after_id ?? args.before_id;
  if (hidden || (anchor !== undefined && !sibs.includes(anchor))) return where;
  const at =
    args.after_id !== undefined ? sibs.indexOf(args.after_id) + 1
    : args.before_id !== undefined ? sibs.indexOf(args.before_id)
    : sibs.length;
  return `${where}, position ${at + 1} of ${sibs.length + 1}`;
}
