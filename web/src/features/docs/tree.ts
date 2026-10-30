import type { Role } from "../../auth/useEffectiveRole";
import type { Doc } from "../../lib/validators";

export type TreeNode = Doc & { children: TreeNode[] };

/** Sibling order: by code unit, which is the server's `COLLATE "C"`. */
function bySortKey(a: Doc, b: Doc): number {
  return a.sort_key < b.sort_key ? -1 : a.sort_key > b.sort_key ? 1 : 0;
}

/** Stands in for the real parent of a page whose parent the user can't see
 *  (`parent_hidden`: the server sends `parent_id: null`). Doc ids are UUIDs,
 *  so it can't clash. Such a page is never a top-level sibling: no move may
 *  anchor on it there, and it doesn't take part in top-level order. */
const HIDDEN_PARENT = ":hidden";

/** A page's real parent as far as moves go: `HIDDEN_PARENT` when it is hidden. */
function realParentOf(d: Doc): string | null {
  return d.parent_hidden ? HIDDEN_PARENT : (d.parent_id ?? null);
}

/** The sidebar's two sections: the workspace's pages, and the pages shared
 *  with the user whose parent they can't see ("Shared with you"), each a
 *  root of its own there. */
export type Tree = { roots: TreeNode[]; shared: TreeNode[] };

/** Build a tree from a flat doc list. Sorts siblings by sort_key
 *  (LexoRank-style, lexicographic). Orphans (parent_id missing) become
 *  top-level. Pages whose parent is hidden (`parent_hidden`) go to `shared`
 *  instead of `roots`. TOTAL: no doc can ever be dropped from the output,
 *  even if parent cycles exist. Cycle members are promoted to roots. */
export function buildTree(docs: Doc[]): Tree {
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
  return {
    roots: roots.filter((n) => !n.parent_hidden),
    shared: roots.filter((n) => n.parent_hidden),
  };
}

/** `docs` without page `id` and every listed page below it. */
export function withoutSubtree(docs: Doc[], id: string): Doc[] {
  const removed = new Set([id]);
  // Parents can be listed after their children: repeat until nothing changes.
  for (let grew = true; grew; ) {
    grew = false;
    for (const d of docs) {
      if (!removed.has(d.id) && d.parent_id !== null && removed.has(d.parent_id)) {
        removed.add(d.id);
        grew = true;
      }
    }
  }
  return docs.filter((d) => !removed.has(d.id));
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
    const sibs = rest.filter((d) => realParentOf(d) === parent);
    const last = sibs[sibs.length - 1];
    updated = { ...moved, parent_id: parent, sort_key: last ? last.sort_key : moved.sort_key };
    insertAt = last ? rest.findIndex((d) => d.id === last.id) + 1 : rest.length;
  }
  const out = [...rest];
  out.splice(insertAt, 0, updated);
  return out;
}

/** The restricted page that decides who can see page `id`: the nearest
 *  restricted one at or above it, as far as the list shows. Null outside
 *  every restricted page. A page in a restricted subtree whose parent isn't
 *  listed (one shared with the user) has a restricted page the user can't
 *  see, so it stands for one of its own: two of them may share theirs, but
 *  nothing tells. */
function restrictionRootOf(byId: ReadonlyMap<string, Doc>, id: string): string | null {
  const seen = new Set<string>();
  for (let page = byId.get(id); page && !seen.has(page.id); ) {
    seen.add(page.id);
    if (page.restricted) return page.id;
    if (!page.within_restricted) return null;
    const parent = page.parent_id === null ? undefined : byId.get(page.parent_id);
    if (!parent) return `${HIDDEN_PARENT}:${page.id}`;
    page = parent;
  }
  return null;
}

/** The question to ask before a move that changes who can see the page:
 *  into a restricted subtree, out of one to where the workspace can see it,
 *  or from one into another (a restricted page nested in its own counts as
 *  another). Null when the page stays under the same restricted page, or
 *  outside all of them. A page that is itself restricted keeps its own
 *  access wherever it goes, so it never asks. */
export function restrictionCrossingText(docs: Doc[], movedId: string, parentId: string | null): string | null {
  const byId = new Map(docs.map((d) => [d.id, d]));
  const moved = byId.get(movedId);
  if (!moved || moved.restricted) return null;
  const parent = parentId === null ? undefined : byId.get(parentId);
  const fromRoot = restrictionRootOf(byId, movedId);
  const toRoot = parentId === null ? null : restrictionRootOf(byId, parentId);
  if (fromRoot === toRoot) return null;
  if (toRoot === null) return `Moving "${moved.title}" here makes it visible to everyone in the workspace.`;
  // From one restricted page into another the audience may grow or shrink.
  const change = fromRoot === null ? "limits who can see it to" : "changes who can see it to the";
  return `Moving "${moved.title}" into "${parent?.title ?? ""}" ${change} people with access there.`;
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

/** What a screen reader hears about a row: "level 2, 3 subpages", and
 *  ", restricted" for a page in a restricted subtree. */
export function rowDescription(row: FlatRow, withinRestricted = false): string {
  const level = `level ${row.depth + 1}`;
  const n = row.descendantCount;
  const described = n === 0 ? level : `${level}, ${n} ${n === 1 ? "subpage" : "subpages"}`;
  return withinRestricted ? `${described}, restricted` : described;
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
  /** Whether the user may add pages under `parent` (null: the top level). */
  canHold: (parent: string | null) => boolean;
};

/** Whether a role may add pages: an editor's or an owner's. */
function addsPages(role: Role | null): boolean {
  return role === "owner" || role === "editor";
}

/** Whether the user may add pages under `parent`, and so move one there:
 *  by their role on it, and at the top level by their workspace role. A
 *  page the list gives no role takes the workspace role. Without a
 *  workspace role (`undefined`: the caller doesn't say) the server decides. */
export function canAddUnder(docs: Doc[], parent: string | null, workspaceRole?: Role | null): boolean {
  if (workspaceRole === undefined) return true;
  if (parent === null) return addsPages(workspaceRole);
  return addsPages(docs.find((d) => d.id === parent)?.effective_role ?? workspaceRole);
}

function moving(flat: FlatRow[], docs: Doc[], id: string, workspaceRole?: Role | null): Moving | null {
  const home = flat.findIndex((r) => r.id === id);
  const page = flat[home];
  const realParent = new Map(docs.map((d) => [d.id, realParentOf(d)]));
  // A page whose parent is hidden doesn't move: the user can't see where it
  // is, and moving it to the top level would declassify it.
  if (!page || !realParent.has(id) || realParent.get(id) === HIDDEN_PARENT) return null;
  const canHold = (parent: string | null) => canAddUnder(docs, parent, workspaceRole);
  // The subtree goes too, in case `flat` wasn't flattened around `id`.
  return { items: withoutRow(flat, id), page, home, docs, realParent, canHold };
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

/** Where a move puts the page among `sibs`, its new siblings without it:
 *  right after `after_id`, at `before_id`'s place, or last. An anchor that
 *  isn't among `sibs` counts as none, so the page goes last, as
 *  applyOptimisticMove puts it. */
export function insertIndex(sibs: readonly string[], args: MoveArgs): number {
  const anchor = args.after_id ?? args.before_id;
  const at = anchor === undefined ? -1 : sibs.indexOf(anchor);
  if (at < 0) return sibs.length;
  return args.after_id !== undefined ? at + 1 : at;
}

/** The move keeps the page's real parent and its place among its real siblings. */
function isNoop(m: Moving, args: MoveArgs): boolean {
  if (m.realParent.get(m.page.id) !== args.parent_id) return false;
  const sibs = m.docs
    .filter((d) => realParentOf(d) === args.parent_id)
    .sort(bySortKey)
    .map((d) => d.id);
  return insertIndex(sibs.filter((id) => id !== m.page.id), args) === sibs.indexOf(m.page.id);
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
  // 4. Same real parent and position: nothing to send. A parent the user
  // can't add pages under takes no drop: the server would refuse it.
  return isNoop(m, args) || !m.canHold(args.parent_id) ? null : { depth, minDepth, maxDepth, args };
}

/** Where dropping `activeId` would put it, or null when nothing would change
 *  (P11) or the user can't add pages there. `flat` is
 *  `flattenVisible(tree, collapsed, activeId)`, `docs` the doc list the tree
 *  was built from, `depthDelta` the horizontal drag in `INDENT`s,
 *  `workspaceRole` the user's (see `canAddUnder`). */
export function projectDrop(
  flat: FlatRow[],
  docs: Doc[],
  activeId: string,
  overId: string,
  zone: Zone,
  depthDelta: number,
  workspaceRole?: Role | null,
): Projection | null {
  const m = moving(flat, docs, activeId, workspaceRole);
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
  return isNoop(m, args) || !m.canHold(over.id) ? null : { depth, minDepth: depth, maxDepth: depth, args };
}

/** The menu and Alt+arrow moves of `id`, or null when it can't move that way.
 *  Up and down move the page (with its subtree) past its previous or next
 *  sibling, indent makes it the previous sibling's last child, and outdent
 *  puts it right after its parent. Up and down pass over pages drawn at the
 *  top level whose parent isn't listed (a template's child): moving past one
 *  alone changes no real order, so they go on to the next sibling. */
export function keyboardMove(
  flat: FlatRow[],
  docs: Doc[],
  id: string,
  cmd: "up" | "down" | "indent" | "outdent",
  workspaceRole?: Role | null,
): MoveArgs | null {
  const m = moving(flat, docs, id, workspaceRole);
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
    case "up":
    case "down": {
      const step = cmd === "up" ? -1 : 1;
      // Up: the gap above sibling `s`. Down: the gap after its visible subtree.
      for (let s = sibling(cmd === "up" ? home - 1 : home, step); s >= 0; s = sibling(s + step, step)) {
        const args = at(cmd === "up" ? s : blockEnd(items, s), depth);
        if (args) return args;
      }
      return null;
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
 *  move, `docs` the pages they show. The position is left out when it can't
 *  be told: when the new parent's children are hidden (it is collapsed), and
 *  when pages drawn at the top level whose parent isn't listed sit between
 *  the page's new real neighbours. The server places a page among its real
 *  siblings only, so where it lands among those is up to its new key. */
export function describeMove(flat: FlatRow[], docs: Doc[], id: string, args: MoveArgs): string {
  const head = `Moved “${flat.find((r) => r.id === id)?.title ?? ""}”`;
  const items = withoutRow(flat, id);
  const pi = args.parent_id === null ? -1 : items.findIndex((r) => r.id === args.parent_id);
  const parent = items[pi];
  if (args.parent_id !== null && !parent) return head;
  const where = parent ? `${head} into “${parent.title}”` : `${head} to the top level`;
  const depth = parent ? parent.depth + 1 : 0;
  // The rows drawn as the new siblings, and among them the real ones.
  const sibs: string[] = [];
  for (let i = pi + 1; i < items.length && items[i]!.depth >= depth; i++) {
    if (items[i]!.depth === depth) sibs.push(items[i]!.id);
  }
  const realParent = new Map(docs.map((d) => [d.id, realParentOf(d)]));
  const real = sibs.filter((s) => realParent.get(s) === args.parent_id);
  // A parent with subpages but no visible row under it is collapsed. (In
  // `flat` the moved page itself still counts as visible.)
  const below = parent ? flat[flat.indexOf(parent) + 1] : undefined;
  const hidden =
    parent !== undefined && parent.descendantCount > 0 && !(below && below.depth > parent.depth);
  const anchor = args.after_id ?? args.before_id;
  if (hidden || (anchor !== undefined && !real.includes(anchor))) return where;
  // The page goes between two real siblings (or the start or end): on
  // screen, between the rows that show them.
  const at = insertIndex(real, args);
  const before = at === 0 ? -1 : sibs.indexOf(real[at - 1]!);
  const after = at === real.length ? sibs.length : sibs.indexOf(real[at]!);
  if (after - before > 1) return where;
  return `${where}, position ${before + 2} of ${sibs.length + 1}`;
}
