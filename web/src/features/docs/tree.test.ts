import { describe, expect, it } from "vitest";
import type { Doc } from "../../lib/validators";
import { buildTree, descendantIds, dropIntent, applyOptimisticMove, moveArgs } from "./tree";
import {
  INDENT,
  countDescendants,
  describeMove,
  dropZone,
  flattenVisible,
  keyboardMove,
  projectDrop,
  rowDescription,
  type FlatRow,
  type MoveArgs,
  type Zone,
} from "./tree";

function doc(id: string, parent: string | null, sort_key: string): Doc {
  return {
    id,
    workspace_id: "w",
    parent_id: parent,
    title: id,
    sort_key,
    icon: null,
    created_by: "u",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
  };
}

describe("buildTree", () => {
  it("returns empty for empty input", () => {
    expect(buildTree([])).toEqual([]);
  });

  it("groups children under parents", () => {
    const t = buildTree([
      doc("a", null, "m"),
      doc("b", "a", "m"),
      doc("c", "a", "n"),
    ]);
    expect(t).toHaveLength(1);
    expect(t[0]!.id).toBe("a");
    expect(t[0]!.children.map((n) => n.id)).toEqual(["b", "c"]);
  });

  it("sorts siblings by sort_key", () => {
    const t = buildTree([
      doc("a", null, "n"),
      doc("b", null, "m"),
      doc("c", null, "z"),
    ]);
    expect(t.map((n) => n.id)).toEqual(["b", "a", "c"]);
  });

  it("treats orphans (parent missing) as top-level", () => {
    const t = buildTree([
      doc("a", null, "m"),
      doc("b", "missing-parent", "m"),
    ]);
    expect(t.map((n) => n.id).sort()).toEqual(["a", "b"]);
  });
});

function countNodes(nodes: ReturnType<typeof buildTree>): number {
  return nodes.reduce((acc, n) => acc + 1 + countNodes(n.children), 0);
}

describe("buildTree totality (no doc ever vanishes)", () => {
  it("keeps every doc for a self-loop", () => {
    expect(countNodes(buildTree([doc("a", "a", "m"), doc("b", null, "n")]))).toBe(2);
  });
  it("keeps every doc for a 2-cycle", () => {
    expect(countNodes(buildTree([doc("a", "b", "m"), doc("b", "a", "n")]))).toBe(2);
  });
  it("keeps every doc for a 3-cycle with a child", () => {
    const t = buildTree([doc("a", "b", "m"), doc("b", "c", "n"), doc("c", "a", "o"), doc("d", "a", "p")]);
    expect(countNodes(t)).toBe(4);
    const ids = new Set<string>();
    (function walk(ns: typeof t) { ns.forEach((n) => { ids.add(n.id); walk(n.children); }); })(t);
    expect(ids.size).toBe(4);
  });
  it("keeps a doc whose parent is missing", () => {
    expect(countNodes(buildTree([doc("a", "ghost", "m")]))).toBe(1);
  });
});

describe("dropIntent", () => {
  const rect = { top: 100, height: 40 };
  it("top quarter → before", () => { expect(dropIntent(105, rect)).toBe("before"); });
  it("middle → into", () => { expect(dropIntent(120, rect)).toBe("into"); });
  it("bottom quarter → after", () => { expect(dropIntent(135, rect)).toBe("after"); });
});

describe("descendantIds", () => {
  it("collects descendants and is cycle-safe", () => {
    const docs = [doc("a", null, "m"), doc("b", "a", "m"), doc("c", "b", "m")];
    expect([...descendantIds(docs, "a")].sort()).toEqual(["b", "c"]);
    expect(() => descendantIds([doc("a", "b", "m"), doc("b", "a", "m")], "a")).not.toThrow();
  });
});

describe("applyOptimisticMove", () => {
  it("nests a doc under a new parent (into)", () => {
    const next = applyOptimisticMove([doc("a", null, "a"), doc("b", null, "b")], "b", { parent_id: "a" });
    const t = buildTree(next);
    expect(t).toHaveLength(1);
    expect(t[0]!.children.map((n) => n.id)).toEqual(["b"]);
  });
  it("reorders a sibling before another", () => {
    const docs = [doc("a", null, "a"), doc("b", null, "b"), doc("c", null, "c")];
    expect(buildTree(applyOptimisticMove(docs, "c", { parent_id: null, before_id: "a" })).map((n) => n.id)).toEqual(["c", "a", "b"]);
  });
  it("reorders a sibling after another", () => {
    const docs = [doc("a", null, "a"), doc("b", null, "b"), doc("c", null, "c")];
    expect(buildTree(applyOptimisticMove(docs, "a", { parent_id: null, after_id: "b" })).map((n) => n.id)).toEqual(["b", "a", "c"]);
  });
});

describe("moveArgs", () => {
  it("returns parent_id null when no target", () => {
    expect(moveArgs(null, "after")).toEqual({ parent_id: null });
  });
  it("before puts before_id", () => {
    const t = doc("a", "p", "m");
    expect(moveArgs(t, "before")).toEqual({ parent_id: "p", before_id: "a" });
  });
  it("after puts after_id", () => {
    const t = doc("a", "p", "m");
    expect(moveArgs(t, "after")).toEqual({ parent_id: "p", after_id: "a" });
  });
  it("into puts parent_id only", () => {
    const t = doc("a", "p", "m");
    expect(moveArgs(t, "into")).toEqual({ parent_id: "a" });
  });
});

// ---- Projection model (P11) ------------------------------------------------

/** The sidebar's rows for `docs`, as DocTree draws them. */
function rows(docs: Doc[], collapsed: string[] = [], activeId?: string): FlatRow[] {
  return flattenVisible(buildTree(docs), new Set(collapsed), activeId);
}

const byKey = (a: Doc, b: Doc) => (a.sort_key < b.sort_key ? -1 : a.sort_key > b.sort_key ? 1 : 0);

/** The children of every real parent, in order ("" is the top level). */
type Order = Record<string, string[]>;

function realOrder(docs: Doc[]): Order {
  const out: Order = {};
  [...docs].sort(byKey).forEach((d) => (out[d.parent_id ?? ""] ??= []).push(d.id));
  return out;
}

const show = (o: Order) => JSON.stringify(Object.keys(o).sort().map((k) => [k, o[k]]));

/** What the server does with POST /api/docs/:id/move (Task 10): the parent
 *  is a listed page (404 otherwise) and not the page or inside it (409
 *  doc.move_cycle); the anchor is a sibling under that parent and not the
 *  page itself (422 doc.bad_anchor); no anchor means last. */
function serverMove(
  docs: Doc[],
  id: string,
  args: { parent_id?: string | null; before_id?: string; after_id?: string },
): Order {
  const parent = args.parent_id ?? null;
  if (parent !== null && !docs.some((d) => d.id === parent)) throw new Error(`404 parent ${parent}`);
  if (parent === id || (parent !== null && descendantIds(docs, id).has(parent))) {
    throw new Error(`409 doc.move_cycle (${parent})`);
  }
  const order = realOrder(docs.filter((d) => d.id !== id));
  const sibs = (order[parent ?? ""] ??= []);
  const anchor = args.after_id ?? args.before_id;
  if (anchor !== undefined && !sibs.includes(anchor)) throw new Error(`422 doc.bad_anchor (${anchor})`);
  const at =
    args.after_id !== undefined ? sibs.indexOf(args.after_id) + 1
    : args.before_id !== undefined ? sibs.indexOf(args.before_id)
    : sibs.length;
  sibs.splice(at, 0, id);
  return order;
}

// P ─ P1, P2 ; Q ; R. The prototype's tree, plus R, so a drop at depth 0
// below P's last child is a real move for R (for Q it is a drop in place).
const proto = [
  doc("P", null, "a"),
  doc("P1", "P", "a"),
  doc("P2", "P", "b"),
  doc("Q", null, "b"),
  doc("R", null, "c"),
];

// A ─ A1, A2 ─ A2a, A3 ; B ; C ─ C1 ; D
const tree1 = [
  doc("A", null, "a"),
  doc("A1", "A", "a"),
  doc("A2", "A", "b"),
  doc("A2a", "A2", "a"),
  doc("A3", "A", "c"),
  doc("B", null, "b"),
  doc("C", null, "c"),
  doc("C1", "C", "a"),
  doc("D", null, "d"),
];

// The sidebar draws pages whose parent isn't listed at the top level: T1 and
// T2 are a template's children (templates aren't listed), Z is an alive
// child of an archived page. Their rendered parent is null, their real one
// is not.
const messy = [
  doc("A", null, "a"),
  doc("A1", "A", "a"),
  doc("T1", "T", "b"),
  doc("T1a", "T1", "a"),
  doc("B", null, "c"),
  doc("Z", "archived", "d"),
  doc("T2", "T", "e"),
];

describe("INDENT", () => {
  it("is the row indent per level (paddingLeft: 4 + depth*12)", () => {
    expect(INDENT).toBe(12);
  });
});

describe("countDescendants", () => {
  it("counts subpages at any depth", () => {
    const [a, b] = buildTree(tree1);
    expect(countDescendants(a!)).toBe(4);
    expect(countDescendants(b!)).toBe(0);
  });
});

describe("rowDescription", () => {
  const row = (depth: number, descendantCount: number): FlatRow =>
    ({ id: "x", parentId: null, depth, title: "x", descendantCount });
  it.each([
    [0, 0, "level 1"],
    [1, 1, "level 2, 1 subpage"],
    [1, 3, "level 2, 3 subpages"],
  ])("depth %i with %i descendants → %s", (depth, n, want) => {
    expect(rowDescription(row(depth, n))).toBe(want);
  });
});

describe("flattenVisible", () => {
  it("lists rows in screen order with depth, rendered parent, title and descendant count", () => {
    expect(rows(tree1)).toEqual([
      { id: "A", parentId: null, depth: 0, title: "A", descendantCount: 4 },
      { id: "A1", parentId: "A", depth: 1, title: "A1", descendantCount: 0 },
      { id: "A2", parentId: "A", depth: 1, title: "A2", descendantCount: 1 },
      { id: "A2a", parentId: "A2", depth: 2, title: "A2a", descendantCount: 0 },
      { id: "A3", parentId: "A", depth: 1, title: "A3", descendantCount: 0 },
      { id: "B", parentId: null, depth: 0, title: "B", descendantCount: 0 },
      { id: "C", parentId: null, depth: 0, title: "C", descendantCount: 1 },
      { id: "C1", parentId: "C", depth: 1, title: "C1", descendantCount: 0 },
      { id: "D", parentId: null, depth: 0, title: "D", descendantCount: 0 },
    ]);
  });
  it("leaves out the children of collapsed rows but still counts them", () => {
    const f = rows(tree1, ["A2", "C"]);
    expect(f.map((r) => r.id)).toEqual(["A", "A1", "A2", "A3", "B", "C", "D"]);
    expect(f.find((r) => r.id === "C")!.descendantCount).toBe(1);
  });
  it("leaves out the dragged row's subtree but keeps the row", () => {
    expect(rows(tree1, [], "A").map((r) => r.id)).toEqual(["A", "B", "C", "C1", "D"]);
  });
  it("gives pages drawn at the top level the rendered parent null", () => {
    expect(rows(messy).map((r) => [r.id, r.parentId, r.depth])).toEqual([
      ["A", null, 0],
      ["A1", "A", 1],
      ["T1", null, 0],
      ["T1a", "T1", 1],
      ["B", null, 0],
      ["Z", null, 0],
      ["T2", null, 0],
    ]);
  });
});

describe("dropZone", () => {
  const rect = { top: 100, height: 40 };
  it("splits another row into above, into and below", () => {
    expect(dropZone(105, rect, false)).toBe("above");
    expect(dropZone(120, rect, false)).toBe("into");
    expect(dropZone(135, rect, false)).toBe("below");
  });
  it("never yields into on one's own row", () => {
    for (let y = 100; y <= 140; y += 2) expect(dropZone(y, rect, true)).not.toBe("into");
    expect(dropZone(105, rect, true)).toBe("above");
    expect(dropZone(135, rect, true)).toBe("below");
  });
});

describe("projectDrop: prototype cases", () => {
  it("un-nests the last child of the last root in place by dragging left", () => {
    const docs = [doc("A", null, "m"), doc("B", "A", "m")];
    const f = rows(docs, [], "B");
    expect(projectDrop(f, docs, "B", "B", "below", -1)?.args).toEqual({ parent_id: null, after_id: "A" });
    expect(projectDrop(f, docs, "B", "A", "below", -1)?.args).toEqual({ parent_id: null, after_id: "A" });
    // No horizontal move: B stays where it is. The prototype sent
    // { parent_id: "A" } here; P11 makes it a no-op.
    expect(projectDrop(f, docs, "B", "B", "below", 0)).toBeNull();
  });

  it("nests by dragging right, clamped to the previous row's depth + 1", () => {
    const docs = [doc("A", null, "a"), doc("B", null, "b")];
    const f = rows(docs, [], "B");
    expect(projectDrop(f, docs, "B", "B", "below", 5)).toEqual({
      depth: 1,
      minDepth: 0,
      maxDepth: 1,
      args: { parent_id: "A" },
    });
  });

  it("makes the gap right below an expanded parent its first child", () => {
    const f = rows(proto, [], "Q");
    expect(projectDrop(f, proto, "Q", "P", "below", -3)).toEqual({
      depth: 1,
      minDepth: 1,
      maxDepth: 1,
      args: { parent_id: "P", before_id: "P1" },
    });
  });

  it("reaches depths 0..2 in the gap below a parent's last child", () => {
    const f = rows(proto, [], "R");
    const at = (d: number) => projectDrop(f, proto, "R", "P2", "below", d);
    expect(at(0)).toEqual({ depth: 0, minDepth: 0, maxDepth: 2, args: { parent_id: null, after_id: "P" } });
    expect(at(1)?.args).toEqual({ parent_id: "P", after_id: "P2" });
    expect(at(2)?.args).toEqual({ parent_id: "P2" });
    // Q already sits right after P: depth 0 there is a drop in place.
    const fq = rows(proto, [], "Q");
    expect(projectDrop(fq, proto, "Q", "P2", "below", -1)).toBeNull();
    expect(projectDrop(fq, proto, "Q", "P2", "below", 1)?.args).toEqual({ parent_id: "P", after_id: "P2" });
  });

  it("collapsed parent: the gap below it picks between after it and into it", () => {
    const f = rows(proto, ["P"], "R");
    expect(f.map((r) => r.id)).toEqual(["P", "Q", "R"]);
    expect(projectDrop(f, proto, "R", "P", "below", 0)?.args).toEqual({ parent_id: null, after_id: "P" });
    expect(projectDrop(f, proto, "R", "P", "below", 1)?.args).toEqual({ parent_id: "P" });
    expect(projectDrop(f, proto, "R", "P", "into", 0)?.args).toEqual({ parent_id: "P" });
  });

  it("leaves the dragged subtree out, so a page can't go inside itself", () => {
    const f = rows(proto, [], "P");
    expect(f.map((r) => r.id)).toEqual(["P", "Q", "R"]);
    expect(projectDrop(f, proto, "P", "P1", "into", 0)).toBeNull();
    expect(projectDrop(f, proto, "P", "P2", "below", 0)).toBeNull();
    // Even when handed rows that still hold the subtree.
    const full = rows(proto);
    expect(projectDrop(full, proto, "P", "P1", "into", 0)).toBeNull();
    expect(projectDrop(full, proto, "P", "P", "below", 1)).toBeNull();
  });

  it("a non-last child can't climb above the next row's depth", () => {
    const f = rows(proto, [], "R");
    expect(projectDrop(f, proto, "R", "P1", "below", -5)).toEqual({
      depth: 1,
      minDepth: 1,
      maxDepth: 2,
      args: { parent_id: "P", after_id: "P1" },
    });
    // P1 dragged left on its own row stays a child of P: nothing to send.
    expect(projectDrop(rows(proto, [], "P1"), proto, "P1", "P1", "below", -1)).toBeNull();
  });
});

describe("projectDrop: edge cases", () => {
  const abc = [doc("A", null, "a"), doc("B", null, "b"), doc("C", null, "c")];

  it("returns null for a drop in place", () => {
    const f = rows(abc, [], "B");
    expect(projectDrop(f, abc, "B", "B", "above", 0)).toBeNull();
    expect(projectDrop(f, abc, "B", "B", "below", 0)).toBeNull();
    expect(projectDrop(f, abc, "B", "C", "above", 0)).toBeNull();
    expect(projectDrop(f, abc, "B", "A", "below", 0)).toBeNull();
    expect(projectDrop(f, abc, "B", "B", "into", 0)).toBeNull();
  });

  it("returns null for the same real parent and position", () => {
    // A, T1 (a template's child), B: B dropped above T1 anchors after A,
    // which is where B already is among its real siblings.
    const docs = [doc("A", null, "a"), doc("T1", "T", "b"), doc("B", null, "c")];
    expect(projectDrop(rows(docs, [], "B"), docs, "B", "T1", "above", 0)).toBeNull();
    // Into the parent it already is the last child of.
    expect(projectDrop(rows(proto, [], "P2"), proto, "P2", "P", "into", 0)).toBeNull();
  });

  it.each([
    ["its own row", "T1", "below", []],
    ["the next row's above gap", "B", "above", []],
    ["the previous row's below gap", "A1", "below", []],
    ["the previous row's below gap, collapsed", "A", "below", ["A"]],
  ] satisfies [string, string, Zone, string[]][])("returns null for a template's child dropped in place: %s", (_name, overId, zone, collapsed) => {
    // Sending this would move T1 out of its template.
    const docs = [doc("A", null, "a"), doc("A1", "A", "a"), doc("T1", "T", "b"), doc("B", null, "c")];
    expect(projectDrop(rows(docs, collapsed, "T1"), docs, "T1", overId, zone, 0)).toBeNull();
  });

  it("anchors a drop next to a template's child on a real sibling, or falls back to last", () => {
    // Below T1: the nearest real sibling before the gap is A, not T1.
    let docs = [doc("A", null, "a"), doc("T1", "T", "b"), doc("B", null, "c"), doc("C", "B", "a")];
    expect(projectDrop(rows(docs, [], "C"), docs, "C", "T1", "below", -1)?.args)
      .toEqual({ parent_id: null, after_id: "A" });
    // Above T1 at the very top: none before, so the nearest real one after.
    docs = [doc("T1", "T", "a"), doc("A", null, "b"), doc("B", null, "c")];
    expect(projectDrop(rows(docs, [], "B"), docs, "B", "T1", "above", 0)?.args)
      .toEqual({ parent_id: null, before_id: "A" });
    // Only template children at the top level: no real sibling, so last.
    docs = [doc("T1", "T", "a"), doc("X", "T1", "a"), doc("T2", "T", "b")];
    expect(projectDrop(rows(docs, [], "X"), docs, "X", "X", "below", -1)?.args)
      .toEqual({ parent_id: null });
  });

  it("into appends as the last child", () => {
    const p = projectDrop(rows(proto, [], "Q"), proto, "Q", "P", "into", 0);
    expect(p).toEqual({ depth: 1, minDepth: 1, maxDepth: 1, args: { parent_id: "P" } });
    expect(serverMove(proto, "Q", p!.args)["P"]).toEqual(["P1", "P2", "Q"]);
  });
});

describe("keyboardMove", () => {
  const km = (id: string, cmd: "up" | "down" | "indent" | "outdent", flat = rows(tree1)) =>
    keyboardMove(flat, tree1, id, cmd);

  it("does nothing past the first and last rows", () => {
    expect(km("A", "up")).toBeNull();
    expect(km("D", "down")).toBeNull();
    expect(km("A1", "up")).toBeNull();
    expect(km("A3", "down")).toBeNull();
  });
  it("moves past the previous or next sibling, with its subtree", () => {
    expect(km("B", "up")).toEqual({ parent_id: null, before_id: "A" });
    expect(km("A", "down")).toEqual({ parent_id: null, after_id: "B" });
    expect(km("A2", "up")).toEqual({ parent_id: "A", before_id: "A1" });
    // Past A2's children, not into A2.
    expect(km("A1", "down")).toEqual({ parent_id: "A", after_id: "A2" });
  });
  it("passes over pages drawn at the top level whose parent isn't listed", () => {
    // A, T1 (a template's child), B: passing T1 alone changes no real order,
    // so up and down go on to the next sibling, as dragging B above A does.
    const docs = [doc("A", null, "a"), doc("T1", "T", "b"), doc("B", null, "c")];
    expect(keyboardMove(rows(docs), docs, "B", "up")).toEqual({ parent_id: null, before_id: "A" });
    expect(keyboardMove(rows(docs), docs, "A", "down")).toEqual({ parent_id: null, after_id: "B" });
    // messy: A ─ A1 ; T1 ─ T1a ; B ; Z ; T2. Between A and B lies T1 with its
    // subpage; after B lie only such pages, so B is already last.
    for (const collapsed of [[], ["T1"]]) {
      const f = rows(messy, collapsed);
      expect(keyboardMove(f, messy, "B", "up")).toEqual({ parent_id: null, before_id: "A" });
      expect(keyboardMove(f, messy, "A", "down")).toEqual({ parent_id: null, after_id: "B" });
      expect(keyboardMove(f, messy, "B", "down")).toBeNull();
      expect(keyboardMove(f, messy, "A", "up")).toBeNull();
    }
  });
  it("indents into the previous sibling as its last child", () => {
    expect(km("B", "indent")).toEqual({ parent_id: "A", after_id: "A3" });
    expect(km("A3", "indent")).toEqual({ parent_id: "A2", after_id: "A2a" });
    expect(km("A3", "indent", rows(tree1, ["A2"]))).toEqual({ parent_id: "A2" });
  });
  it("can't indent without a previous sibling", () => {
    expect(km("A", "indent")).toBeNull();
    expect(km("A1", "indent")).toBeNull();
  });
  it("outdents to right after the parent", () => {
    expect(km("A2", "outdent")).toEqual({ parent_id: null, after_id: "A" });
    expect(km("A3", "outdent")).toEqual({ parent_id: null, after_id: "A" });
    expect(km("A2a", "outdent")).toEqual({ parent_id: "A", after_id: "A2" });
  });
  it("can't outdent at the top level", () => {
    expect(km("A", "outdent")).toBeNull();
    expect(km("D", "outdent")).toBeNull();
  });
});

describe("describeMove", () => {
  it("names the page, its new parent and its position, by title", () => {
    const f = rows(tree1);
    expect(describeMove(f, "B", { parent_id: "A", after_id: "A1" })).toBe("Moved “B” into “A”, position 2 of 4");
    expect(describeMove(f, "D", { parent_id: "C" })).toBe("Moved “D” into “C”, position 2 of 2");
    expect(describeMove(f, "D", { parent_id: "B" })).toBe("Moved “D” into “B”, position 1 of 1");
    expect(describeMove(f, "A2", { parent_id: null, after_id: "A" })).toBe("Moved “A2” to the top level, position 2 of 5");
    expect(describeMove(f, "B", { parent_id: null, before_id: "A" })).toBe("Moved “B” to the top level, position 1 of 4");
  });
  it("leaves the position out when the new parent's children are hidden", () => {
    expect(describeMove(rows(tree1, ["C"]), "D", { parent_id: "C" })).toBe("Moved “D” into “C”");
  });
});

describe("projectDrop matches today's moveArgs for every gesture it supports", () => {
  /** The drag that asks for what today's `moveArgs(target, position)` does:
   *  "before" is the gap above the target and "after" the gap below its last
   *  visible row, both at the target's depth; "into" is the row's middle. */
  function gesture(
    f: FlatRow[],
    active: FlatRow,
    target: FlatRow,
    position: "before" | "after" | "into",
  ): [string, Zone, number] {
    if (position === "into") return [target.id, "into", 0];
    const delta = target.depth - active.depth;
    if (position === "before") return [target.id, "above", delta];
    let last = f.indexOf(target);
    while ((f[last + 1]?.depth ?? -1) > target.depth) last++;
    return [f[last]!.id, "below", delta];
  }

  it.each([[[]], [["A"]], [["A2"]], [["C"]]])("collapsed %j", (collapsed: string[]) => {
    const mismatches: string[] = [];
    for (const active of tree1) {
      const f = rows(tree1, collapsed, active.id);
      const a = f.find((r) => r.id === active.id);
      if (!a) continue; // hidden under a collapsed row: not draggable
      for (const target of f) {
        if (target.id === active.id) continue;
        const t = tree1.find((d) => d.id === target.id)!;
        for (const position of ["before", "after", "into"] as const) {
          const [overId, zone, depthDelta] = gesture(f, a, target, position);
          const p = projectDrop(f, tree1, active.id, overId, zone, depthDelta);
          const today = show(serverMove(tree1, active.id, moveArgs(t, position)));
          const now = show(p ? serverMove(tree1, active.id, p.args) : realOrder(tree1));
          if (now !== today) {
            mismatches.push(`${active.id} ${position} ${target.id}: ${JSON.stringify(p?.args ?? null)}`);
          }
        }
      }
    }
    expect(mismatches).toEqual([]);
  });
});

describe("projections only send valid, real moves", () => {
  // Every drag and keyboard move gives null, or args the server accepts that
  // change the real order: a no-op never POSTs.
  it.each([["tree1", tree1], ["proto", proto], ["messy", messy]])("%s", (_name, docs: Doc[]) => {
    const bad: string[] = [];
    const check = (what: string, id: string, args: MoveArgs | null) => {
      if (!args) return;
      try {
        if (show(serverMove(docs, id, args)) === show(realOrder(docs))) {
          bad.push(`${what}: no-op ${JSON.stringify(args)}`);
        }
      } catch (e) {
        bad.push(`${what}: ${(e as Error).message} ${JSON.stringify(args)}`);
      }
    };
    const ids = docs.map((d) => d.id);
    for (const collapsed of [[], ...ids.map((id) => [id])]) {
      for (const active of ids) {
        const f = rows(docs, collapsed, active);
        if (!f.some((r) => r.id === active)) continue;
        for (const over of f) {
          for (const zone of ["above", "below", "into"] as const) {
            for (let delta = -3; delta <= 3; delta++) {
              const p = projectDrop(f, docs, active, over.id, zone, delta);
              check(`[${collapsed.join()}] ${active} ${zone} ${over.id} ${delta}`, active, p?.args ?? null);
            }
          }
        }
        for (const cmd of ["up", "down", "indent", "outdent"] as const) {
          const args = keyboardMove(rows(docs, collapsed), docs, active, cmd);
          check(`[${collapsed.join()}] ${active} ${cmd}`, active, args);
        }
      }
    }
    expect(bad).toEqual([]);
  });
});

describe("keyboard up and down can swap any two adjacent real siblings", () => {
  // Every reorder is reachable by keyboard (WCAG 2.5.7): for each pair of
  // adjacent real siblings X, Y that the sidebar draws as siblings, "Y up"
  // and "X down" each give exactly the order with X and Y swapped, even with
  // pages drawn at the top level (a template's child) between them.
  const interleaved = [doc("A", null, "a"), doc("T1", "T", "b"), doc("B", null, "c")];
  it.each([["tree1", tree1], ["proto", proto], ["messy", messy], ["A,T1,B", interleaved]])(
    "%s",
    (_name, docs: Doc[]) => {
      const bad: string[] = [];
      const order = realOrder(docs);
      const ids = docs.map((d) => d.id);
      for (const collapsed of [[], ...ids.map((id) => [id])]) {
        const f = rows(docs, collapsed);
        const shown = new Set(f.map((r) => r.id));
        for (const [parent, sibs] of Object.entries(order)) {
          // Only the top level and listed, expanded parents: the children of
          // an unlisted parent (T, "archived") aren't drawn as its children.
          if (parent !== "" && !shown.has(parent)) continue;
          if (!sibs.every((id) => shown.has(id))) continue;
          for (let i = 0; i + 1 < sibs.length; i++) {
            const [x, y] = [sibs[i]!, sibs[i + 1]!];
            const want = show({ ...order, [parent]: sibs.map((id, j) => (j === i ? y : j === i + 1 ? x : id)) });
            for (const [id, cmd] of [[y, "up"], [x, "down"]] as const) {
              const args = keyboardMove(f, docs, id, cmd);
              const got = args ? show(serverMove(docs, id, args)) : "null";
              if (got !== want) bad.push(`[${collapsed.join()}] ${id} ${cmd}: ${JSON.stringify(args)}`);
            }
          }
        }
      }
      expect(bad).toEqual([]);
    },
  );
});
