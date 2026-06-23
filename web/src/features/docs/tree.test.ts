import { describe, expect, it } from "vitest";
import type { Doc } from "../../lib/validators";
import { buildTree, descendantIds, dropIntent, applyOptimisticMove, moveArgs } from "./tree";

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
