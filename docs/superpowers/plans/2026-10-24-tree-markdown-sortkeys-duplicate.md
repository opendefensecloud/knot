# Markdown Lists, Sort Keys, Sidebar Drag and Drop, Duplicate: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the four problems reported in PR #42 at the root: Markdown list round trips, sort keys, sidebar drag and drop, and Duplicate. Prove with a compatibility harness that existing pages only improve.

**Architecture:**
- **Markdown (`knot-markdown`).**
  - An event adapter restores tight-list paragraphs on import.
  - The serializer gets a shared container writer and a tolerant export that repairs damaged shapes.
  - Tasks are listed from the Y.Doc using the same walk as toggles.
  - A frozen copy of today's code drives a differential harness.
- **Sort keys (`knot-storage`).**
  - A fractional-index generator replaces the old one.
  - A heal migration fixes existing keys and makes top-level keys unique.
  - Placement moves into one in-transaction function that rebalances when needed.
- **Sidebar (`web`).** A projection model with pure functions in `tree.ts`, and explicit keyboard/menu moves.
- **Duplicate (server).**
  - A lossless CRDT state copy with remapped ids.
  - Boards and files are copied.
  - All rows are written in one transaction.

**Tech Stack:** Rust (yrs 0.27.3, sqlx, axum), Postgres 18, React + dnd-kit core 6.3.1, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-10-24-tree-markdown-sortkeys-duplicate-design.md`. Read it before any task, together with the "Planning decisions" section below. Problem ids (A1–A12, B1–B6, C1–C9, D) refer to the spec. Prototype paths below are absolute; they are reference material to port, never code to copy blindly.

## Global Constraints

- **Compatibility is the top constraint.**
  - A healthy page's stored content, sort order and Markdown output never change. Healthy means the legacy export `m` succeeds and `legacy_serialise(legacy_parse(m)) == m`.
  - Every change is either a named bug class or provably lossless.
- **Every bug gets a test that fails on main.** Each task's report shows the RED run (command plus failing assertion) before the fix.
  - A test marked *regression guard* covers a bug that an earlier task in this plan already fixed. Its report names that task, and it needs no RED run.
- **Existing tests stay unchanged,** except those listed in P6. Each change is justified in its commit message as encoding a bug or as a mechanical API adaptation.
- **Placement errors are JSON errors:**
  - `422 doc.bad_anchor` for an anchor that isn't a live, non-template sibling under the target parent, or that is the moved doc itself;
  - `409 doc.sort_key_conflict` (kept);
  - `409 doc.move_cycle` (kept);
  - `409 doc.archived` for an archived parent (kept).
- **Duplicate:**
  - `POST /api/docs/{id}/duplicate` with body `{ "include_subpages": bool }` returns `201` and a `DocResponse`;
  - `422 doc.is_template`;
  - `413 doc.duplicate_too_large` above 200 pages or 200 MiB of files;
  - `409 doc.duplicate_conflict` when the source was moved or archived, or its files changed, while copying;
  - the title is `"<title> (copy)"` for the top page only;
  - subpages are unticked by default.
- **Copy (exact):**
  - menu items "Duplicate…", "Move up", "Move down", "Indent", "Outdent";
  - dialog title `Duplicate “<title>”`, checkbox "Include its N subpages", hint "Every subpage is copied with all its content, drawings and files.", buttons "Cancel" and "Duplicate";
  - messages "Couldn't duplicate this page.", "This page is too large to duplicate (over 200 pages or 200 MB of files).", "Couldn't move";
  - screen-reader instructions "Drag to move. Use the row menu or Alt with the arrow keys to move by keyboard.";
  - row description `level <depth+1>` plus `, <n> subpage` / `, <n> subpages` when the row has descendants. `n` is the descendant count, the same number archive and Duplicate use (e.g. "level 2, 3 subpages").
- **Shared constants:**
  - `INDENT = 12` px, matching `paddingLeft: 4 + depth*12`, which stays;
  - `RENDERER_VERSION = 2`;
  - `MAX_DUPLICATE_PAGES = 200`;
  - `MAX_DUPLICATE_BLOB_BYTES = 200 * 1024 * 1024`;
  - sort-key alphabet `[0-9a-z]`, valid keys `^[0-9a-z]*[1-9a-z]$`, rebalance when a key would exceed 64 characters.
- **Repo checks:**
  - Rust: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo nextest run --workspace --all-features`, `cargo deny check`;
  - web: `pnpm tsc`, `pnpm lint`, `pnpm test`;
  - e2e: Playwright.
  - Rust integration tests use `KNOT_TEST_DATABASE_URL=postgres://knot:knot@127.0.0.1:5434/knot`.
- **Commits:** Conventional Commits with the scopes `markdown`, `tasks`, `sort-keys`, `tree` and `duplicate`. Commits are signed (1Password SSH agent). If signing fails, retry once after 30 s, then stop and report it. Never commit unsigned on this branch: the owner chose signed commits for it on 2026-10-24.
- **Docs:**
  - CHANGELOG:
    - Upgrading:
      - two migrations, one that repairs page order where needed and makes top-level order keys unique, and one that marks cached Markdown for a one-time background refresh;
      - while the rollout is in progress, old pods may reject some top-level creates and workspace imports with a conflict. Don't run workspace imports until every pod runs the new version;
    - Added: Duplicate, and Move/Indent menu items with Alt+arrow shortcuts;
    - Fixed: each area.
  - `.claude/skills/knot-api/SKILL.md`: the duplicate route and the move/create 422.
  - Follow-up issues listed in the PR: Markdown escaping, and the full ARIA tree model.

## Planning decisions (rulings on conflicts found during planning)

- **P1. Task text stays legacy-identical.**
  - The list of task items, their indexes and checked state come from the Y-walk (`knot_crdt::tasks::task_items`).
  - Text, assignee and due date come from running today's `extract_tasks(md)` (unchanged) over `to_markdown::task_item_markdown(txn, item)`: `"- [ ] "` followed by the item's own blocks, written by the new serializer. Leave out every block that is, or contains, a `list_item`: nested lists, and tables or quotes holding checklists. Take that call's first task. If it returns none, use empty text and no assignee or due date.
  - Named class `task_with_nested_list`: through the A4 bug, legacy took text, assignee and due date from nested-list content too. The class is exempt for text, and for assignee and due date when legacy's value came from nested-list content.
- **P2. Byte-identical cases the prototype broke stay identical:**
  - in a blockquote, a paragraph followed by an image, board or attachment keeps `> a\n> ![x](…)`;
  - empty items keep `- `, `- [ ] ` and `n. ` with the trailing space.
- **P3. Glue rule.**
  - A *run* is one or more consecutive inline nodes (text, hard_break) directly under a block container.
  - A chain `paragraph (run paragraph)+` becomes one paragraph. Its runs sit between the paragraphs' inline content.
  - A run next to only one paragraph, or alone, becomes its own paragraph.
  - Adjacent text with equal marks merges; `{}` counts as no mark.
  - Both crates implement the same helper, because knot-export must not depend on knot-markdown. A shared test-vector table (`[t,p]`→2, `[p,t,p]`→1, `[p,hb,p,hb,p]`→1, `[t]`→1, `[p,t]`→2, `[p,t,hb,p]`→1) lives in each crate's tests.
  - knot-export's existing `text_under_blockquote_is_wrapped` (`crates/knot-export/tests/walk.rs:390`) stays green unchanged.
- **P4. Unknown nodes degrade.**
  - Add `pub fn serialise_with_report(engine, doc) -> Result<(String, Vec<String>), SerError>`. The `Vec` holds unknown tag names.
  - `serialise` keeps its signature and discards the report.
  - The server warns and counts `knot_markdown_unknown_nodes_total`.
- **P5. `sort_key_between` becomes a `String`-returning function.**
  - `lib.rs:31` `pub use lexorank::between as sort_key_between;` is replaced by `pub fn sort_key_between(a: Option<&str>, b: Option<&str>) -> String { lexorank::between(a, b).expect("valid sort keys") }`.
  - The 12 existing test call sites stay unchanged.
  - Production code moves to placement (Task 10). Until then, `doc_store.rs:598` calls `crate::sort_key_between`.
  - `between(None, None)` returns `"m"`.
- **P6. Existing tests that change, and why:**
  - `crates/knot-storage/src/lexorank.rs` in-module tests: mechanical `.unwrap()` on `between` results (Task 8). `is_base36` may be replaced by `is_valid`.
  - `docs_integration.rs` `move_with_unknown_anchor_falls_to_end`: asserts `422 doc.bad_anchor` (Task 10).
  - `e2e/flows/tree-reorder.spec.ts` is rewritten, and `e2e/flows/tree-dnd.spec.ts` deleted (Task 15). They encoded the bug: a teleported pointer, and "no toast" as the only check.
  - `e2e/flows/sidebar-resize.spec.ts` (Task 13, amended during execution): `:215` waits for `[data-testid^='tree-row-']` instead of `li[role='button']`, which Task 13 removes (C.7), and `:227`'s target selector gains `[data-testid^='tree-row-']`, so the row div, still a drag target, keeps counting for the WCAG 2.5.8 clearance check. The test encoded the `role="button"` misuse. The comment at `DocTree.archive.test.tsx:113` is reworded only (it no longer names a dnd-kit sortable or keyboard sensor).
  - These insert duplicate top-level keys and switch to distinct keys (Task 9):
    - `knot-storage/tests/documents.rs` `templates_flow_set_and_list`;
    - `shares_integration.rs` `cross_doc_revoke_returns_404_and_token_stays_valid` and `public_board_svg_404_for_cross_doc_board`;
    - `comments_integration.rs` `cross_doc_comment_mutation_is_rejected`;
    - `search_integration.rs`: the `make_doc` helper (`:49`) gets a distinct key per call, which fixes `results_capped_at_max_limit` and `multi_word_prefix_is_and`.
  - Any other existing test that fails *only* because it inserts duplicate top-level keys gets the same fix, and is named in the commit message.
  - Nothing else changes. `round_trip_all_fixtures` and `tasks_live_reindex.rs` stay as they are.
- **P7. Constraint and order.**
  - The heal migration (Task 9) runs inside its migration transaction:
    1. `DROP CONSTRAINT IF EXISTS documents_workspace_id_parent_id_sort_key_key`;
    2. the heal;
    3. `DROP CONSTRAINT IF EXISTS documents_sort_key_unique`;
    4. `ADD CONSTRAINT documents_sort_key_unique UNIQUE NULLS NOT DISTINCT (workspace_id, parent_id, sort_key) DEFERRABLE INITIALLY IMMEDIATE`.
  - Its helper is `CREATE OR REPLACE FUNCTION pg_temp.knot_spread_key(...)`. It is always called qualified, and dropped at the end, so re-running the file is a no-op.
  - **What users see:** the sidebar re-sorts siblings on the client by code-unit comparison (`web/src/features/docs/tree.ts:40-42`), which is `COLLATE "C"` order.
  - **The heal** ranks rows by `sort_key COLLATE "C", parent_id NULLS FIRST, created_at, id`, which is exactly the order users see: `list_alive` orders `parent_id NULLS FIRST`, so the sidebar lists top-level pages before template children, and `buildTree`'s stable sort by key keeps that order on a key tie. `parent_id` is the same within a group, so it only decides between the groups of a root list. (Amended in Task 9 from `sort_key COLLATE "C", created_at, id`; Task 7's real-data `SELECT` uses the same order.) A group is healed when any key is invalid, duplicated, or longer than 64 characters.
  - **Root list.** The sidebar shows the children of templates at the root, sorted together with the top-level pages (`buildTree`, `tree.ts:9-17`).
    - The heal treats each workspace's root list as one unit. The root list is the rows with `parent_id IS NULL` plus the rows whose parent is a template, archived ones included.
    - If any group in it needs healing, the whole root list is respread in one combined C order, so the rendered order is kept. Each group stays unique because all combined keys are distinct.
    - The runtime rebalance (Task 10) rewrites only the database group. A rebalance of a workspace's top-level group may therefore move template children shown at the root relative to top-level pages. This is an accepted limitation, listed in the PR: template children at the root are an existing quirk, and the rebalance only runs on invalid or over-long keys.
  - After the heal, sibling and list queries order by `sort_key COLLATE "C", id`.
  - **Group rewrites are one statement.** Every rewrite of a group's keys, in the heal and in the rebalance, is a single `UPDATE documents d SET sort_key = v.k FROM (VALUES …) v(id, k) WHERE d.id = v.id AND d.workspace_id = $ws AND d.parent_id IS NOT DISTINCT FROM $parent`.
    - A `DEFERRABLE INITIALLY IMMEDIATE` unique constraint is checked at the end of that statement, so transient duplicates are fine.
    - Never use `SET CONSTRAINTS`.
    - The parent predicate makes the READ COMMITTED re-check skip rows that have meanwhile moved to another group.
    - If the `moving` doc is in the group being rebalanced, it goes into the same `VALUES` with the provisional key `'~' || id`. That way its old key can't collide with a respread key; the caller's final `UPDATE` overwrites it.
- **P8. Group lock** (taken inside `placement::place`):
  - Under a parent: `SELECT archived_at IS NOT NULL FROM documents WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE`.
    - In the placed paths it is used *instead of* `lock_live_parent` (`FOR SHARE`, `doc_store.rs:798-818`). Taking both would deadlock two concurrent creates.
    - It returns `Archived` exactly as `lock_live_parent` does. A missing parent is left to the foreign key, as today.
  - At the top level: `pg_advisory_xact_lock(1802399604, hashtext(workspace_id::text))`.
  - **Moves lock the target parent and the moved doc together:**
    - under a parent, one statement: `SELECT id, archived_at IS NOT NULL FROM documents WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR NO KEY UPDATE`;
    - at the top level: the advisory lock, then the moved doc `FOR NO KEY UPDATE`.

    Then the cycle check. Two moves that swap nesting (D into P, P into D) therefore serialize: one succeeds and the other gets `409 doc.move_cycle`, with no deadlock. `FOR NO KEY UPDATE` is enough for the moved row: with the deferrable constraint, neither `parent_id` nor `sort_key` is a key column. It still conflicts with archive's `FOR UPDATE`.
  - **Restore:** when the doc may move to the top level (`expected_parent.is_some()`), it takes `lock_group(None)` first, before its parent lock (`doc_store.rs:547`). Taking it again inside `place` is a no-op.
  - **Safety net:** `create_placed`, `move_placed`, restore and `apply_duplicate` retry the whole transaction once on SQLSTATE `40P01` (deadlock detected). One example is archive's single stamping `UPDATE` racing a rebalance inside the subtree being archived.
  - The existing explicit-key `create(ws, parent, title, sort_key, user)` and `move_to(.., sort_key)` stay exactly as they are, for the tests that call them. Routes stop using them.
- **P9. Sensors.** `MouseSensor({ activationConstraint: { distance: 6 } })` plus `TouchSensor({ activationConstraint: { delay: 250, tolerance: 5 } })`, replacing `PointerSensor` and `KeyboardSensor`.
- **P10. Menu events.** `ContextMenu` also stops `onMouseDown` and `onTouchStart` propagation, because React events bubble through portals to the draggable row.
- **P11. Projection** (signatures in Task 12):
  - `dropZone(pointerY, rect, isOwnRow)`: one's own row never yields `"into"`.
  - `projectDrop(flat, docs, activeId, overId, zone, depthDelta)` evaluates in this order:
    1. **Drop in place:** the projected *rendered* slot equals the active row's current rendered slot → `null`, whichever row is hovered. Same rendered parent and same rendered previous sibling, ignoring the active row. Examples: own row at the same depth, the next row's "above" gap, the previous row's "below" gap.
    2. **Projected parent** and position, among *rendered* rows.
    3. **Anchors** only from *real* siblings (docs whose real `parent_id` equals the projected parent): the nearest real sibling before the position gives `after_id`; otherwise the nearest after gives `before_id`; otherwise no anchor (Last).
    4. **No-op:** same real parent and same position among real siblings → `null`.
  - `FlatRow.parentId` is the *rendered* parent. A template's child shows at the root with `parentId: null`. Real parents come from `docs`.
- **P12. Move errors (Task 14).** `move.mutationFn` throws when the result is an error, so 409/422 roll back the optimistic update and show "Couldn't move". This lands in Task 14 because the menu moves added there are the first moves jsdom can drive.
- **P13. DOM contracts kept for existing e2e:**
  - the inline `paddingLeft` on an ancestor of the row Link;
  - `onContextMenu` on the Link;
  - `data-testid="doc-row-<id>"` on the Link of real rows only, never on the drag ghost or any other element.
  - New row containers use `data-testid="tree-row-<id>"`. Existing specs count `[data-testid^='doc-row-']`.
  - Missed here and amended during execution (Task 13): `e2e/flows/sidebar-resize.spec.ts` waited for `li[role='button']` rows and measured them as drag targets. It now selects `[data-testid^='tree-row-']` (see P6).
- **P14. Duplicate remap** lives in `knot-markdown` (`pub mod remap`). It rewrites:
  - element attributes `board_id`, `src` and `url`;
  - link-mark hrefs (`knot://doc/<id>`, `/api/blobs/<id>`) through `XmlText::diff` plus `format` (`use yrs::Text`).
  - Offsets are **byte** offsets: `str::len` for text chunks, 1 per embed (docs use `OffsetKind::Bytes`).
  - The new mark value is the whole existing link map with only `href` replaced (editor links also carry `target`, `rel`, `class`, `title`).
- **P15. Duplicate permissions.** Return `403 acl.editor_required` when `role.0 == WorkspaceRole::Viewer || ctx.role == WorkspaceRole::Viewer`, written the way `move_doc` does it (`docs.rs:351-353`). `WorkspaceRole` has no meaningful `Ord`; never compare it with `>=`.
- **P16. Files.**
  - **Postgres-stored bytes** (`blobs.bytes_deleted_with_metadata() == true`) are copied inside the transaction: `INSERT INTO blob_bytes SELECT $new, bytes FROM blob_bytes WHERE blob_id = $old`.
    - It must affect exactly 1 row. Otherwise the source was purged meanwhile, and the transaction aborts with `409 doc.duplicate_conflict`.
  - **Other stores** copy before the transaction through `BlobStore::copy(&self, src: Uuid, dst: Uuid, content_type: &str) -> Result<()>`.
    - The default is `get` + `put`.
    - S3 overrides it with `copy_object_internal(self.key(src), self.key(dst))`. A non-2xx status is an error.
    - S3 CopyObject can answer 200 with an error body, so a `head_object(dst)` whose size isn't `byte_size` is also an error.
  - **Cleanup is durable** (migration `20261024120200_blob_copy_pending.sql`: `blob_copy_pending(blob_id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now())`).
    - Before the first copy, every planned non-DB `dst` id is inserted into `blob_copy_pending` and committed. It is recorded *before* copying, so a copy whose response was lost is still covered.
    - `apply_duplicate` deletes those entries inside its transaction.
    - Nothing is deleted on the request path. A commit whose acknowledgement was lost, or one not yet visible (synchronous replication), must keep its files.
    - The hourly purge job (`archive.rs`, under `PURGE_LOCK_KEY`) also handles entries older than 1 hour:
      - no `blobs` row: delete the object (a missing object is fine), then the entry;
      - otherwise: just the entry.

      This also covers a process killed mid-duplicate.
  - **Final placement conflicts.** If the final placement returns `BadAnchor` or `Archived` (the source was moved or archived meanwhile), the duplicate returns `409 doc.duplicate_conflict`.
  - **Dropped requests.** The route runs the whole duplicate inside `tokio::spawn` and awaits the handle (as `routes/api/export.rs:232-246` does), so the 30 s `TimeoutLayer` or a client disconnect can't cancel it halfway.
- **P17. One-time derived-data refresh.**
  - **Migration:** `doc_markdown_cache.renderer_version int NOT NULL DEFAULT 1`, plus a `BEFORE INSERT OR UPDATE` trigger. With `v := NULLIF(current_setting('knot.renderer_version', true), '')`:
    - if `v` is set, `NEW.renderer_version := v::int`;
    - otherwise, on UPDATE, `NEW.renderer_version := 1`;
    - otherwise, on INSERT, the value is kept (the column default 1 when not given). Raw-SQL test seeds can therefore insert version-2 rows.
  - **New renders:** `put_rendered` runs `SELECT set_config('knot.renderer_version', $v::text, true)` and the upsert in one transaction, so new renders get 2.
    - Its upsert has `ON CONFLICT (doc_id) DO UPDATE … WHERE doc_markdown_cache.rendered_at_seq <= EXCLUDED.rendered_at_seq`, so a late render never overwrites a newer one.
    - It returns `bool`: whether a row was written. When nothing was written, the caller skips the task reindex.
  - **Old writers:** any other writer gets 1, including old pods during a rolling upgrade and the kept `put`. Their legacy renders are swept again.
  - **Stale** means a live doc (`archived_at IS NULL`) with no cache row at all, or with `renderer_version < RENDERER_VERSION`. Pages damaged at creation never rendered, so they have no row.
  - **Sweep:** `reindex::spawn_with_sweep(state, rx)` (called from `main.rs`) runs it; `reindex::spawn(state, rx)` stays unchanged without it.
    - On each tick, *before* the empty-pending check, it asks for up to 10 stale ids after a cursor and adds them to `pending`.
    - **Cursor:** `Option<Uuid>` on `d.id` alone (`($after IS NULL OR d.id > $after) ORDER BY d.id LIMIT 10`). It walks `documents_pkey` and probes the cache by primary key, so no new index is needed.
    - The cursor advances, so docs whose refresh fails can't stall it.
    - After an empty result, the cursor resets to `None` and the next query waits 10 minutes. This catches legacy rows written later by old pods and docs restored from the archive, at negligible cost.
    - **One sweeper:** only one replica sweeps at a time. The sweeper takes `pg_try_advisory_lock(SWEEP_LOCK_KEY)` on one dedicated connection, detached from the pool, and holds it for a whole pass: from the first query until the empty result, so the refreshes of the queued ids run under it too. Other replicas skip while it is held. This follows the purge job (`archive.rs:196`).
    - The sweep keeps its ids in its own set, so their flush takes the quiet path. An id that is also dirty from an edit takes the edit path.
    - Sweep refreshes write tasks with `TaskStore::sync_for_doc_quiet`, which writes only rows whose values changed. A healthy page's `doc_tasks.updated_at` stays as it is, and `/tasks` (ordered by `updated_at DESC`) doesn't reshuffle. Edits keep using `upsert_for_doc` unchanged.
- **P18. Snapshots without booting rooms.**
  - New `knot-crdt` accessor: `Rooms::get_active(doc_id)` returns the room only if one is already running.
  - New `pub(crate) async fn current_snapshot(state: &AppState, doc_id: Uuid) -> Result<(DocHandle, i64), SnapshotError>` in `routes/api/markdown.rs`.
    - If a room is active, it uses that room's snapshot, as `live_snapshot` does.
    - Otherwise it hydrates from storage, like `export_check::hydrate`, and returns the latest update seq.
  - Users:
    - `refresh_markdown_inner`: no behaviour change for active rooms;
    - the sweep;
    - Duplicate.

    None of them acquire rooms. Rooms are never idle-evicted, so acquiring would keep every page resident.
- **P19. Duplicate copy set.**
  - **Boards and files:** those *referenced* in the copied content:
    - `excalidraw_board.board_id`;
    - `image.src` and `attachment.url` of the form `/api/blobs/<id>`;
    - link marks to `/api/blobs/<id>`.

    Each is copied when it belongs to the same workspace and the user can read its page: `acl.effective_role(meta.workspace_id, meta.doc_id, user)` is `Some`, the rule in `crates/knot-server/src/export.rs:156-180`. Otherwise the reference is left as it is.
  - **Pages:** `knot://doc/<id>` links are remapped only to pages inside the copied set.
  - **Caps:** pages count the top page; bytes count the copied files.
- **P20. Frozen reference location (deviation from the spec's `tests/legacy/`).**
  - The frozen copies and the compat classifiers live in `crates/knot-markdown/src/legacy/` behind the feature `legacy-compat`, so the server's real-data check (Task 7) can use them.
  - Production builds don't enable the feature. `knot-markdown` enables it for its own tests through a self dev-dependency.
- **P21. Probe inputs** live in `crates/knot-markdown/tests/probes/*.md`, used only by the compat harness.
  - `tests/fixtures/` keeps its meaning: every fixture must round-trip byte for byte.

## Review Focus

1. **Pages whose stored attribute types are mixed.** For example `checked` as Bool from the editor and the string `"true"` from import, `level` as a number or a string, and `{}` marks. Output must not depend on the representation. Test: Task 1's corpus emits both representations of every attribute, and the differential runs over both.
2. **Rolling upgrade.** Old pods keep serving during and after the pre-upgrade migration. They can write order keys that the new rules reject, and cache rows with legacy renders.
   - New code rebalances instead of failing. Test: Task 10's `placement_rebalances_invalid_neighbours` seeds keys `"0"`, `"00"`, `"m0"`, `"M"` and a 70-character key with raw SQL.
   - Legacy cache writes are re-swept. Test: Task 6's `legacy_put_marks_row_stale`.
3. **Concurrent sidebar actions.** Two users create pages at the same level at once. Test: Task 10's `concurrent_first_creates_get_distinct_keys` (20 tasks, under a parent and at the top level, no 500s).
4. **Keyboard users on Windows/Linux pressing Alt+←/→.** That is browser Back/Forward, and it must be intercepted. Test: Task 15's e2e `alt_arrow_indents_without_navigating` asserts that the URL is unchanged.
5. **A Duplicate request dropped or failed mid-flight.** No rows may remain, and every copied object must be recorded for cleanup and removed by the purge job. Test: Task 18's `duplicate_failure_leaves_no_rows_or_objects`, using a recording blob-store double that forces the transaction to fail after the copies.

---

## File structure

```text
crates/knot-markdown/src/
├── from_markdown.rs        importer: TightParagraphs adapter (Task 2)
├── to_markdown.rs          serializer: container writer, tolerant export, serialise_with_report, task_item_markdown (Tasks 3–5)
├── normalise.rs            NEW: run grouping + merging (Task 4)
├── tasks.rs                + extract_tasks_from_doc (Task 5); extract_tasks(md) unchanged
├── remap.rs                NEW: remap_refs for Duplicate (Task 16)
└── legacy/                 NEW, feature "legacy-compat": frozen to_markdown/from_markdown/tasks + compat.rs classifiers (Task 1)
crates/knot-markdown/tests/
├── common/{mod.rs, corpus.rs}   NEW (Task 1)
├── compat.rs, tasks_compat.rs   NEW differential harness (Tasks 1–5)
├── probes/*.md             NEW: the 40 probe inputs (Task 1)
├── fixtures/*.md           + list_breaks.md, nested_tasklists.md, list_blocks.md (Tasks 2–3)
└── round_trip.rs           + round_trip_fixture_directory (Task 1)
crates/knot-crdt/src/{tasks.rs (NEW: task_items, patch_task_checked), room.rs, registry.rs (get_active)}
crates/knot-storage/src/{lexorank.rs, lib.rs, placement.rs (NEW), duplicate.rs (NEW), markdown_cache.rs, doc_store.rs, blobs.rs, blobs/s3.rs, audit.rs}
migrations/20261024120000_heal_sort_keys.sql, 20261024120100_markdown_renderer_version.sql, 20261024120200_blob_copy_pending.sql
crates/knot-server/src/{reindex.rs, main.rs, lib.rs, duplicate.rs (NEW), routes/api/{docs.rs, markdown.rs, export_import.rs}}
crates/knot-server/tests/{markdown_compat.rs, tasks_lockstep.rs, refresh_sweep.rs, placement_integration.rs, workspace_import_integration.rs, duplicate_integration.rs} (all NEW)
web/src/features/docs/{tree.ts, DocTree.tsx, TreeRow.tsx (NEW), DuplicateDialog.tsx (NEW), docs.api.ts}
web/src/components/ContextMenu.tsx
e2e/flows/{tree-helpers.ts (NEW), tree-reorder.spec.ts (rewritten), tree-moves.spec.ts (NEW), duplicate.spec.ts (NEW)}; tree-dnd.spec.ts removed
```

---

## Phase M: Markdown

### Task 1: Compatibility harness foundation

**Files:**
- Create:
  - `crates/knot-markdown/src/legacy/{mod.rs,to_markdown.rs,from_markdown.rs,tasks.rs,compat.rs}`.
    - `to_markdown.rs`, `from_markdown.rs` and `tasks.rs` are verbatim copies of today's files, with `crate::` paths adjusted and their `#[cfg(test)]` modules removed.
    - All of it sits behind `#[cfg(feature = "legacy-compat")] pub mod legacy;` in `lib.rs`.
  - `tests/common/{mod.rs,corpus.rs}`. `mod.rs` starts with `#![allow(dead_code)]`, as `crates/knot-export/tests/common/mod.rs:1` does.
  - `tests/compat.rs`.
  - `tests/probes/*.md`: the 40 Markdown inputs of `/private/tmp/claude-501/-Users-christianhuening-code-odc-knot/33e5095c-5ecc-4688-b5a2-e58623055ffa/scratchpad/pr42-probes/shared/cases.rs`, one file per case, named after the case.
- Modify:
  - `crates/knot-markdown/Cargo.toml`: `[features] legacy-compat = []` and a new `[dev-dependencies] knot-markdown = { path = ".", features = ["legacy-compat"] }`. Do not use `required-features`: it makes `--test compat` error or skip silently.
  - `tests/round_trip.rs`: add `round_trip_fixture_directory`, which reads every `tests/fixtures/*.md` sorted and asserts `serialise(parse(md)) == md`. The existing `round_trip_all_fixtures` stays.

**Interfaces (produced):**
- **Frozen reference:** `knot_markdown::legacy::{serialise, parse, extract_tasks}`, with today's signatures.
- **Compat module (`knot_markdown::legacy::compat`):**
  - `pub fn schema_violations(doc: &yrs::Doc) -> Vec<String>` checks the content rules of `tools/schema.json`: `list_item: paragraph block*`; `table_cell`/`table_header: block+`; `code_block: text*`; no `XmlText` or `hard_break` directly under block containers.
  - `pub fn healthy(doc: &DocHandle) -> Option<String>` returns the legacy export when it is a fixed point.
  - `pub fn normalised(doc: &yrs::Doc) -> String` is a canonical dump: tags, attribute values as strings, text, mark keys and values with `{}`/`Bool(true)` unified.
  - `pub fn export_class(doc: &DocHandle) -> Option<&'static str>` and `pub fn import_class(md: &str) -> Option<&'static str>` start empty. Tasks 2–5 add classes.
- **Corpus (`tests/common/corpus.rs`):**
  - `pub struct Case { pub name: String, pub doc: DocHandle }`;
  - `pub fn generated() -> Vec<Case>`: deterministic combinations of every node and mark, nesting up to 4, each attribute in both editor shape (number, Bool, `{}`, link `target`/`rel`/`class`, table `colwidth`/`colspan`) and import shape (string, `Bool(true)`);
  - `pub fn random(seed: u64, n: usize) -> Vec<Case>`, using an xorshift generator;
  - `pub fn markdown_inputs() -> Vec<(String, String)>`: fixtures plus probes.

- [ ] **Step 1: Write the harness tests** in `tests/compat.rs`:
  - `serializer_matches_legacy_on_healthy_docs`: over `generated()` + `random(1..=50, 200)`, new == legacy byte for byte for every healthy doc with no `export_class`.
  - `importer_matches_legacy_on_healthy_inputs`: compare `normalised` for each input whose legacy import is schema-valid and whose legacy export is a fixed point, when it has no `import_class`.
  - `no_text_is_lost`: every text character of the doc appears in the new export.
  - `schema_check_finds_the_damaged_shapes`: `list_item[p, hard_break, p]` gives a non-empty violation list.
  - `legacy_import_produces_schema_violations_today`: `legacy::parse("- a  \n  b\n")` has violations. This documents the bug.
- [ ] **Step 2: Run.**
  - Run: `cargo nextest run -p knot-markdown --test compat --test round_trip`
  - Expected: compile failure (the legacy module is missing).
- [ ] **Step 3: Implement** the legacy copies, compat module, corpus, probes and the directory-driven fixture test.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-markdown`
  - Expected: all pass, including the existing 22 tests (13 in `tests/round_trip.rs`, 9 in `src/tasks.rs`). New and legacy code are identical at this point.
- [ ] **Step 5: Commit** `test(markdown): differential compatibility harness against the frozen serializer`.

### Task 2: Importer adapter (A1, A2, A3, A7 import side, A9 import side)

**Files:**
- Modify: `crates/knot-markdown/src/from_markdown.rs`, `src/legacy/compat.rs` (classes).
- Create: fixture `tests/fixtures/list_breaks.md`, which must be a fixed point.
- Test: `tests/import_lists.rs`.
- Reference: `TightParagraphs` in `/private/tmp/claude-501/-Users-christianhuening-code-odc-knot/33e5095c-5ecc-4688-b5a2-e58623055ffa/scratchpad/pr42-probes/knot-markdown-fixed/src/from_markdown.rs` (around line 544).

**Interfaces:**
- `parse` is unchanged.
- A private `struct TightParagraphs<'a, I: Iterator<Item = Event<'a>>>` wraps the pulldown parser.
- New `import_class` values:
  - `item_first_child_not_paragraph` (e.g. `- # h`);
  - `rule_in_container`;
  - `html_between_item_lines` (e.g. `"- a\n  <!-- c -->\n  b\n"`: legacy gives `p "ab"`, new gives `p a, p b`);
  - `markdown_escaping` (probes `escaped-star`, `escaped-task-text`: the writer doesn't escape text, which is a non-goal and a follow-up issue);
  - `leading_hard_break` (probe `tight-hard-break-first`, a hard break as the first inline: the same follow-up).

- [ ] **Step 1: Write the failing tests** (`tests/import_lists.rs`):

```rust
#[test] fn hard_break_in_tight_item_stays_in_its_paragraph() { /* "- a  \n  b\n" → list_item[paragraph["a", hard_break, "b"]]; schema_violations empty; serialise == "- a  \n  b\n" */ }
#[test] fn soft_break_in_tight_item_joins_with_space() { /* "- line one\n  continued\n" → one paragraph "line one continued" */ }
#[test] fn breaks_work_in_ordered_task_nested_and_quoted_lists() { /* one assertion each: serialise succeeds and schema clean */ }
#[test] fn image_and_board_in_tight_items_are_kept() { /* "- ![a](x.png)" and "- ![l](knot://board/<uuid>.svg)" → export contains them */ }
#[test] fn html_comment_between_item_lines_keeps_lines_apart() { /* "- a\n  <!-- c -->\n  b" → text contains "a" and "b", not "ab" */ }
#[test] fn empty_item_gets_an_empty_paragraph() { /* "- a\n-\n- c" → middle list_item[paragraph[]]; re-export keeps three items */ }
#[test] fn rule_inside_item_or_quote_stays_in_its_container() { /* "> a\n>\n> ***\n>\n> b" → blockquote contains the horizontal_rule */ }
```

  Also add `new_import_is_schema_valid_on_every_input` to `compat.rs`, over `markdown_inputs()`.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-markdown --test import_lists --test compat`
  - Expected: the hard-break test fails with the damaged shape or a schema violation.
- [ ] **Step 3: Implement the adapter:**
  - emit `Start(Paragraph)` on the first inline event under an `Item`, and `End(Paragraph)` before the next block event, `Rule`, `Html` or `End(Item)`; `TaskListMarker` passes through;
  - remove the `list_item` branches in Text (362-369) and Code (387-392), and `ensure_para_child` (542, used only there);
  - at `TagEnd::Item`, insert an empty paragraph at index 0 if the first child isn't a paragraph;
  - make `Rule` (402-405) use `push_block` instead of `frag.push_back`.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-markdown`
  - Any new differential failure must be a named class, with its justification in the report.
- [ ] **Step 5: Commit** `fix(markdown): keep tight list item content inside its paragraph on import`.

### Task 3: Serializer for lists and containers (A4, A5, A6, A8, A9 export side)

**Files:**
- Modify: `crates/knot-markdown/src/to_markdown.rs`, `src/legacy/compat.rs`.
- Create: fixtures `nested_tasklists.md`, `list_blocks.md` (fixed points).
- Test:
  - `tests/export_lists.rs`, `tests/compat.rs`;
  - `crates/knot-server/tests/markdown_import_integration.rs` (it has an `export` helper at `:229`).

**Interfaces:**
- `write_list_item(buf, txn, item, marker)`: the indent is the marker width only (`"- "` = 2, `"10. "` = 4), and the checkbox is written as item content.
- A private `fn write_container(buf, txn, parent, in_quote) -> Result<(), SerError>`, shared by list items and blockquotes.
- New `export_class` values: `nested_tasklist`, `multi_block_item`, `quote_block_sequence`, `ordered_checkbox`, `rule_in_container`. Each must hold the fixed-point property under the new code.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn nested_checklist_indents_by_marker_width() { /* [ ] a > [x] b > [ ] c → "- [ ] a\n  - [x] b\n    - [ ] c\n"; parse(serialise) structurally equal */ }
#[test] fn ordered_items_keep_checkboxes() { /* "1. [ ] a\n2. [x] b\n" round-trips */ }
#[test] fn list_item_with_several_blocks_round_trips() { /* [p,p], [p,image], [p,ordered start=3], [p,list,p], [p,quote,p] — each: fixed point and structurally equal */ }
#[test] fn blockquote_list_then_paragraph_round_trips() { /* blockquote[list, paragraph] */ }
#[test] fn rule_in_item_or_quote_is_not_turned_into_a_heading() { /* list_item[p "a", horizontal_rule] → re-import keeps paragraph + rule */ }
#[test] fn healthy_quote_and_empty_items_are_byte_identical() { /* "> a\n> b\n", "> a\n> ![x](u)\n", "- \n", "- [ ] \n", "2. \n" serialise exactly as legacy (P2) */ }
```

  Add `import_export_import_is_stable` to `compat.rs`: for every input without the `import_class` `markdown_escaping` or `leading_hard_break`, `normalised(parse(serialise(parse(md)))) == normalised(parse(md))`.

  **Server tests** in `markdown_import_integration.rs` (A10, nested checklists):
  - `markdown_of_nested_checklist_uses_marker_indent`;
  - `history_restore_keeps_nested_checklists`: restore through the history restore route keeps the structure;
  - `from_template_keeps_nested_checklists`;
  - `workspace_import_keeps_list_line_breaks_and_nested_checklists`:
    - import a zip, built as in `contributors_integration.rs:769-790`, holding a `.md` with `- a  \n  b` and a nested checklist;
    - GET markdown then returns exactly the 2-space-indented form, and the stored doc is schema-valid.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-markdown --test export_lists --test compat`, then `cargo nextest run -p knot-server --test markdown_import_integration`
  - Expected: the nested-checklist tests show 6-space indentation.
- [ ] **Step 3: Implement the indentation, the ordered checkbox, and `write_container`.**
  - **No blank line between blocks when:**
    - the previous block is a heading, code block, rule or empty paragraph;
    - or the previous block is a paragraph and the next "can interrupt a paragraph": a code block, heading or blockquote; a bullet list whose first item has text; an ordered list starting at 1 that has text;
    - or, in a quote, paragraph → paragraph;
    - or, in a quote, paragraph → image/board/attachment (P2).
  - **Otherwise** write a blank line.
  - **Empty items** keep their trailing space (P2).
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-markdown`, then `cargo nextest run -p knot-server --test markdown_import_integration`
- [ ] **Step 5: Commit** `fix(markdown): write nested and multi-block list items so they round-trip`.

### Task 4: Tolerant export (damaged shapes, unknown nodes) and the routes that round-trip (A10)

**Files:**
- Create: `crates/knot-markdown/src/normalise.rs`.
- Modify:
  - `to_markdown.rs`, `lib.rs`, `src/legacy/compat.rs` (class `damaged_shape`);
  - `crates/knot-export/src/walk.rs` (P3);
  - `crates/knot-server/src/routes/api/markdown.rs` (count unknowns);
  - `crates/knot-obs/src/metrics.rs` (describe `knot_markdown_unknown_nodes_total`).
- Test:
  - `tests/export_tolerant.rs`;
  - `crates/knot-export/tests/walk.rs`;
  - `crates/knot-server/tests/markdown_import_integration.rs` (it has an `export` helper at `:229`).

**Interfaces:**
- `pub fn serialise_with_report(engine: &YrsEngine, doc: &DocHandle) -> Result<(String, Vec<String>), SerError>` (P4).
- `normalise::block_children(txn, parent) -> Vec<Child>`, where `enum Child { Block(XmlElementRef), Para(Vec<XmlOut>) }`. It applies P3.

- [ ] **Step 1: Write the failing tests.**
  - **Markdown tests:**

```rust
#[test] fn damaged_hard_break_item_exports_healed() { /* hand-built list_item[p "first", hard_break, p "second"] → "- first  \n  second\n" */ }
#[test] fn damaged_soft_break_item_exports_healed() { /* list_item[p "line one", text " ", p "continued"] → "- line one continued\n" */ }
#[test] fn damaged_bold_runs_merge() { /* p[**bold**], stray " " that is itself bold, p[**more**] → "- **bold more**\n"; with an unmarked stray " " → "- **bold** **more**\n" */ }
#[test] fn damaged_chain_becomes_one_paragraph() { /* [p a, hb, p b, hb, p c] → "- a  \n  b  \n  c\n" */ }
#[test] fn stray_text_at_top_level_and_in_cells_becomes_paragraph() {}
#[test] fn unknown_node_degrades_to_text_and_is_reported() { /* el("callout",[p "hi"]) → "hi\n", report == ["callout"]; serialise() also Ok */ }
```

  - **P3 test-vector table:** in `tests/export_tolerant.rs` and in `crates/knot-export/tests/walk.rs` (`walk_glue_vectors`). `text_under_blockquote_is_wrapped` stays unchanged and green.
  - **Server tests** in `markdown_import_integration.rs`. Each returns 500 before this task.
    - `markdown_of_damaged_page_is_healed`: GET markdown of a hand-built damaged page returns 200 with the healed text.
    - `history_restore_heals_a_damaged_page`: restore through the history restore route gives a schema-valid page with the healed content.
    - `from_template_heals_a_damaged_template`.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-markdown --test export_tolerant`, then `cargo nextest run -p knot-server --test markdown_import_integration`
- [ ] **Step 3: Implement** the normaliser, use it in every block container, add `serialise_with_report`, mirror P3 in knot-export, and count unknowns in the server.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-markdown -p knot-export`, then `cargo nextest run -p knot-server --test markdown_import_integration`
- [ ] **Step 5: Commit** `fix(markdown): export damaged and unknown content instead of failing the page`.

### Task 5: Tasks from the Y.Doc, in lockstep with toggles (A11, A12)

**Files:**
- Create: `crates/knot-crdt/src/tasks.rs`.
- Modify:
  - `crates/knot-crdt/src/{lib.rs,room.rs}`: move `patch_task_checked` (`room.rs:675-721`) into `tasks.rs` as `pub`, built on `task_items`, and have `room.rs` call it with no behaviour change;
  - `crates/knot-markdown/src/{tasks.rs,to_markdown.rs,legacy/compat.rs}`;
  - `crates/knot-server/src/routes/api/markdown.rs`: `refresh_markdown_inner` uses `extract_tasks_from_doc`.
- Test:
  - `crates/knot-markdown/tests/tasks_compat.rs`;
  - `crates/knot-server/tests/tasks_lockstep.rs`. Copy the inline setup of `tasks_live_reindex.rs:13-79` (`AppState::with_pool`, `Rooms::new(...).with_dirty_tx`, `reindex::spawn`).

**Interfaces:**
- **In knot-crdt:**
  - `pub fn task_items<T: ReadTxn>(frag: &XmlFragmentRef, txn: &T) -> Vec<XmlElementRef>`. Pre-order; every `list_item` that has a `checked` attribute, including those inside table cells and ordered lists.
  - `pub fn patch_task_checked(...)` with today's signature.
- **In `to_markdown`:** `pub(crate) fn task_item_markdown<T: ReadTxn>(txn: &T, item: &XmlElementRef) -> String` (P1).
- **In knot-markdown:** `pub fn extract_tasks_from_doc(doc: &DocHandle) -> Vec<Task>` (P1). `item_index` is the position in `task_items`, and `checked` is the attribute read as `"true"`.
- **Doc clone helper (tests):** `engine.encode_state_as_update` then `apply_update` into `engine.new_doc()`. `yrs::Doc::clone` shares its store, so it is not a copy.
- **"Legacy agrees with the room"** for a doc means:
  - the legacy listing has as many tasks as `task_items`;
  - for every k, flipping item k on a clone with `patch_task_checked` and re-running legacy `serialise` + `extract_tasks` changes only legacy task k's `checked`.

- [ ] **Step 1: Write the failing tests.**
  - **Unit:**
    - `nested_checklist_lists_parent_and_child_separately` gives `[(0,"parent"),(1,"child"),(2,"next")]`;
    - `table_cell_checklists_are_listed`;
    - `ordered_list_checkboxes_are_listed`;
    - `plain_bullet_starting_with_brackets_is_not_a_task`;
    - `due_date_in_second_paragraph_is_kept`: `[p "do X", p "by <knot://time link>"]` gives the legacy due date.
  - **Differential (`tasks_compat.rs`):**
    - for corpus docs where legacy agrees with the room, the new listing equals the legacy listing in text, index, checked, assignee and due date;
    - the class `task_with_nested_list` is exempt as P1 says.
  - **Lockstep property:** for every corpus doc and every k, flipping item k on a clone changes exactly `extract_tasks_from_doc(clone)[k].checked` and nothing else.
  - **Server:** `toggle_from_tasks_flips_the_listed_item` for:
    - a nested checklist, where the wrong item flips before this task;
    - a checklist in a table cell, where the item is missing before this task;
    - an ordered-list checklist, a regression guard (fixed by Task 3).
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-markdown --test tasks_compat`, then `cargo nextest run -p knot-server --test tasks_lockstep`
- [ ] **Step 3: Implement** `task_items` and the moved `patch_task_checked`, `task_item_markdown` and `extract_tasks_from_doc`, and switch the server refresh.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-crdt -p knot-markdown`, then `cargo nextest run -p knot-server --test tasks_lockstep --test tasks_live_reindex`
- [ ] **Step 5: Commit** `fix(tasks): list checklist items from the page itself, in step with toggles`.

### Task 6: One-time refresh of cached Markdown and task rows

**Files:**
- Create: `migrations/20261024120100_markdown_renderer_version.sql` (column + trigger, P17).
- Modify:
  - `crates/knot-storage/src/markdown_cache.rs`;
  - `crates/knot-storage/src/tasks.rs` (`sync_for_doc_quiet`);
  - `crates/knot-markdown/src/lib.rs` (`pub const RENDERER_VERSION: i32 = 2;`);
  - `crates/knot-crdt/src/registry.rs` (`get_active`, P18);
  - `crates/knot-server/src/{routes/api/markdown.rs,reindex.rs,main.rs}` (`main.rs:264` calls `spawn_with_sweep`).
- Test: `crates/knot-storage/tests/markdown_cache.rs`, `crates/knot-server/tests/refresh_sweep.rs`.

**Interfaces:**
- `MarkdownCacheStore::put_rendered(doc_id, rendered_at_seq: i64, markdown: &str, renderer_version: i32) -> Result<bool>` (P17: seq guard; returns whether a row was written). `put` stays and now yields version 1 through the trigger.
- `MarkdownCacheStore::stale_ids(current: i32, after: Option<Uuid>, limit: i64) -> Result<Vec<Uuid>>`.
  - `FROM documents d LEFT JOIN doc_markdown_cache c ON c.doc_id = d.id WHERE d.archived_at IS NULL AND (c.doc_id IS NULL OR c.renderer_version < $current) AND ($after IS NULL OR d.id > $after) ORDER BY d.id LIMIT $limit`.
  - Every column is qualified.
- `TaskStore::sync_for_doc_quiet(doc_id, tasks)`: like `upsert_for_doc`, but it inserts, updates and deletes only the rows whose values changed.
- `current_snapshot` (P18), used by `refresh_markdown_inner`.

- [ ] **Step 1: Write the failing tests:**
  - `stale_ids_returns_old_renders_only_and_pages_by_cursor`;
  - `legacy_put_marks_row_stale`: `put_rendered(.., 2)` then `put` gives version 1;
  - `trigger_keeps_explicit_insert_version`: a raw `INSERT` with version 2 keeps 2, and a raw `UPDATE` without the setting gives 1;
  - `sweep_rerenders_stale_rows_and_reindexes_tasks`:
    - seed a cache row with `renderer_version = 1` and broken nested-checklist Markdown;
    - within a deadline, the row has version 2, healed Markdown and correct task rows;
    - a fresh version-2 row keeps the same `updated_at`;
    - no room was acquired for the swept doc (`get_active` is `None` afterwards).
  - `sweep_skips_past_a_failing_doc`: a doc whose refresh fails doesn't stop later docs from being refreshed.
  - `sweep_renders_a_page_without_cache_row`.
  - `late_render_does_not_overwrite_a_newer_one`: `put_rendered` at seq 5 after seq 6 writes nothing and returns `false`.
  - `sweep_keeps_healthy_task_timestamps`: a healthy page's `doc_tasks.updated_at` is unchanged after the sweep.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-storage --test markdown_cache`, then `cargo nextest run -p knot-server --test refresh_sweep`
- [ ] **Step 3: Implement** the migration, the store methods, `get_active`, `current_snapshot`, the refresh writing `put_rendered(.., RENDERER_VERSION)` (skipping the task reindex when it returns `false`), and `spawn_with_sweep` (P17).
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-storage --test markdown_cache`, then `cargo nextest run -p knot-server --test refresh_sweep --test tasks_live_reindex`
- [ ] **Step 5: Commit** `feat(markdown): refresh cached Markdown and tasks once after an upgrade`.

### Task 7: Real-data compatibility check (opt-in)

**Files:**
- Create: `crates/knot-server/tests/markdown_compat.rs`.
- Modify:
  - `crates/knot-server/Cargo.toml`: a dev-dependency on `knot-markdown` with feature `legacy-compat`;
  - `deploy/helm/knot/README.md`: one paragraph on how to run the check.

**Interfaces:**
- `pub async fn compat_report(pool: &PgPool) -> CompatReport`, inside the test file. Two tests call it:
  - `#[tokio::test] #[ignore] async fn real_data_compat()` reads `KNOT_COMPAT_DATABASE_URL` and connects read-only. It runs no migrations, prints the report, and fails if any healthy page outside the named classes changes.
  - `real_data_compat_runs_on_a_seeded_db` (not ignored) uses `fresh_db` seeded with a healthy page and a damaged page, and asserts the counts: 1 unchanged, 1 `damaged_shape`.
- **The report:**
  - for every live page, hydrated offline as in P18: the export by legacy and by new code, classified with `knot_markdown::legacy::compat` (`healthy`, `export_class`), with counts and the first diff per class;
  - sort-key groups the heal migration would rewrite, from a `SELECT` mirroring its predicate (P7);
  - the number of stale docs (P17). If `doc_markdown_cache.renderer_version` is missing (a pre-upgrade backup; check `information_schema.columns`), every live doc counts as stale.

- [ ] **Step 1: Write both tests.**
- [ ] **Step 2: Run, expect FAIL** (`compat_report` missing).
- [ ] **Step 3: Implement the check.**
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-server --test markdown_compat`
  - The `--ignored` path is documented; the controller runs it against local databases.
- [ ] **Step 5: Commit** `test(markdown): opt-in real-data compatibility check`.

## Phase S: Sort keys

### Task 8: New sort-key generator

**Files:**
- Modify:
  - `crates/knot-storage/src/lexorank.rs`;
  - `crates/knot-storage/src/lib.rs` (P5);
  - `crates/knot-storage/src/doc_store.rs:598` (`crate::sort_key_between`, P5).
- Create: `crates/knot-server/tests/placement_integration.rs` (Task 10 adds to it).
- Reference: `/private/tmp/claude-501/-Users-christianhuening-code-odc-knot/33e5095c-5ecc-4688-b5a2-e58623055ffa/scratchpad/pr42-probes/lexo/newimpl.rs`. Its `between(None, None)` returns `"i"`; special-case it to `"m"`.

**Interfaces:**
- `pub enum KeyError { Invalid(String), Unordered }`, deriving `Debug, PartialEq, Eq, thiserror::Error`.
- `pub fn is_valid(k: &str) -> bool`.
- `pub fn between(a: Option<&str>, b: Option<&str>) -> Result<String, KeyError>`.
- `pub fn spread(n: usize) -> Vec<String>`:
  - `w` = the smallest width with `36^w > 2(n+1)`;
  - for `i = 1..=n`: `floor(i * 36^w / (n+1))`, written as a `w`-digit base-36 string left-padded with `'0'`, then trailing `'0'`s stripped.
  - Task 9's SQL implements the same formula.

- [ ] **Step 1a: Write the behaviour tests and show their RED on main.**
  - Put them in a new integration file, `crates/knot-storage/tests/sort_key_props.rs`: `top_inserts_never_collide`, `inserting_after_the_same_key_never_collides`, `prefix_family_stays_inside`, `exhaustive_small_alphabet`, `simulation_2000_seeds` and `appends_grow_slowly`.
  - They use only `knot_storage::sort_key_between`, which returns `String` on main and after P5, plus a local `fn valid(k: &str) -> bool` for `^[0-9a-z]*[1-9a-z]$`.
  - So they compile against main's code, and the RED run shows the failing assertion (e.g. a="m", b="m01" gives "m0h").
- [ ] **Step 1b: Write the new-API tests** in `lexorank.rs` `mod tests`: `invalid_inputs_are_errors_not_panics` and `spread_is_sorted_valid_distinct`, plus the mechanical `.unwrap()` on existing tests (P6). Their RED is a compile failure.

  The bodies of all these tests are sketched below. Each one lives in the file given by Step 1a or 1b.

  **Server tests** in `placement_integration.rs`, all failing on main:
  - `ten_new_pages_under_a_parent_all_succeed_in_order` (on main #8 returns 409);
  - `ten_new_top_level_pages_get_distinct_ordered_keys`;
  - `insert_after_first_ten_times`.

```rust
#[test] fn top_inserts_never_collide() { /* 2000 × between(None, first) all distinct and descending; fails on main */ }
#[test] fn inserting_after_the_same_key_never_collides() { /* m, then 50 × between(Some(anchor), Some(next)) — the "insert after first" chain; fails on main */ }
#[test] fn prefix_family_stays_inside() { /* all a ≤2 chars, b = a + '0'.. + c (≤3 chars): a < r < b */ }
#[test] fn exhaustive_small_alphabet() { /* alphabet ['0','1','2','9','a','m','y','z'], a ≤2, b ≤3, every valid a<b: valid(r) && a<r<b; fails on main (a="m", b="m01" gives "m0h") */ }
#[test] fn simulation_2000_seeds() { /* xorshift ops: top insert, insert after, move; keys stay unique and ordered */ }
#[test] fn invalid_inputs_are_errors_not_panics() { /* "", "0", "00", "m0", "M", "-", equal and reversed → Err */ }
#[test] fn spread_is_sorted_valid_distinct() { /* n in {1,2,10,35,36,100,5000} */ }
#[test] fn appends_grow_slowly() { /* 1000 × between(last, None) ≤ 210 chars */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-storage --test sort_key_props`, then `cargo nextest run -p knot-storage --lib lexorank`, then `cargo nextest run -p knot-server --test placement_integration`
- [ ] **Step 3: Implement the generator** (midpoint over `[0-9a-z]`, common prefix with `a` padded by `'0'`, no trailing `'0'`).
- [ ] **Step 4: Run, expect PASS** for `cargo nextest run -p knot-storage`, then `cargo nextest run -p knot-server --test placement_integration --test docs_integration`, then `cargo check --workspace --all-targets --all-features` (existing callers unchanged).
- [ ] **Step 5: Commit** `fix(sort-keys): generator that always finds a key between two neighbours`.

### Task 9: Heal migration and top-level uniqueness

**Files:**
- Create: `migrations/20261024120000_heal_sort_keys.sql`, with a header in the `heal_doc_cycles` style (P7).
- Modify:
  - `crates/knot-storage/src/doc_store.rs`: sibling and list queries order by `sort_key COLLATE "C", id`; `list_alive` keeps `parent_id NULLS FIRST` first;
  - the duplicate-key tests in P6.
- Test: `crates/knot-storage/tests/migrations_apply.rs`, using the `VERSION` pattern at 179-233 (apply up to the previous migration, seed, apply the rest).

**Interfaces:** the migration and its `pg_temp.knot_spread_key(i, n)` implement `lexorank::spread`'s formula exactly.

- [ ] **Step 1: Write the failing tests:**
  - `heal_sort_keys_leaves_healthy_groups_untouched`: keys and `updated_at` byte for byte;
  - `heal_sort_keys_preserves_order_and_fixes_bad_groups`: seeds `"0"`, `"00"`, `"m0"`, `"M"`, a 70-character key, top-level duplicates and archived rows. Each group's order before (`sort_key COLLATE "C"`, `created_at`, `id`) equals its order after, and all keys are valid and distinct;
  - `heal_assigns_exactly_lexorank_spread_keys`: a bad group of n rows, for n in {1, 2, 10, 35, 36, 100}, gets exactly `lexorank::spread(n)`;
  - `heal_keeps_rendered_root_order_with_template_children`: a bad top-level group interleaved, by key, with the children of a template keeps the combined C order of the root list (P7).
  - `heal_sort_keys_is_idempotent`: re-running the whole SQL file (`sqlx::raw_sql(include_str!(…))`) changes nothing;
  - `top_level_duplicates_are_rejected_after_migration`.
- [ ] **Step 2: Run, expect FAIL** (migration missing). `cargo nextest run -p knot-storage --test migrations_apply`
- [ ] **Step 3: Write the migration, switch the ORDER BYs, and fix the P6 tests.**
- [ ] **Step 4: Run, expect PASS** for `cargo nextest run -p knot-storage`, then the full workspace. For stale migration embedding, run `cargo clean -p knot-test-support -p knot-storage` if needed.
- [ ] **Step 5: Commit** `fix(sort-keys): repair existing page order keys and make top-level keys unique`.

### Task 10: Transactional placement and rebalance

**Files:**
- Create: `crates/knot-storage/src/placement.rs`.
- Modify:
  - `doc_store.rs`: new trait methods `create_placed` and `move_placed`. The explicit-key `create` and `move_to` stay unchanged (P8). Restore's top-level path (`:598`) calls `place(.., Placement::Last, Some(doc_id))`;
  - `crates/knot-server/src/routes/api/docs.rs`: create (`:167`), move (`:396`) and from-template (`:698`) use placement; 422 anchors.
- Test:
  - `crates/knot-storage/tests/placement.rs`;
  - `crates/knot-server/tests/placement_integration.rs`;
  - `docs_integration.rs` (P6).

**Interfaces:**

```rust
pub enum Placement { First, Last, After(Uuid), Before(Uuid) }
pub(crate) async fn lock_group(conn: &mut PgConnection, ws: Uuid, parent: Option<Uuid>) -> Result<(), DocStoreError>; // P8; re-taking it in the same transaction is a no-op
pub(crate) async fn place(conn: &mut PgConnection, ws: Uuid, parent: Option<Uuid>, placement: Placement, moving: Option<Uuid>) -> Result<String, DocStoreError>;
// DocStore:
async fn create_placed(&self, ws: Uuid, parent: Option<Uuid>, title: &str, placement: Placement, created_by: Uuid) -> Result<Document, DocStoreError>;
async fn move_placed(&self, ws: Uuid, doc_id: Uuid, actor: Uuid, parent: Option<Uuid>, placement: Placement) -> Result<Document, DocStoreError>;
// DocStoreError gains: BadAnchor
```

- **`place` steps:**
  1. `lock_group` (P8).
  2. Read the whole group: archived and templates included, `moving` excluded, ordered by `sort_key COLLATE "C", id`.
  3. Validate the anchor: it must be a live, non-template member of the group, else `BadAnchor`.
  4. Neighbours are the anchor and the *immediately adjacent row in the whole group*. An archived row between two live ones still bounds the key, so it can't collide.
  5. Call `between`. On `Err` or a result longer than 64 characters, rebalance the group with `spread(n)` in one statement (P7) and retry once.
  6. Return the key.
- **Callers:**
  - `create_placed`: `place`, then insert.
  - `move_placed`: the combined lock of the target parent and the moved doc (P8), then the cycle check, then `place`, then update.
  - Both keep audit and invalidation as today.
- **Route mapping:**
  - create: no `after_id` → `First`; `after_id` → `After`;
  - move: `after_id` → `After`, else `before_id` → `Before`, else `Last`;
  - from-template → `Last`;
  - `BadAnchor` → `422 doc.bad_anchor`.

- [ ] **Step 1: Write the failing tests:**
  - `placement_rebalances_invalid_neighbours` (raw SQL seeds `"0"`, `"00"`, `"m0"`, `"M"` and a 70-character key in one group; creates and moves around them succeed, keys end up valid, and order is preserved). Before this task, the `expect` in `sort_key_between` panics, giving a 500;
  - `archived_sibling_between_neighbours_does_not_collide` (a regression guard: `siblings()` already includes archived rows);
  - `concurrent_first_creates_get_distinct_keys` (20 `tokio::spawn`, under a parent and at the top level; every request succeeds);
  - `rebalance_concurrent_with_sibling_move_both_succeed` (a rebalance triggered by a seeded `"0"` key, while another task moves a sibling of that group into another parent);
  - `rebalance_with_the_moved_page_in_its_group`:
    - start from `spread(11)` with `"1"` and `"0"` inserted in front;
    - move the last page to the top;
    - it succeeds, and all keys stay valid and distinct (P7 provisional key);
  - `swapping_nesting_moves_do_not_deadlock` (move D into P and P into D concurrently: one 200, one `409 doc.move_cycle`, no 500);
  - `restore_concurrent_with_top_level_rebalance_both_succeed` (restore a child whose parent is archived, so it goes to the top level, while a top-level rebalance runs);
  - `move_with_non_sibling_anchor_is_422`;
  - `move_with_self_anchor_is_422`;
  - change `move_with_unknown_anchor_falls_to_end` to expect 422 (P6).
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-storage --test placement`, then `cargo nextest run -p knot-server --test placement_integration --test docs_integration`
- [ ] **Step 3: Implement placement and switch the callers.**
- [ ] **Step 4: Run, expect PASS** for the full `knot-storage` and `knot-server` suites, including `archive_integration`'s 409s for archived parents.
- [ ] **Step 5: Commit** `fix(sort-keys): place pages in one transaction and rebalance instead of failing`.

### Task 11: Workspace import assigns fresh keys

**Files:**
- Modify: `crates/knot-server/src/routes/api/export_import.rs` (create loop at 536-571).
- Test: new `crates/knot-server/tests/workspace_import_integration.rs`. Build the zip the way `contributors_integration.rs:769-790` does.

**Interfaces:**
- The manifest `sort_key` is used only for ordering: per parent, order by (`sort_key COLLATE "C"`, manifest index).
- Docs are created with `create_placed(.., Placement::Last)`, under each new parent or under the import target.

- [ ] **Step 1: Write the failing tests:**
  - `importing_the_same_export_twice_under_a_parent_keeps_every_doc` (`/api/workspace/import?parent_id=<p>`). On main the second import collides, skips docs and regrafts their children;
  - `importing_the_same_export_twice_at_top_level_keeps_every_doc` (a regression guard after Task 9);
  - `import_preserves_manifest_order`.
- [ ] **Step 2: Run, expect FAIL** (the first test). `cargo nextest run -p knot-server --test workspace_import_integration`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS,** including `contributors_integration` and `archive_integration` unchanged.
- [ ] **Step 5: Commit** `fix(sort-keys): give imported pages fresh keys in their original order`.

## Phase T: Sidebar

### Task 12: Projection model (pure functions)

**Files:**
- Modify: `web/src/features/docs/tree.ts`, `tree.test.ts`; `DocTree.tsx` (import `countDescendants` from `tree.ts`).
- Reference: `/private/tmp/claude-501/-Users-christianhuening-code-odc-knot/33e5095c-5ecc-4688-b5a2-e58623055ffa/scratchpad/pr42-probes/dnd/projection-proto.mts`. It has no `docs` argument, and its no-op assertion at line 52 (`"B","B","below",0` → `{parent_id:"A"}`) becomes `null`.

**Interfaces:**

```ts
export const INDENT = 12;
export type FlatRow = { id: string; parentId: string | null /* rendered parent */; depth: number; title: string; descendantCount: number };
export type Zone = "above" | "below" | "into";
export type MoveArgs = { parent_id: string | null; before_id?: string; after_id?: string };
export type Projection = { depth: number; minDepth: number; maxDepth: number; args: MoveArgs };
export function countDescendants(node: TreeNode): number; // moved from DocTree.tsx:365
export function rowDescription(row: FlatRow): string; // "level 2, 3 subpages" (Global Constraints)
export function flattenVisible(tree: TreeNode[], collapsed: ReadonlySet<string>, activeId?: string): FlatRow[]; // active row's subtree left out
export function dropZone(pointerY: number, rect: { top: number; height: number }, isOwnRow: boolean): Zone;
export function projectDrop(flat: FlatRow[], docs: Doc[], activeId: string, overId: string, zone: Zone, depthDelta: number): Projection | null; // P11
export function keyboardMove(flat: FlatRow[], docs: Doc[], id: string, cmd: "up" | "down" | "indent" | "outdent"): MoveArgs | null;
export function describeMove(flat: FlatRow[], id: string, args: MoveArgs): string; // "Moved “B” into “A”, position 2 of 3"
```

- [ ] **Step 1: Write the failing tests** (a Vitest table):
  - **Prototype cases:**
    - un-nest the last child of the last root in place (`depthDelta = -1`);
    - nest by dragging right;
    - first-child clamp under an expanded parent;
    - depths 0..2 below the last child;
    - collapsed parent (`into` vs depth);
    - dragged subtree excluded;
    - a non-last child can't climb above the next row's depth.
  - **Edge cases:**
    - drop in place returns null;
    - same real parent and position returns null;
    - a template's child rendered at the root, dropped in place on its own row, in the next row's "above" gap, or in the previous row's "below" gap, returns null;
    - a drop next to a template child anchors on a real sibling or falls back to Last;
    - `into` appends as the last child.
  - **`keyboardMove`:** first and last rows, indent without a previous sibling (null), outdent at the root (null).
  - **`rowDescription`:** "level 1", "level 2, 1 subpage", "level 2, 3 subpages".
  - **Compatibility table:** for every gesture that today's `moveArgs` supports (reorder before/after between siblings, `into`), applying the `projectDrop` args gives the same order as applying today's `moveArgs` args.
- [ ] **Step 2: Run, expect FAIL.** `cd web && pnpm test src/features/docs/tree.test.ts`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** for `pnpm test`, `pnpm tsc` and `pnpm lint`.
- [ ] **Step 5: Commit** `feat(tree): projection model for nesting, un-nesting and reordering`.

### Task 13: DocTree drag rewrite

**Files:**
- Create: `web/src/features/docs/TreeRow.tsx` (split out of `DocTree.tsx`).
- Modify: `DocTree.tsx`, `web/src/components/ContextMenu.tsx` (P10).
- Test: `DocTree.dnd.test.tsx`, `ContextMenu.test.tsx`.

**Interfaces:**
- **DndContext:**
  - `sensors = [MouseSensor, TouchSensor]` (P9).
  - A custom `collisionDetection` stores `args.pointerCoordinates` in a ref. It returns the row under the pointer, or the row with the closest vertical centre. It returns no collision when the pointer is outside the tree's rect, so a drop outside cancels.
  - `measuring={{ droppable: { strategy: MeasuringStrategy.Always } }}`.
  - `onDragMove` reads `pointerY` from that ref, takes `depthDelta = Math.round(delta.x / INDENT)`, calls `dropZone` + `projectDrop`, and sets state only when the projection changes.
  - `onDragEnd` mutates only when the projection is non-null.
- **Rows:**
  - The row `div` (`data-testid="tree-row-<id>"`) uses `useDraggable`'s `listeners` and `setNodeRef`, plus `useDroppable`. It does **not** spread `attributes`, so there is no `role="button"` and no extra tab stop.
  - The row `div` carries `data-drop-zone` and `data-drop-depth` while it is the drop target.
  - The drop line is drawn at `left: 4 + depth*INDENT`.
  - The `<li>` holds no dnd attributes, and the DragOverlay ghost has no `doc-row-`/`tree-row-` testid.
- **Collapsed state:** a `collapsed: Set<string>` in DocTree. During a drag, the dragged row's subtree is *unmounted*, not hidden with CSS, so descendants can never be targets.
- **Click suppression:**
  - `onDragStart` adds `window.addEventListener("click", h, { capture: true })`. `h` calls `preventDefault()` and `stopPropagation()` when `treeRef.current?.contains(e.target)`.
  - It is removed after its first call, or 100 ms after drag end/cancel. dnd-kit's own capture listener on `document` stops propagation before any tree listener could run, so the listener must be on `window`.
- **Accessibility:**
  - `accessibility={{ announcements, screenReaderInstructions: { draggable: <copy from Global Constraints> } }}`;
  - `announcements` comes from an exported `makeAnnouncements(getFlat)`, which implements `onDragStart`, `onDragOver`, `onDragEnd` and `onDragCancel` with titles, never ids;
  - the row Link gets `aria-describedby="<attributes['aria-describedby']> tree-desc-<id>"`, plus a visually hidden `<span id="tree-desc-<id>">` holding `rowDescription(row)`.
- **P13 contracts** stay. **ContextMenu (P10)** stops `onMouseDown` and `onTouchStart` propagation too.

- [ ] **Step 1: Write the failing tests** (Vitest, stubbed-fetch pattern from `DocTree.archive.test.tsx`):
  - the row `div` and the `<li>` have no `role` and no `tabindex`, and the Link is focusable;
  - the row link keeps `paddingLeft` and `onContextMenu`;
  - for the fixture Folder > Child > Grandchild, Folder's Link `aria-describedby` points at an element whose text is "level 1, 2 subpages";
  - `makeAnnouncements` messages contain titles, not ids;
  - after `onDragStart`, a click on a row link is default-prevented, and after the timeout it isn't;
  - ContextMenu: a `mousedown` on an item doesn't reach the parent's handler.
- [ ] **Step 2: Run, expect FAIL.** `cd web && pnpm test src/features/docs src/components`
- [ ] **Step 3: Implement**, keeping the P13 DOM contracts.
- [ ] **Step 4: Run, expect PASS** for the full web suite (existing DocTree tests unchanged), `pnpm tsc` and `pnpm lint`.
- [ ] **Step 5: Commit** `fix(tree): reliable drag and drop for nesting, un-nesting and reordering`.

### Task 14: Keyboard and menu moves

**Files:**
- Modify: `TreeRow.tsx`, `DocTree.tsx` (also P12).
- Test: `DocTree.keyboard.test.tsx`.

**Interfaces:**
- **Context-menu items** `ctx-move-up`, `ctx-move-down`, `ctx-indent` and `ctx-outdent`, for editors. An item is disabled when `keyboardMove` returns null.
- **`onKeyDown` on the row link:**
  - Alt+ArrowUp / Alt+ArrowDown / Alt+ArrowRight / Alt+ArrowLeft call `keyboardMove`, always with `preventDefault()`;
  - Shift+F10 and the ContextMenu key open the row menu at the link's bounding rect.
- **Announcements:** a polite live region (`data-testid="tree-live"`) announces `describeMove`.
- **Focus:** after a move, focus returns to `doc-row-<id>`, found by id after render.

- [ ] **Step 1: Write the failing tests:**
  - each menu item sends the right args and is disabled at the edges;
  - Alt+→ indents, and the event's `defaultPrevented` is true;
  - Shift+F10 opens the menu positioned at the link;
  - the live region text;
  - focus is on the moved row after the refetch;
  - `move_error_rolls_back_and_toasts` (P12): `ctx-move-down` against a stub fetch returning 422 restores the order and shows "Couldn't move".
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** for `pnpm test`, `pnpm tsc` and `pnpm lint`.
- [ ] **Step 5: Commit** `feat(tree): move pages by keyboard and from the page menu`.

### Task 15: Sidebar e2e

**Files:**
- Create:
  - `e2e/flows/tree-helpers.ts`:
    - `seedPages(request, specs)`: creates pages via the API in order, each with `after_id` set to the previous sibling, so the sidebar order matches;
    - `dragRow(page, fromId, to: { id: string } | { x: number; y: number }, { zone, dx, waitForZone = !process.env.E2E_NO_ZONE_WAIT })`. A point target is used for drops outside the tree. `releasing_on_own_row_does_not_reload` and `drop_outside_cancels` pass `waitForZone: false`, because no projection appears there. The steps:
      1. mouse down on the source `doc-row-<fromId>` link. It exists on main and on the branch, and its mousedown bubbles to the row's listeners;
      2. move 10 px to activate;
      3. *re-measure* the target `doc-row-<toId>` box (the dragged subtree has collapsed);
      4. move in `steps: 10`;
      5. optionally wait for `tree-row-<toId>` to have `data-drop-zone` and `data-drop-depth`. This wait is the helper's only use of `tree-row-`;
      6. release.
  - `e2e/flows/tree-moves.spec.ts`.
- Modify: `e2e/flows/tree-reorder.spec.ts`, rewritten to use the helpers:
  - assert the starting order via `GET /api/docs` before each drag;
  - assert the result with `expect.poll` on `parent_id` and order via the API;
  - keep the five scenarios. For scenario 5 (drag P onto its child Q), assert Q isn't rendered during the drag, release, then poll that the parent ids are unchanged.
- Delete: `e2e/flows/tree-dnd.spec.ts`. Its "both rows still render" assertion moves into `tree-moves`.

**Tests in `tree-moves.spec.ts`:**
- `un_nest_last_child_of_last_root_by_dragging_left`;
- `nest_by_dragging_right`;
- `drop_into_collapsed_parent`;
- `menu_indent_and_outdent`;
- `alt_arrow_indents_without_navigating` (URL unchanged; Review Focus 4);
- `releasing_on_own_row_does_not_reload` (a `page.on("load")` counter stays at 1);
- `drop_outside_cancels`;
- `both_rows_render_after_moves`.

- [ ] **Step 1: Write the specs.** Run them on main with `E2E_NO_ZONE_WAIT=1` (main has no zone attributes) and record in the report which assertions fail and why. The un-nest, menu, Alt and own-row-reload tests are expected to fail on main.
- [ ] **Step 2: Run on the branch, expect PASS three times in a row** (flake check):
  - Run: `cd e2e && pnpm playwright test flows/tree-reorder.spec.ts flows/tree-moves.spec.ts flows/create-placement.spec.ts --repeat-each=3`
  - Use the local stack per the dev-stack notes.
- [ ] **Step 3: Commit** `test(tree): deterministic e2e for drag, keyboard and menu moves`.

## Phase D: Duplicate

### Task 16: `remap_refs`

**Files:**
- Create: `crates/knot-markdown/src/remap.rs`, `tests/remap.rs`.

**Interfaces:**
- `pub struct Remap { pub docs: HashMap<Uuid, Uuid>, pub boards: HashMap<Uuid, Uuid>, pub blobs: HashMap<Uuid, Uuid> }`.
- `pub fn references(doc: &yrs::Doc) -> Refs`, where `Refs { docs, boards, blobs: HashSet<Uuid> }` lists every reference the remap can rewrite.
- `pub fn remap_refs(doc: &yrs::Doc, map: &Remap) -> usize` returns the number of rewrites (P14). Ids are parsed as UUIDs: exact matches only, never substrings.

- [ ] **Step 1: Write the failing tests:**
  - `references` finds every kind of reference, including in nested tables and lists;
  - every reference kind is rewritten;
  - unknown ids are left alone;
  - after remapping, the tree equals the source except for the mapped ids;
  - link-mark attributes other than `href` are preserved;
  - non-ASCII text before a link mark keeps the mark on the right characters (byte offsets).
- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-markdown --test remap`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(duplicate): remap page, board and file references in a copied page`.

### Task 17: Storage for Duplicate

**Files:**
- Create: `crates/knot-storage/src/duplicate.rs`, `migrations/20261024120200_blob_copy_pending.sql` (P16).
- Modify:
  - `blobs.rs`: `BlobStore::copy(&self, src, dst, content_type) -> Result<()>`, defaulting to `get` + `put` (P16);
  - `blobs/s3.rs`: the override, with the `head_object` size check;
  - `doc_store.rs`: `live_subtree(ws, root) -> Vec<SubtreeRow { id, parent_id, title, icon, sort_key }>`;
  - `audit.rs`: `record_in_tx_with_data` (data as `sqlx::types::JsonValue`; the crate has no `serde_json`).
- Test: `crates/knot-storage/tests/duplicate.rs`.

**Interfaces:**
- **`live_subtree`** is a recursive CTE over the root's descendants, root excluded. It stops at archived and template rows and skips their subtrees. Parents come before children, and siblings are in `sort_key COLLATE "C", id` order.

```rust
pub struct PageCopy { pub new_id: Uuid, pub parent: Option<Uuid>, pub title: String, pub icon: Option<String>, pub state: Vec<u8> }
pub struct BoardCopy { pub new_id: Uuid, pub new_doc_id: Uuid, pub label: Option<String>, pub state: Vec<u8>, pub svg: Option<Vec<u8>> }
pub struct BlobCopy { pub new_id: Uuid, pub new_doc_id: Uuid, pub source: BlobMetadata, pub bytes_in_db: bool }
pub struct DuplicatePlan { pub ws: Uuid, pub actor: Uuid, pub source_id: Uuid, pub top: PageCopy, pub pages: Vec<PageCopy>, pub boards: Vec<BoardCopy>, pub blobs: Vec<BlobCopy> }
pub struct DuplicateOutcome { pub top: Document, pub pages: Vec<(Uuid, i64)> /* (page, first update seq) */ }
pub async fn apply_duplicate(pool: &PgPool, plan: &DuplicatePlan) -> Result<DuplicateOutcome, DocStoreError>; // DocStoreError gains DuplicateConflict
pub async fn record_pending_copies(pool: &PgPool, ids: &[Uuid]) -> Result<(), DocStoreError>;
pub async fn purge_stale_copies(pool: &PgPool, blobs: &dyn BlobStore, older_than: std::time::Duration) -> Result<usize, DocStoreError>;
```

- **One transaction.** The group lock is taken only at the end, so the heavy inserts don't block other creates and moves in the source's group:
  1. Insert the top page under `plan.top.parent` with the provisional key `"~" + new_id`. It is unique, and it is invisible to others until commit.
  2. Insert the subpages in source order. Each new parent group gets `spread` keys.
  3. For each page: one `doc_updates` row with the actor, plus `doc_contributors` (the SQL from `insert_batch`).
  4. Boards: one row each, plus one `board_updates` row and `svg_cached`.
  5. Blob rows. For `bytes_in_db`, also `INSERT INTO blob_bytes SELECT $new, bytes FROM blob_bytes WHERE blob_id = $old`, which must affect exactly 1 row, else `DuplicateConflict`. Delete this plan's `blob_copy_pending` entries.
  6. Audit `doc.duplicate` with `{"source_id": …}`, and the invalidations as for create.
  7. Last: `place(conn, ws, plan.top.parent, Placement::After(plan.source_id), Some(plan.top.new_id))` (Task 10), then `UPDATE` the top page's key, then commit. A `BadAnchor` or `Archived` here becomes `DuplicateConflict`.

- [ ] **Step 1: Write the failing tests:**
  - `apply_duplicate_inserts_everything_atomically`;
  - `no_partial_rows_on_failure` (a forced unique conflict leaves no rows);
  - `live_subtree_skips_archived_and_template_subtrees`;
  - `copy_default_impl_copies_bytes` (an in-memory `BlobStore` test double; PG-stored bytes are copied by SQL instead);
  - `db_bytes_missing_is_a_conflict` (source `blob_bytes` deleted after planning gives `DuplicateConflict` and no rows);
  - `purge_stale_copies_deletes_unreferenced_objects_only` (one pending entry with a `blobs` row, one without; with `older_than = 0`, only the second object is deleted, and both entries go).
- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-storage --test duplicate`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(duplicate): store a page copy with its subpages, drawings and files in one transaction`.

### Task 18: Duplicate route

**Files:**
- Create: `crates/knot-server/src/duplicate.rs`; add `pub mod duplicate;` to `crates/knot-server/src/lib.rs`.
- Modify:
  - `routes/api/docs.rs`: register `POST /api/docs/{id}/duplicate` in `doc_id_routes`. The handler lives in `docs.rs`, so it can use the private `to_response`;
  - `crates/knot-server/src/archive.rs`: `run_purge_once` also calls `purge_stale_copies(.., 1 h)` (P16).
- Test: `crates/knot-server/tests/duplicate_integration.rs`.
  - Base it on the `export_integration.rs` harness (`world_with`, `set_content`, `upload`, `archive`, `member`).
  - Add a helper that creates a page under a parent, like `archive_integration`'s `create_doc(&who, title, parent)`.

**Interfaces:**
- `pub async fn run(state: AppState, ctx: …, source_id: Uuid, include_subpages: bool) -> Result<Document, DuplicateError>` in `duplicate.rs`.
  - The handler calls it through `tokio::spawn(...).await` (P16).
  - It maps `DuplicateError` to 403, 404, 409 (`doc.archived`, `doc.duplicate_conflict`), 413 and 422 JSON errors, and returns `201` with `to_response(&top)`.
- **`run` steps:**
  1. Roles (P15).
  2. Refuse a template source (422) and an archived source (409).
  3. The page set: `live_subtree` when subpages are included, else the source alone. Caps: pages first (413).
  4. For each page, `current_snapshot` (P18, no room acquired) into a transient doc, then `references`.
  5. Choose the boards and readable blobs (P19). Caps: bytes (413).
  6. Allocate new ids for pages, boards and blobs.
  7. `record_pending_copies` for every planned non-DB `dst` (P16), then copy those blob objects with `BlobStore::copy`.
  8. `remap_refs` on each transient doc, then `encode_state_as_update`. For boards, merge snapshot and updates and encode once.
  9. `apply_duplicate`. On failure, nothing is deleted on the request path: the pending entries leave cleanup to the purge job.
  10. After commit, best effort for each copy: `serialise` + `put_rendered`, and task extraction from the transient doc.

- [ ] **Step 1: Write the failing tests:**
  - `duplicate_is_lossless` (`colwidth`/`colspan`, multi-block cells, an attachment node, a list line break; GET markdown of the copy returns 200);
  - `copy_sits_right_after_the_source`;
  - `copy_title_and_icon`;
  - `board_is_copied_and_independent` (new id, same state and SVG; editing the copy leaves the source untouched);
  - `images_survive_archiving_and_purging_the_original`;
  - `subpages_copied_with_internal_links_remapped` (links to pages outside the set unchanged);
  - `subpages_not_copied_by_default`;
  - `unreadable_blobs_are_not_copied` (reference left unchanged);
  - `archived_source_is_409`;
  - `viewer_is_403`;
  - `template_is_422`;
  - `too_large_is_413` (201 pages);
  - `duplicate_failure_leaves_no_rows_or_objects` (Review Focus 5):
    - a test double `RecordingBlobStore` returns `false` from `bytes_deleted_with_metadata()`. Its `copy` only records `dst` and returns `Ok(())`, with no delegation. Its `delete` records;
    - on its first `copy` it archives the source through the pool, so the final placement fails;
    - assert `409 doc.duplicate_conflict`, which proves `apply_duplicate` ran;
    - assert no new documents, blobs or boards rows, and a `blob_copy_pending` entry for every copied `dst`;
    - then run `purge_stale_copies(.., 0)` and assert deleted == copied and the pending table is empty;
  - `byline_credits_the_duplicating_user`;
  - `copied_tasks_are_indexed`.
- [ ] **Step 2: Run, expect FAIL** (404). `cargo nextest run -p knot-server --test duplicate_integration`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(duplicate): duplicate a page, optionally with its subpages`.

### Task 19: Duplicate in the sidebar

**Files:**
- Create: `web/src/features/docs/DuplicateDialog.tsx`, `DuplicateDialog.test.tsx`.
- Modify:
  - `docs.api.ts`: `duplicate(id, body): Promise<ApiResult<Doc>>`, using `parse(Doc, r.ok)` and importing `ApiResult` from `web/src/lib/api.ts`;
  - `TreeRow.tsx`: menu item `ctx-duplicate` "Duplicate…", for editors;
  - `DocTree.tsx`: renders the dialog outside the `DndContext`.
- Mockup: `docs/superpowers/specs/2026-10-24-duplicate-mockup.html`.

**Interfaces:**
- **When the dialog appears:** only when `countDescendants(node) > 0`. Otherwise the copy is made immediately.
- **Dialog:** `role="dialog"`, `aria-modal`, Escape closes it, focus is trapped, and focus returns to the row link. The checkbox is unticked by default.
- **Success:** invalidate `["docs"]`, then `markDocEditMode(id)` (`editMode.ts`), then navigate to `/doc/<id>`.
- **Errors:**
  - 413 shows the too-large message inside the dialog when it is open, otherwise as a toast;
  - any other error shows "Couldn't duplicate this page." the same way.

- [ ] **Step 1: Write the failing tests:**
  - the item is shown for editors and not for viewers;
  - no dialog appears without subpages (exactly one POST with `include_subpages: false`);
  - the dialog appears with N in its text and an unticked default;
  - ticking sends `true`;
  - Escape closes it and returns focus;
  - a 413 shows the too-large message in the dialog;
  - a 413 without a dialog shows a toast;
  - navigation happens on success;
  - a `docsApi.duplicate` contract test.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** for `pnpm test`, `pnpm tsc` and `pnpm lint`.
- [ ] **Step 5: Commit** `feat(duplicate): Duplicate… in the page menu`.

### Task 20: Duplicate e2e

**Files:**
- Create: `e2e/flows/duplicate.spec.ts`, modelled on `archive.spec.ts`.

**Tests:**
- `duplicate_without_subpages`: the copy appears right after the original and opens.
- `duplicate_with_subpages`:
  - a page with an image, a board and a resized table is copied with its subpages;
  - after archiving the original, the copy's image and board still render;
  - the column widths match.

- [ ] **Step 1: Write the spec.**
- [ ] **Step 2: Run, expect PASS.** `cd e2e && pnpm playwright test flows/duplicate.spec.ts --repeat-each=2`
- [ ] **Step 3: Commit** `test(duplicate): e2e duplicate with and without subpages`.

## Final

### Task 21: Docs and verification (controller)

- **CHANGELOG, knot-api skill, Helm README.** The README covers the real-data check command, the migrations, and the one-time refresh.
- **PR description:** credits @szEvEz and #42, and lists the follow-up issues.
- **Full repo checks.**
- **Compatibility harness evidence:** the counts per named class.
- **Real-data check** against the local databases, with the command given to the owner.
- **Final whole-branch review.**
