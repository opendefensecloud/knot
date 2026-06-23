# Doc-tree reorder: robustness + visual drag — design (item A)

**Date:** 2026-06-23
**Status:** Approved (brainstorm)

## Problem

Dragging documents in the sidebar tree (`web/src/features/docs/DocTree.tsx`) is
broken in three compounding ways:

1. **Documents vanish.** Dropping a doc onto a row sets `parent_id = target` with
   **no cycle check** (frontend `DocTree.tsx:105`, backend `routes/api/docs.rs:265`).
   Dropping a doc onto one of its own descendants creates a parent-loop
   (A→C→…→A). The rows still exist in Postgres, but `buildTree` (`tree.ts:8`) can
   never reach a cyclic island from the roots, so the whole loop disappears from the
   sidebar — and refresh does not recover it because the loop persists. **The data
   is intact; the UI hides it.**
2. **Reordering is finicky and can't actually reorder.** Every drop nests
   (`parent_id = target`); there is no sibling-reorder gesture. It uses dnd-kit's
   flat-list `verticalListSortingStrategy` + `closestCenter` on a *nested* tree, with
   no drop indicator, so intent is ambiguous.
3. **Order/optimistic mismatch.** The optimistic update (`reorderInto`, `tree.ts:29`)
   changes only `parent_id`, never `sort_key`; the server computes a fresh
   `sort_key` via `sort_key_between`. The optimistic tree and the server disagree
   until refetch.

## Goal

No document can ever disappear from the sidebar due to a move; reordering supports
both **sibling reorder** and **nest/un-nest** via a clear Notion-style drag with
drop indicators; already-orphaned docs are healed.

## Data model (current)

- `documents.parent_id uuid NULL` (FK → documents, `ON DELETE CASCADE`),
  `documents.sort_key text` (LexoRank-style), `UNIQUE (workspace_id, parent_id,
  sort_key)` (`migrations/20260602000001_v0_1_schema.sql:58`).
- `list_alive` returns all non-archived, non-template docs `ORDER BY parent_id NULLS
  FIRST, sort_key` (`doc_store.rs:146`). The flat list is shaped into a tree on the
  client by `buildTree`.
- Backend move (`routes/api/docs.rs:265` → `doc_store.rs::move_to`) already accepts
  `{ parent_id, before_id?, after_id? }` and computes the new `sort_key` from the
  destination siblings. A `descendant_ids()`-style recursive query already exists
  (used by subtree export).

## Design

The work splits into **data-integrity** (must-have, fixes vanish) and **drag UX**
(the visible improvement). Integrity is defense-in-depth at three layers so a single
bug can never hide a doc.

### 1. Backend: reject moves that create a cycle

In the move handler (`routes/api/docs.rs`), before persisting, reject when
`new_parent == doc_id` **or** `new_parent` is a descendant of `doc_id`. Implement the
check authoritatively inside the move transaction with a recursive CTE (walk
ancestors of `new_parent`; if `doc_id` appears, it's a cycle), reusing the existing
recursive-descendant machinery in `doc_store.rs`. On violation return
`409 doc.move_cycle`. This is the primary guarantee: the DB can never hold a cycle.

### 2. Backend: heal pre-existing cycles (migration)

Add a forward migration that breaks any cycle already in the data: for every doc that
is its own ancestor, set `parent_id = NULL` (promote to root). Idempotent and safe —
it only touches genuinely cyclic rows. This makes already-vanished docs reappear at
the top level on next load. Implemented as a recursive-CTE `UPDATE`.

### 3. Frontend: `buildTree` can never drop a node

Make `buildTree` total: every input doc appears exactly once in the output tree.
After the normal parent-linking pass, detect any node **not reachable from a real
root** (cycle members, or a node whose `parent_id` points outside the set) and
**promote it to a root** instead of silently dropping it. Add a dev-only
`console.warn` when this happens (it signals data that the backend guards should have
prevented). This is the UI safety net: even with corrupt data, nothing vanishes.

### 4. Frontend: drop intent + drag UX (Notion-style)

Replace the flat-list sortable with explicit drop-intent computed from the pointer
position over the hovered row:

- pointer in the **top ~25%** of a row → **insert before** it (sibling, same parent);
- pointer in the **bottom ~25%** → **insert after** it (sibling, same parent);
- pointer in the **middle ~50%** → **nest as child** (append into the hovered row).

Render a **drop indicator**: a 2px accent insertion line (indented to the destination
depth) for before/after, or a full-row accent highlight for nest. Use dnd-kit's
pointer sensor (keep an activation distance to avoid accidental drags) with a custom
collision/over read; compute intent in the drag-over handler and stash it in state so
the indicator and the final drop agree.

Translate intent → move request:

- **nest** → `{ parent_id: hovered.id }` (server appends at end of new siblings);
- **before** → `{ parent_id: hovered.parent_id, before_id: hovered.id }`;
- **after** → `{ parent_id: hovered.parent_id, after_id: hovered.id }`.

**Frontend cycle guard (defense in depth):** before mutating, if the destination
parent is the moved node or one of its descendants (computed from the current tree),
**ignore the drop** (snap back, no request). The backend still enforces; this just
avoids a doomed request and a flash.

### 5. Frontend: correct optimistic update

Rework the optimistic step so the cached tree matches the intended result:

- set the moved node's `parent_id` to the destination parent;
- give it a **provisional `sort_key`** via a small local helper `midpointKey(before,
  after)` that returns a string strictly between its neighbors' keys (generic
  lexicographic midpoint — it only has to sort correctly locally; `onSettled`'s
  refetch replaces it with the server's authoritative key).
- `before`/`after` are the destination neighbors' `sort_key`s (or open-ended at list
  ends).

Keep `onError` rollback and `onSettled` invalidation. Because `buildTree` is now
total, even a wrong provisional key cannot hide a node — worst case is a momentary
order glitch corrected on refetch.

## Components / files

- `crates/knot-server/src/routes/api/docs.rs` — cycle check in move handler.
- `crates/knot-storage/src/doc_store.rs` — recursive ancestor/descendant check used
  by the move (reuse existing recursive query if present).
- `migrations/<ts>_heal_doc_cycles.sql` — break existing cycles.
- `web/src/features/docs/tree.ts` — total `buildTree`; `midpointKey`; a pure
  `dropIntent(pointerY, rowRect)` → `"before" | "after" | "inside"` helper;
  `ancestorIds`/`isDescendant` helper for the cycle guard; reworked optimistic
  transform.
- `web/src/features/docs/DocTree.tsx` — new drag handling, drop indicator rendering,
  intent→request translation.

## Testing (exhaustive — the core requirement)

**Unit (`web/src/features/docs/tree.test.ts`), pure functions:**
- `buildTree` totality: every doc in → exactly once out, for: normal tree; a
  self-loop (`parent_id == id`); a 2- and 3-node cycle; a node whose parent is absent.
  Assert output node count == input count in **every** case (the "no vanish"
  invariant).
- `dropIntent`: top/middle/bottom of a row rect → before/inside/after.
- `midpointKey`: result sorts strictly between neighbors; stable for adjacent keys.
- `isDescendant`/cycle guard: detects self and deep-descendant targets.

**Backend (`crates/knot-server/tests/docs_move_integration.rs`):**
- Reorder before/after a sibling updates order, returns 200, and `list_alive` still
  returns **all** docs.
- Move to a new parent (nest) and to root (un-nest) works; counts preserved.
- Move onto self → `409 doc.move_cycle`; move onto a descendant → `409`; the tree is
  unchanged and complete afterward.
- Healing migration: seed a cycle via direct SQL, run the heal query, assert the
  cyclic node is promoted to `parent_id IS NULL` and all docs are reachable.

**E2E (`e2e/flows/tree-reorder.spec.ts`), every scenario, asserting the visible row
count never drops:**
- reorder a sibling up and down; nest a doc under another; un-nest back to root;
  move across parents; attempt to drop a parent onto its own child (expect no-op /
  snap-back) — after each, assert all seeded docs are still visible in the tree.

## Risks / notes

- The provisional `midpointKey` only needs local correctness; server refetch is
  authoritative. We deliberately do **not** reimplement the server's exact LexoRank
  on the client (avoids drift).
- The heal migration is a one-time data repair; it runs everywhere but only changes
  genuinely cyclic rows, so it is a no-op on healthy databases.
- Out of scope: real-time propagation of another user's reorder to your sidebar
  (covered by the broader realtime story; here `onSettled` refetch is sufficient).
