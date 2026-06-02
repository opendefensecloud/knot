import type { Doc } from "../../lib/validators";

export type TreeNode = Doc & { children: TreeNode[] };

/** Build a tree from a flat doc list. Sorts siblings by sort_key
 *  (LexoRank-style, lexicographic). Orphans (parent_id missing) become
 *  top-level. */
export function buildTree(docs: Doc[]): TreeNode[] {
  const byId = new Map<string, TreeNode>();
  docs.forEach((d) => byId.set(d.id, { ...d, children: [] }));
  const roots: TreeNode[] = [];
  byId.forEach((node) => {
    if (node.parent_id && byId.has(node.parent_id)) {
      byId.get(node.parent_id)!.children.push(node);
    } else {
      roots.push(node);
    }
  });
  const sortKey = (a: TreeNode, b: TreeNode) =>
    a.sort_key < b.sort_key ? -1 : a.sort_key > b.sort_key ? 1 : 0;
  function sortRec(nodes: TreeNode[]) {
    nodes.sort(sortKey);
    nodes.forEach((n) => sortRec(n.children));
  }
  sortRec(roots);
  return roots;
}
