# Doc-tree Reorder Robustness + Visual Drag Implementation Plan (item A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** No document can vanish from the sidebar on reorder, and dragging supports clear sibling-reorder and nest/un-nest with Notion-style drop indicators.

**Architecture:** Defense-in-depth for data integrity (backend rejects cycle-creating moves inside the move transaction; a heal migration breaks any pre-existing cycle; `buildTree` becomes total so the UI can never drop a node) plus a reworked dnd-kit drag that computes drop intent from pointer position and uses the backend's existing `parent_id`/`before_id`/`after_id` move API. Optimistic updates reposition the moved node with a provisional `sort_key`; the server key is authoritative on refetch.

**Tech Stack:** Rust (axum, sqlx, recursive CTEs) backend; React + TS + @dnd-kit + TanStack Query frontend. Backend tests: `cargo nextest run -p knot-server` / `-p knot-storage` against dev-compose Postgres (use `knot_test_support::fresh_db`; never testcontainers). Frontend: `cd web && pnpm test` (vitest), `pnpm tsc --noEmit`. E2E: `cd e2e && pnpm playwright test`.

**Spec:** `docs/superpowers/specs/2026-06-23-doc-tree-reorder-robustness-design.md`

**Preconditions:** dev-compose Postgres healthy (`make compose.up`).

---

## File Structure

- Modify: `crates/knot-storage/src/doc_store.rs` — `DocStoreError::Cycle`; cycle check inside `move_to`'s transaction.
- Modify: `crates/knot-server/src/routes/api/docs.rs` — map `Cycle` → `409 doc.move_cycle`.
- Modify: `crates/knot-storage/tests/documents.rs` — append store-level move/cycle/heal tests (reuses the existing `setup()` helper).
- Create: `migrations/<ts>_heal_doc_cycles.sql` — break pre-existing cycles.
- Modify: `web/src/features/docs/tree.ts` — total `buildTree`; `dropIntent`; `descendantIds`; `applyOptimisticMove`.
- Modify: `web/src/features/docs/tree.test.ts` — totality + new-helper tests.
- Modify: `web/src/features/docs/DocTree.tsx` — pointer-intent drag, drop indicators, cycle guard, optimistic via `applyOptimisticMove`.
- Create: `e2e/flows/tree-reorder.spec.ts` — all drag scenarios; assert no row vanishes.

---

## Task 1: Backend — reject cycle-creating moves

**Files:**
- Modify: `crates/knot-storage/src/doc_store.rs`
- Modify: `crates/knot-server/src/routes/api/docs.rs`
- Test: `crates/knot-storage/tests/documents.rs` (append; reuse the file's existing `setup()` helper)

Context: `move_to` (`doc_store.rs:290`) begins a tx then `UPDATE documents SET parent_id, sort_key`. `DocStoreError` (`doc_store.rs:31`) has `Sqlx/NotFound/Conflict`. `DocStore::create(workspace_id, parent_id, title, sort_key, created_by)` and `move_to(workspace_id, doc_id, actor, parent_id, sort_key)` are the store methods. The move handler maps store errors at `docs.rs:326-336`. The existing `tests/documents.rs` already has `async fn setup() -> (PgDocStore, Uuid, Uuid)` and imports `DocStore, PgDocStore, ... sort_key_between`.

- [ ] **Step 1: Write the failing store tests**

Append to `crates/knot-storage/tests/documents.rs` (and add `DocStoreError` to the existing `use knot_storage::{...}` import list). Reuse the file's `setup()`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn move_to_descendant_is_rejected_as_cycle() {
    let (store, ws, u) = setup().await;
    let a = store.create(ws, None, "A", "m", u).await.unwrap();
    let b = store.create(ws, Some(a.id), "B", "m", u).await.unwrap();

    // Move A under B (its own child) → cycle.
    let err = store.move_to(ws, a.id, u, Some(b.id), "n").await.unwrap_err();
    assert!(matches!(err, DocStoreError::Cycle), "expected Cycle, got {err:?}");

    // Move A under itself → cycle.
    let err = store.move_to(ws, a.id, u, Some(a.id), "n").await.unwrap_err();
    assert!(matches!(err, DocStoreError::Cycle));

    // All docs still present.
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_reorder_and_nest_preserve_all_docs() {
    let (store, ws, u) = setup().await;
    let a = store.create(ws, None, "A", "a", u).await.unwrap();
    let b = store.create(ws, None, "B", "b", u).await.unwrap();
    let c = store.create(ws, None, "C", "c", u).await.unwrap();

    store.move_to(ws, c.id, u, Some(a.id), "m").await.unwrap(); // nest C under A
    store.move_to(ws, b.id, u, None, "z").await.unwrap();       // reorder B at root

    let all = store.list_alive(ws).await.unwrap();
    assert_eq!(all.len(), 3, "no doc lost");
    assert_eq!(all.iter().find(|d| d.id == c.id).unwrap().parent_id, Some(a.id));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p knot-storage --test doc_move_cycle`
Expected: FAIL — `DocStoreError::Cycle` doesn't exist (compile error) / cycle not rejected.

- [ ] **Step 3: Add the `Cycle` error variant**

In `crates/knot-storage/src/doc_store.rs`, add to the `DocStoreError` enum (after `Conflict`):

```rust
    #[error("cycle")]
    Cycle,
```

- [ ] **Step 4: Add the cycle check inside `move_to`**

In `move_to` (`doc_store.rs:298`), immediately after `let mut tx = self.pool.begin().await?;` and before the `UPDATE`, insert:

```rust
        // Reject moves that would create a parent cycle: the destination
        // parent must not be the doc itself or one of its descendants. Done
        // inside the tx so it's correct under concurrency.
        if let Some(p) = parent_id {
            let creates_cycle: bool = sqlx::query_scalar(
                "WITH RECURSIVE sub AS (
                     SELECT id FROM documents WHERE id = $1
                     UNION ALL
                     SELECT d.id FROM documents d JOIN sub s ON d.parent_id = s.id
                 )
                 SELECT EXISTS(SELECT 1 FROM sub WHERE id = $2)",
            )
            .bind(doc_id)
            .bind(p)
            .fetch_one(&mut *tx)
            .await?;
            if creates_cycle {
                return Err(DocStoreError::Cycle);
            }
        }
```

- [ ] **Step 5: Map the error in the move handler**

In `crates/knot-server/src/routes/api/docs.rs`, in `move_doc`'s match (after the `Conflict` arm, ~line 332), add:

```rust
        Err(knot_storage::DocStoreError::Cycle) => {
            json_err(StatusCode::CONFLICT, "doc.move_cycle", "")
        }
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo nextest run -p knot-storage --test doc_move_cycle`
Expected: PASS (2 tests).

- [ ] **Step 7: fmt + clippy**

Run: `cargo fmt -p knot-storage -p knot-server && cargo clippy -p knot-storage -p knot-server --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/knot-storage/src/doc_store.rs crates/knot-server/src/routes/api/docs.rs crates/knot-storage/tests/doc_move_cycle.rs
git commit -m "fix(docs): reject moves that create a parent cycle"
```

---

## Task 2: Backend — heal pre-existing cycles (migration)

**Files:**
- Create: `migrations/<ts>_heal_doc_cycles.sql`
- Modify: `crates/knot-storage/tests/doc_move_cycle.rs` (add heal test)

- [ ] **Step 1: Create the migration**

Generate a correctly-ordered filename: `make migrate.create NAME=heal_doc_cycles` (creates `migrations/<timestamp>_heal_doc_cycles.sql`). Replace its contents with:

```sql
-- heal_doc_cycles
-- Promote any document that is its own ancestor (a parent cycle) to the
-- workspace root by nulling its parent_id. Depth-capped so a pre-existing
-- cycle cannot loop forever. Idempotent; a no-op on healthy data.
WITH RECURSIVE anc(start, cur, depth) AS (
    SELECT id, parent_id, 1
    FROM documents
    WHERE parent_id IS NOT NULL
    UNION ALL
    SELECT a.start, d.parent_id, a.depth + 1
    FROM anc a
    JOIN documents d ON d.id = a.cur
    WHERE a.cur IS NOT NULL AND a.depth < 1000
)
UPDATE documents
SET parent_id = NULL, updated_at = now()
WHERE id IN (SELECT start FROM anc WHERE cur = start);
```

- [ ] **Step 2: Write the heal test**

Append to `crates/knot-storage/tests/doc_move_cycle.rs`. This injects a cycle with raw SQL (bypassing the move guard) then runs the identical heal statement and asserts the cycle members are promoted to roots:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn heal_query_promotes_cycle_members_to_root() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());
    let w = ws.create("default", "W").await.unwrap();
    let u = users.create_local("a@x.test", "U", "hash").await.unwrap();
    let docs = PgDocStore::new(pool.clone());
    let a = docs.create(w.id, None, "A", "a", u.id).await.unwrap();
    let b = docs.create(w.id, Some(a.id), "B", "b", u.id).await.unwrap();

    // Inject a cycle directly: A.parent = B (B is A's child) → A<->B loop.
    sqlx::query("UPDATE documents SET parent_id = $1 WHERE id = $2")
        .bind(b.id)
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();

    // Run the heal statement (same SQL as the migration).
    sqlx::query(
        "WITH RECURSIVE anc(start, cur, depth) AS (
             SELECT id, parent_id, 1 FROM documents WHERE parent_id IS NOT NULL
             UNION ALL
             SELECT a.start, d.parent_id, a.depth + 1
             FROM anc a JOIN documents d ON d.id = a.cur
             WHERE a.cur IS NOT NULL AND a.depth < 1000
         )
         UPDATE documents SET parent_id = NULL, updated_at = now()
         WHERE id IN (SELECT start FROM anc WHERE cur = start)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Both cycle members are now roots; all docs reachable, none lost.
    let all = docs.list_alive(w.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d.parent_id.is_none()));
}
```

- [ ] **Step 3: Run migration + test**

Run: `cargo nextest run -p knot-storage --test doc_move_cycle` (fresh_db applies the new migration first).
Expected: PASS (3 tests total).

- [ ] **Step 4: Commit**

```bash
git add migrations/ crates/knot-storage/tests/doc_move_cycle.rs
git commit -m "fix(docs): migration to heal pre-existing parent cycles"
```

---

## Task 3: Frontend — total buildTree + drag helpers (pure, tested)

**Files:**
- Modify: `web/src/features/docs/tree.ts`
- Test: `web/src/features/docs/tree.test.ts`

- [ ] **Step 1: Write the failing tests**

Add to `web/src/features/docs/tree.test.ts` (keep existing tests; add these imports/cases). Update the import line to:

```ts
import { buildTree, descendantIds, dropIntent, applyOptimisticMove, moveArgs } from "./tree";
```

Add a node-count helper and new describe blocks:

```ts
function countNodes(nodes: ReturnType<typeof buildTree>): number {
  return nodes.reduce((acc, n) => acc + 1 + countNodes(n.children), 0);
}

describe("buildTree totality (no doc ever vanishes)", () => {
  it("keeps every doc for a self-loop", () => {
    const docs = [doc("a", "a", "m"), doc("b", null, "n")];
    expect(countNodes(buildTree(docs))).toBe(2);
  });
  it("keeps every doc for a 2-cycle", () => {
    const docs = [doc("a", "b", "m"), doc("b", "a", "n")];
    expect(countNodes(buildTree(docs))).toBe(2);
  });
  it("keeps every doc for a 3-cycle with a child", () => {
    const docs = [doc("a", "b", "m"), doc("b", "c", "n"), doc("c", "a", "o"), doc("d", "a", "p")];
    const t = buildTree(docs);
    expect(countNodes(t)).toBe(4);
    const ids = new Set<string>();
    (function walk(ns: typeof t) { ns.forEach((n) => { ids.add(n.id); walk(n.children); }); })(t);
    expect(ids.size).toBe(4); // no duplicates
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
    const cyclic = [doc("a", "b", "m"), doc("b", "a", "m")];
    expect(() => descendantIds(cyclic, "a")).not.toThrow();
  });
});

describe("applyOptimisticMove", () => {
  it("nests a doc under a new parent (into)", () => {
    const docs = [doc("a", null, "a"), doc("b", null, "b")];
    const next = applyOptimisticMove(docs, "b", { parent_id: "a" });
    const t = buildTree(next);
    expect(t).toHaveLength(1);
    expect(t[0]!.children.map((n) => n.id)).toEqual(["b"]);
  });
  it("reorders a sibling before another", () => {
    const docs = [doc("a", null, "a"), doc("b", null, "b"), doc("c", null, "c")];
    const next = applyOptimisticMove(docs, "c", { parent_id: null, before_id: "a" });
    expect(buildTree(next).map((n) => n.id)).toEqual(["c", "a", "b"]);
  });
  it("reorders a sibling after another", () => {
    const docs = [doc("a", null, "a"), doc("b", null, "b"), doc("c", null, "c")];
    const next = applyOptimisticMove(docs, "a", { parent_id: null, after_id: "b" });
    expect(buildTree(next).map((n) => n.id)).toEqual(["b", "a", "c"]);
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd web && pnpm test tree`
Expected: FAIL — `descendantIds`/`dropIntent`/`applyOptimisticMove` not exported; totality cases fail.

- [ ] **Step 3: Implement the helpers**

In `web/src/features/docs/tree.ts`, replace `buildTree` with the total version and add the new helpers. Keep `moveArgs` as-is. Replace `reorderInto` (it is superseded by `applyOptimisticMove`; remove it and its old test block).

```ts
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
    if (import.meta.env?.DEV) {
      // eslint-disable-next-line no-console
      console.warn(`buildTree: promoted unreachable node ${node.id} (parent cycle?)`);
    }
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
```

- [ ] **Step 4: Run to verify pass + typecheck**

Run: `cd web && pnpm test tree && pnpm tsc --noEmit`
Expected: PASS. (If `reorderInto` is still imported anywhere besides DocTree, grep and remove; DocTree is updated in Task 4.)

- [ ] **Step 5: Commit**

```bash
git add web/src/features/docs/tree.ts web/src/features/docs/tree.test.ts
git commit -m "feat(tree): total buildTree + drop-intent/optimistic move helpers"
```

---

## Task 4: Frontend — pointer-intent drag with drop indicators

**Files:**
- Modify: `web/src/features/docs/DocTree.tsx`

Context: current drag uses per-row `useSortable` + `SortableContext` (`verticalListSortingStrategy`) + `closestCenter`, and `onDragEnd` always nests (`{ parent_id: targetId }`). The `move` mutation's `onMutate` uses `reorderInto`. We rework drag to compute intent and render an insertion line / nest highlight, and switch optimistic to `applyOptimisticMove`. **This task needs browser + e2e verification (Task 5); dnd-kit rect fields may need adjustment.**

- [ ] **Step 1: Update imports + optimistic mutation**

In `DocTree.tsx`:
- Import the new helpers: `import { buildTree, applyOptimisticMove, descendantIds, dropIntent, moveArgs, type TreeNode } from "./tree";` (merge with the existing `./tree` import; drop `reorderInto`).
- Add `DragOverEvent` to the `@dnd-kit/core` import.
- In the `move` mutation `onMutate`, replace `reorderInto(prev.ok, a.id, a.body.parent_id ?? null)` with `applyOptimisticMove(prev.ok, a.id, a.body)`.

- [ ] **Step 2: Add drop state + over/end handlers in `DocTree`**

Add state and handlers (replace the existing `onDragEnd`):

```tsx
  const [drop, setDrop] = useState<{ overId: string; intent: "before" | "after" | "into" } | null>(null);

  function onDragOver(e: DragOverEvent) {
    const over = e.over;
    const activeRect = e.active.rect.current.translated;
    if (!over || over.id === e.active.id || !activeRect) { setDrop(null); return; }
    const centerY = activeRect.top + activeRect.height / 2;
    const intent = dropIntent(centerY, { top: over.rect.top, height: over.rect.height });
    setDrop({ overId: String(over.id), intent });
  }

  function onDragEnd(e: DragEndEvent) {
    const d = drop;
    setDrop(null);
    if (!e.over || !d) return;
    const movedId = String(e.active.id);
    const targetId = String(e.over.id);
    if (movedId === targetId) return;
    const docs = list.data && "ok" in list.data ? list.data.ok : [];
    const target = docs.find((x) => x.id === targetId) ?? null;
    if (!target) return;
    const destParent = d.intent === "into" ? target.id : (target.parent_id ?? null);
    // Cycle guard (backend also enforces): can't move into self/descendant.
    if (destParent === movedId || (destParent && descendantIds(docs, movedId).has(destParent))) {
      notify("error", "Can't move a document inside itself");
      return;
    }
    move.mutate({ id: movedId, body: moveArgs(target, d.intent) });
  }
```

Wire them on `DndContext` and drop the flat-list strategy. Replace the `<DndContext ...>`/`<SortableContext ...>` opening with:

```tsx
        <DndContext
          sensors={sensors}
          collisionDetection={closestCenter}
          onDragOver={onDragOver}
          onDragEnd={onDragEnd}
          onDragCancel={() => setDrop(null)}
        >
          <SortableContext items={flatIds} strategy={verticalListSortingStrategy}>
```

(Keep `SortableContext`/`verticalListSortingStrategy` so each row keeps a stable `useSortable` drag handle; the visual indicator — not list re-sorting — communicates the drop. If rows visibly shift during drag in a distracting way, switch each `TreeRow` from `useSortable` to `useDraggable` + `useDroppable` and remove `SortableContext`; the e2e in Task 5 is the acceptance contract either way.)

- [ ] **Step 3: Pass drop hint into rows + render the indicator**

Thread `drop` into each `TreeRow` (both the top-level map and the recursive child render): add prop `dropHint={drop && drop.overId === n.id ? drop.intent : null}`.

In `TreeRow`'s props add `dropHint: "before" | "after" | "into" | null`. Wrap the row's clickable container so it is `relative`, and render the indicator:

```tsx
      {dropHint === "before" && (
        <span className="pointer-events-none absolute left-0 right-1 top-0 h-0.5 bg-accent rounded" />
      )}
      {dropHint === "after" && (
        <span className="pointer-events-none absolute left-0 right-1 bottom-0 h-0.5 bg-accent rounded" />
      )}
```

And add a nest highlight by including `dropHint === "into" ? "ring-1 ring-accent bg-accent/10" : ""` in the row container's className (the existing `group flex items-center ...` div). Ensure that div is `relative`.

- [ ] **Step 4: Typecheck + verify in the app**

Run: `cd web && pnpm tsc --noEmit` → PASS.
Then `make dev` and manually drag rows: dragging near a row's top/bottom shows the insertion line (reorder); dragging over the middle highlights it (nest). Reorder, nest, and un-nest should all work and persist after refresh.

- [ ] **Step 5: Commit**

```bash
git add web/src/features/docs/DocTree.tsx
git commit -m "feat(tree): pointer-intent drag with reorder/nest drop indicators"
```

---

## Task 5: E2E — every drag scenario, no doc vanishes

**Files:**
- Create: `e2e/flows/tree-reorder.spec.ts`

- [ ] **Step 1: Read the existing dnd e2e for the drag pattern**

Run: `sed -n '1,120p' e2e/flows/tree-dnd.spec.ts` (existing tree dnd test). Reuse its auth/setup helpers and its mouse-driven drag sequence (dnd-kit needs `mouse.move` in steps with intermediate points, not a single `dragTo`). Match the project's `data-testid` for rows (`doc-row-<id>`) and the new-doc flow.

- [ ] **Step 2: Write the spec**

Create `e2e/flows/tree-reorder.spec.ts`. Seed several docs, then for each scenario perform a drag and assert **every seeded doc is still visible** (`doc-row-<id>` present) afterward:

```ts
import { expect, test } from "@playwright/test";
// Reuse the SAME setup/auth helpers and the drag(...) routine from tree-dnd.spec.ts.

// Scenarios (each its own test, all asserting the visible row count is unchanged):
//  1. reorder a root sibling downward (drop on lower third of a sibling)
//  2. reorder a root sibling upward (drop on upper third of a sibling)
//  3. nest doc B under doc A (drop on middle of A); B appears under A
//  4. un-nest B back to root (drag B onto a root row's edge)
//  5. attempt to drop a parent onto its own child → no-op; both still visible
// After EVERY drag: assert all seeded doc rows are still in the DOM.
```

Implement each scenario using the drag helper. The non-negotiable assertion in every test: after the drag, `await expect(page.getByTestId(\`doc-row-${id}\`)).toBeVisible()` for **all** seeded ids (this is the "no document vanishes" guarantee). For scenario 5, also assert the parent's `parent_id` did not become its child (the row is still at its original level / still present).

- [ ] **Step 3: Run the spec**

Run: `cd e2e && pnpm playwright test tree-reorder`
Expected: PASS. If drag coordinates need tuning for the before/after/into zones, adjust the mouse target Y within the target row (top ~20%, middle ~50%, bottom ~80%).

- [ ] **Step 4: Commit**

```bash
git add e2e/flows/tree-reorder.spec.ts
git commit -m "test(e2e): exhaustive tree reorder scenarios; no doc vanishes"
```

---

## Task 6: Full verification

- [ ] **Step 1: Backend**

Run: `cargo nextest run -p knot-storage -p knot-server` then `cargo clippy -p knot-storage -p knot-server --all-targets -- -D warnings`
Expected: all tests pass (incl. `doc_move_cycle`), no warnings.

- [ ] **Step 2: Frontend**

Run: `cd web && pnpm test && pnpm tsc --noEmit`
Expected: all pass (incl. `tree` suite).

- [ ] **Step 3: E2E (affected)**

Run: `cd e2e && pnpm playwright test tree-reorder tree-dnd`
Expected: PASS. The old `tree-dnd` flow must still pass (or be updated if it asserted the old always-nest behavior — if so, reconcile it with the new intent model and note the change).

- [ ] **Step 4: Manual smoke**

`make dev`: drag to reorder, nest, un-nest; confirm the insertion line vs nest highlight; refresh and confirm order persists; attempt to drop a parent into its child and confirm it's refused with no doc disappearing.

---

## Self-Review notes

- **Spec coverage:** backend cycle rejection in the move tx (Task 1) ✓; heal migration (Task 2) ✓; total `buildTree` (Task 3) ✓; Notion-style drop-line + nest-zone drag (Task 4) ✓; correct optimistic update (Task 3 `applyOptimisticMove`, wired Task 4) ✓; exhaustive unit + backend + e2e tests asserting no vanish (Tasks 1–3,5) ✓.
- **Refinement vs spec:** the spec named a `midpointKey`; the plan uses the simpler **anchor-key + stable-sort** approach in `applyOptimisticMove` (provisional only; server authoritative on refetch) — same guarantee, less fragile. The cycle check lives inside `move_to`'s transaction (stronger than a handler-level check).
- **Naming consistency:** helper names `buildTree`/`dropIntent`/`descendantIds`/`applyOptimisticMove`/`moveArgs` and intent values `"before"|"after"|"into"` are identical across `tree.ts`, its tests, and `DocTree.tsx`. Error code `doc.move_cycle` and `DocStoreError::Cycle` consistent backend-side.
- **Risk:** Task 4 (dnd-kit wiring) and Task 5 (Playwright drag coordinates) are the iteration-prone parts; the no-vanish guarantee does not depend on them — it is locked by Tasks 1–3 and their headless tests.
