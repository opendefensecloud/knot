# Markdown Lists, Sort Keys, Sidebar Drag and Drop, Duplicate: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the four problems reported in PR #42 at the root: Markdown list round trips, sort keys, sidebar drag and drop, and Duplicate. Prove with a compatibility harness that existing pages only improve.

**Architecture:**
- **Markdown (`knot-markdown`).**
  - An event adapter restores tight-list paragraphs on import.
  - The serializer gets a shared container writer and a tolerant export that repairs damaged shapes.
  - Tasks are extracted from the Y.Doc using the room's own walk.
  - A frozen copy of today's code drives a differential harness.
- **Sort keys (`knot-storage`).**
  - A fractional-index generator replaces the old one.
  - Placement moves into transactional store methods that rebalance when needed.
  - A heal migration fixes existing keys and makes top-level keys unique.
- **Sidebar (`web`).** A projection model with pure functions in `tree.ts`, and explicit keyboard/menu moves.
- **Duplicate (server).**
  - A lossless CRDT state copy with remapped ids.
  - Boards and files are copied.
  - All rows are written in one transaction.

**Tech Stack:** Rust (yrs 0.27, sqlx, axum), Postgres 18, React + dnd-kit core 6.3.1, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-10-24-tree-markdown-sortkeys-duplicate-design.md`. Read it before any task, together with the "Planning decisions" section below. Problem ids (A1–A12, B1–B6, C1–C9, D) refer to the spec.

## Global Constraints

- **Compatibility is the top constraint.**
  - A healthy page's stored content, sort order and Markdown output never change. Healthy means the legacy export `m` succeeds and `legacy_serialise(legacy_parse(m)) == m`.
  - Every change is either a named bug class or provably lossless.
- **Every bug gets a test that fails on main.** Each task's report shows the RED run (command plus failing assertion) before the fix.
- **Existing tests stay unchanged,** except the three named in Task 9 and Task 10. Each of those changes is justified in its commit message as encoding a bug.
- **Placement errors are JSON errors:**
  - `422 doc.bad_anchor` for an anchor that isn't a live sibling under the target parent, or that is the moved doc itself;
  - `409 doc.sort_key_conflict` (kept);
  - `409 doc.move_cycle` (kept).
- **Duplicate:**
  - `POST /api/docs/{id}/duplicate` with body `{ "include_subpages": bool }` returns `201` and a `DocResponse`;
  - `422 doc.is_template`;
  - `413 doc.duplicate_too_large` above 200 pages or 200 MiB of files;
  - the title is `"<title> (copy)"` for the top page only;
  - subpages are unticked by default.
- **Copy (exact):**
  - menu items "Duplicate…", "Move up", "Move down", "Indent", "Outdent";
  - dialog title `Duplicate “<title>”`, checkbox "Include its N subpages", hint "Every subpage is copied with all its content, drawings and files.", buttons "Cancel" and "Duplicate";
  - toasts "Couldn't duplicate this page.", "This page is too large to duplicate (over 200 pages or 200 MB of files).", "Couldn't move".
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
- **Commits:** Conventional Commits with the scopes `markdown`, `tasks`, `sort-keys`, `tree` and `duplicate`. Signed if 1Password is available; otherwise `git -c commit.gpgsign=false commit`. The owner authorized this before, and the branch is re-signed before push.
- **Docs:**
  - CHANGELOG:
    - Upgrading: two migrations, one that repairs page order where needed and one that marks cached Markdown for a one-time background refresh;
    - Added: Duplicate, and Move/Indent menu items with Alt+arrow shortcuts;
    - Fixed: each area.
  - `.claude/skills/knot-api/SKILL.md`: the duplicate route and move/create 422.
  - Follow-up issues listed in the PR: Markdown escaping, and the full ARIA tree model.

## Planning decisions (rulings on conflicts found during planning)

- **P1. Task text stays legacy-identical.**
  - Task items, indexes, checked state, assignee and due date come from the Y-walk.
  - The text is produced by running the *legacy text joiner* over the Markdown of the item's own blocks with nested lists excluded. The joiner is pulldown events with today's space-at-every-boundary rule.
  - Named class `task_with_nested_list`: legacy text included nested content through the A4 bug.
- **P2. Byte-identical cases the prototype broke stay identical:**
  - in a blockquote, a paragraph followed by an image, board or attachment keeps `> a\n> ![x](…)`;
  - empty items keep `- `, `- [ ] ` and `n. ` with the trailing space.
- **P3. Glue rule.**
  - Only `[paragraph, stray-run, paragraph]` becomes one paragraph.
  - A stray run next to a single paragraph, or alone, becomes its own paragraph.
  - The knot-export walker adopts the same rule. Its existing test `text_under_blockquote_is_wrapped` (`[t, p]` gives two paragraphs) stays green.
  - The helper is implemented in both crates, because knot-export must not depend on knot-markdown. A shared test-vector table lives in each crate's tests.
- **P4. Unknown nodes degrade.**
  - Add `pub fn serialise_with_report(engine, doc) -> Result<(String, Vec<String>), SerError>`. The `Vec` holds unknown tag names.
  - `serialise` keeps its signature and discards the report.
  - The server warns and counts `knot_markdown_unknown_nodes_total`.
- **P5. `sort_key_between` stays a `String`-returning helper,** implemented as `lexorank::between(a, b).expect("valid sort keys")`.
  - Production code uses the placement API and never calls it.
  - This keeps 13 existing test call sites unchanged.
  - `between(None, None)` returns `"m"`.
- **P6. Tests that encode bugs are changed, with justification:**
  - `docs_integration.rs` `move_with_unknown_anchor_falls_to_end`: asserts 422 `doc.bad_anchor`;
  - `knot-storage/tests/documents.rs` `templates_flow_set_and_list`;
  - `shares_integration.rs` `cross_doc_revoke_returns_404_and_token_stays_valid`.

  The last two insert duplicate top-level keys. They switch to distinct keys.
- **P7. Constraint.**
  - The migration does: drop the old unnamed `documents_workspace_id_parent_id_sort_key_key`, heal, then add `documents_sort_key_unique UNIQUE NULLS NOT DISTINCT (workspace_id, parent_id, sort_key) DEFERRABLE INITIALLY IMMEDIATE`, all in one transaction.
  - Rebalance uses `SET CONSTRAINTS documents_sort_key_unique DEFERRED` inside its transaction.
  - Sibling and list queries order by `sort_key COLLATE "C", id`.
- **P8. Group lock.**
  - Under a parent: `SELECT … FROM documents WHERE id = parent FOR NO KEY UPDATE`.
  - At the top level: `pg_advisory_xact_lock(0x6b6e6f74, hashtext(workspace_id::text))`.
  - Order: the moved doc's row lock first, then the group lock (as in `move_to`).
- **P9. Sensors.** `MouseSensor({ activationConstraint: { distance: 6 } })` plus `TouchSensor({ activationConstraint: { delay: 250, tolerance: 5 } })`, replacing `PointerSensor` and `KeyboardSensor`.
- **P10. Menu events.** `ContextMenu` also stops `onMouseDown` and `onTouchStart` propagation, because React events bubble through portals to the draggable row.
- **P11. `projectDrop` is zone-based:**
  - signature `projectDrop(flat, activeId, overId, zone, depthDelta)`;
  - `zone = dropZone(pointerY, rect)`;
  - hovering one's own row always yields a gap zone;
  - it returns `null` for no-ops, judged against the doc's *real* `parent_id` and neighbours, not render depth.
- **P12. Move errors.** `move.mutationFn` throws when the result is an error, so 409/422 roll back the optimistic update and show the "Couldn't move" toast.
- **P13. DOM contracts kept for existing e2e:**
  - the inline `paddingLeft` on an ancestor of the row Link;
  - `onContextMenu` on the Link;
  - `doc-row-<id>` only on real rows, never on the drag ghost.
- **P14. Duplicate remap** lives in `knot-markdown` (`pub mod remap`). It rewrites:
  - element attributes `board_id`, `src` and `url`;
  - link-mark hrefs (`knot://doc/<id>`, `/api/blobs/<id>`), through `XmlText::diff` plus `format`.
- **P15. Duplicate permissions.** Require `EffectiveDocRole` ≥ Editor on the source *and* `ctx.role != Viewer`, the same rule as create.
- **P16. Files in Postgres** are copied inside the transaction (`INSERT INTO blob_bytes SELECT …`). S3 objects are copied before it through a new `BlobStore::copy(src, dst)` (default `get` + `put`; S3 uses `copy_object_internal`), with detached cleanup on failure.
- **P17. Derived-data refresh.**
  - `doc_markdown_cache.renderer_version int NOT NULL DEFAULT 1`. New renders write 2 through `put_rendered`.
  - A sweep in `reindex.rs` feeds up to 10 stale, live doc ids into the pending set per 2 s flush, oldest first.
  - The old `put` is kept for tests.

## Review Focus

1. **Pages whose stored attribute types are mixed.** For example `checked` as Bool from the editor and the string `"true"` from import, `level` as a number or a string, and `{}` marks. Output must not depend on the representation. Test: Task 1's corpus generator emits both representations of every attribute, and the differential runs over both.
2. **Rolling upgrade.** Old pods keep serving during and after the pre-upgrade migration and can still write keys the new rules reject. New code must rebalance instead of failing. Test: Task 9's `placement_rebalances_invalid_and_duplicate_neighbours` seeds keys `"0"`, `"00"`, `"m0"` and duplicates with raw SQL.
3. **Concurrent sidebar actions.** Two users create pages at the same level, or duplicate and move at once. Test: Task 9's `concurrent_first_creates_get_distinct_keys` (20 tasks, under a parent and at the top level).
4. **Keyboard users on Windows/Linux pressing Alt+←/→.** That is browser Back/Forward, and it must be intercepted. Test: Task 15's e2e `alt_arrow_indents_without_navigating` asserts that the URL is unchanged.
5. **A Duplicate request dropped mid-flight** (408 or client disconnect). No orphaned rows or S3 objects may remain. Test: Task 18's `duplicate_failure_leaves_no_rows_or_objects`, using the `ScriptedBlobStore` double to fail after the copies.

---

## File structure

```text
crates/knot-markdown/src/
├── from_markdown.rs        importer: TightParagraphs adapter (Task 2)
├── to_markdown.rs          serializer: container writer, tolerant export, serialise_with_report (Tasks 3–4)
├── normalise.rs            NEW: stray grouping + run merging helpers (Task 4)
├── tasks.rs                extract_tasks_from_doc + legacy text joiner (Task 5)
├── remap.rs                NEW: remap_refs for Duplicate (Task 16)
└── legacy/                 NEW, cfg(feature = "legacy-compat"): frozen to_markdown/from_markdown/tasks (Task 1)
crates/knot-markdown/tests/
├── common/{mod.rs, schema_check.rs, corpus.rs}   NEW (Task 1)
├── compat.rs               NEW differential harness (Task 1, grown in Tasks 2–5)
├── fixtures/*.md           + list_breaks, nested_tasklists, list_blocks, probe_* (Tasks 1–3)
└── round_trip.rs           reads the fixture directory (Task 1)
crates/knot-crdt/src/tasks.rs   NEW: task_items(frag, txn) shared walk (Task 5)
crates/knot-storage/src/{lexorank.rs, placement.rs (NEW), duplicate.rs (NEW), markdown_cache.rs, doc_store.rs, blobs*.rs, audit.rs}
migrations/20261024120000_heal_sort_keys.sql, 20261024120100_markdown_renderer_version.sql
crates/knot-server/src/{reindex.rs, routes/api/{docs.rs, markdown.rs, export_import.rs}, duplicate.rs (NEW)}
crates/knot-server/tests/{markdown_compat.rs (NEW, #[ignore] real-data), placement_integration.rs (NEW), duplicate_integration.rs (NEW), tasks_lockstep.rs (NEW)}
web/src/features/docs/{tree.ts, DocTree.tsx, TreeRow.tsx (NEW, split), DuplicateDialog.tsx (NEW), docs.api.ts}
web/src/components/ContextMenu.tsx
e2e/flows/{tree-reorder.spec.ts (rewritten deterministic), tree-moves.spec.ts (NEW), duplicate.spec.ts (NEW)}; tree-dnd.spec.ts removed
```

---

## Phase M: Markdown

### Task 1: Compatibility harness foundation

**Files:**
- Create:
  - `crates/knot-markdown/src/legacy/{mod.rs,to_markdown.rs,from_markdown.rs,tasks.rs}`: verbatim copies of today's files with `crate::` paths adjusted, behind the feature `legacy-compat`;
  - `tests/common/{mod.rs,schema_check.rs,corpus.rs}`;
  - `tests/compat.rs`;
  - fixtures `probe_*.md`, copied from the 40 Markdown inputs in the scratchpad's `pr42-probes/shared/cases.rs`.
- Modify:
  - `crates/knot-markdown/Cargo.toml`: `[features] legacy-compat = []`, and `[dev-dependencies] knot-markdown = { path = ".", features = ["legacy-compat"] }`, or `required-features` on the test targets;
  - `tests/round_trip.rs`: `round_trip_all_fixtures` reads `tests/fixtures/*.md` sorted, and skips names listed in `KNOWN_NOT_FIXED_POINT`. Today's 14 fixtures must stay in the list that has to round-trip.

**Interfaces (produced):**
- **Frozen reference:** `knot_markdown::legacy::{serialise, parse, extract_tasks}`, with the same signatures as today.
- **Schema checker (`tests/common/schema_check.rs`):** `pub fn schema_violations(doc: &yrs::Doc) -> Vec<String>`. It checks the content rules of `tools/schema.json`:
  - `list_item: paragraph block*`;
  - no `XmlText` or `hard_break` directly under block containers;
  - `table_cell: block+`;
  - and so on.
- **Corpus (`tests/common/corpus.rs`):**
  - `pub struct Case { name: String, doc: DocHandle }`;
  - `pub fn generated() -> Vec<Case>`: deterministic combinations of every node and mark, nesting up to 4, each attribute in both editor shape (number, Bool, `{}`) and import shape (string, `Bool(true)`);
  - `pub fn random(seed: u64, n: usize) -> Vec<Case>`, using an xorshift generator;
  - `pub fn from_markdown_inputs() -> Vec<(String, String)>`: fixtures plus probe inputs.
- **Harness (`tests/compat.rs`):**
  - `fn healthy(doc) -> Option<String>` returns the legacy export when it is a fixed point;
  - `fn named_class(doc) -> Option<&'static str>`, which grows in Tasks 2–5.

- [ ] **Step 1: Write the harness tests.**
  - `serializer_matches_legacy_on_healthy_docs`: over `generated()` + `random(1..=50, 200)`, assert new == legacy byte-for-byte for every healthy doc with no named class.
  - `importer_matches_legacy_on_healthy_inputs`: compare under the normaliser (tags, attribute values as strings, text, mark keys and values).
  - `no_text_is_lost` (rule 2).
  - `schema_check_finds_the_damaged_shapes`: build `list_item[p, hard_break, p]` and assert a non-empty violation list.
  - `legacy_import_produces_schema_violations_today`: the `- a  \n  b` input gives violations under `legacy::parse` (this documents the bug).
- [ ] **Step 2: Run.** `cargo nextest run -p knot-markdown --test compat --test round_trip`.
  - Expected: all pass, since new == legacy on main.
  - The schema test is RED for the *new* `parse` only after Task 2 makes it the importer; until then it asserts on `legacy::parse`.
- [ ] **Step 3: Implement the legacy copies, checker, corpus and the directory-driven fixture test.**
- [ ] **Step 4: Run.** `cargo nextest run -p knot-markdown`. Expected: all pass, including the existing 23.
- [ ] **Step 5: Commit** `test(markdown): differential compatibility harness against the frozen serializer`.

### Task 2: Importer adapter (A1, A2, A3, A7 import side, A9 import side)

**Files:**
- Modify: `crates/knot-markdown/src/from_markdown.rs`.
- Create: fixture `tests/fixtures/list_breaks.md`.
- Test: `tests/import_lists.rs`.
- Reference: the scratchpad `pr42-probes/knot-markdown-fixed/src/from_markdown.rs` (`TightParagraphs`). Port it; don't copy it blindly.

**Interfaces:**
- `parse` is unchanged.
- A private `struct TightParagraphs<I: Iterator<Item = Event>>` wraps the pulldown parser.

- [ ] **Step 1: Write the failing tests** (`tests/import_lists.rs`):

```rust
#[test] fn hard_break_in_tight_item_stays_in_its_paragraph() { /* "- a  \n  b\n" → list_item[paragraph["a", hard_break, "b"]]; schema_violations empty; serialise == "- a  \n  b\n" */ }
#[test] fn soft_break_in_tight_item_joins_with_space() { /* "- line one\n  continued\n" → one paragraph "line one continued" */ }
#[test] fn breaks_work_in_ordered_task_nested_and_quoted_lists() { /* one assertion each: serialise succeeds and schema clean */ }
#[test] fn image_and_board_in_tight_items_are_kept() { /* "- ![a](x.png)" and "- ![l](knot://board/<uuid>.svg)" → item has paragraph-or-image block, export contains them */ }
#[test] fn html_comment_between_item_lines_keeps_lines_apart() { /* "- a\n  <!-- c -->\n  b" → text contains "a" and "b" not "ab" */ }
#[test] fn empty_item_gets_an_empty_paragraph() { /* "- a\n-\n- c" → middle list_item[paragraph[]] ; re-export keeps three items */ }
#[test] fn rule_inside_item_or_quote_stays_in_its_container() { /* "> a\n>\n> ***\n>\n> b" → blockquote contains the horizontal_rule */ }
```

  Also add to `compat.rs`:
  - `new_import_is_schema_valid_on_every_input`, over all inputs;
  - the named classes `item_first_child_not_paragraph` (e.g. `- # h`) and `rule_in_container`.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-markdown --test import_lists`
  - Expected: the hard-break test fails with the damaged shape or a schema violation.
- [ ] **Step 3: Implement the adapter:**
  - emit `Start(Paragraph)` on the first inline event under an `Item`, and `End(Paragraph)` before the next block event, `Rule`, `Html` or `End(Item)`; `TaskListMarker` passes through;
  - remove the `list_item` branches in Text (362-369) and Code (387-392), and `ensure_para_child` where it is now unused;
  - at `TagEnd::Item`, insert an empty paragraph at index 0 if the first child isn't a paragraph;
  - make `Rule` use `push_block`.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-markdown`
  - Expected: all pass, including the compat harness. Any new differential failure must be a named class with a justification in the report.
- [ ] **Step 5: Commit** `fix(markdown): keep tight list item content inside its paragraph on import`.

### Task 3: Serializer for lists and containers (A4, A5, A6, A8, A9 export side)

**Files:**
- Modify: `crates/knot-markdown/src/to_markdown.rs`.
- Create: fixtures `nested_tasklists.md`, `list_blocks.md`.
- Test: `tests/export_lists.rs`.

**Interfaces:**
- `write_list_item(buf, txn, item, marker)`: the indent is the marker width only, and the checkbox is written as item content.
- A private `fn write_container(buf, txn, parent, in_quote) -> Result<(), SerError>`, shared by list items and blockquotes.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn nested_checklist_indents_by_marker_width() { /* [ ] a > [x] b > [ ] c → "- [ ] a\n  - [x] b\n    - [ ] c\n"; parse(serialise) structurally equal */ }
#[test] fn ordered_items_keep_checkboxes() { /* "1. [ ] a\n2. [x] b\n" round-trips */ }
#[test] fn list_item_with_several_blocks_round_trips() { /* [p,p], [p,image], [p,ordered start=3], [p,list,p], [p,quote,p] — each: fixed point and structurally equal */ }
#[test] fn blockquote_list_then_paragraph_round_trips() { /* blockquote[list, paragraph] */ }
#[test] fn rule_in_item_or_quote_is_not_turned_into_a_heading() { /* list_item[p "a", horizontal_rule] → re-import keeps paragraph + rule */ }
#[test] fn healthy_quote_and_empty_items_are_byte_identical() { /* "> a\n> b\n", "> a\n> ![x](u)\n", "- \n", "- [ ] \n", "2. \n" serialise exactly as legacy (P2) */ }
```

  Add to `compat.rs` the named classes `nested_tasklist`, `multi_block_item`, `quote_block_sequence`, `ordered_checkbox` and `rule_in_container`. Each must hold the fixed-point property under the new code.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cargo nextest run -p knot-markdown --test export_lists`
  - Expected: the nested-checklist test shows 6-space indentation.
- [ ] **Step 3: Implement the indentation, the ordered checkbox, and `write_container`.**
  - **No blank line between blocks when:**
    - the previous block is a heading, code block, rule or empty paragraph;
    - or the previous block is a paragraph and the next "can interrupt a paragraph": a code block, heading or blockquote; a bullet list whose first item has text; an ordered list starting at 1 that has text;
    - or, in a quote, paragraph → paragraph;
    - or, in a quote, paragraph → image/board/attachment (P2).
  - **Otherwise** write a blank line.
  - **Empty items** keep their trailing space (P2).
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-markdown`
- [ ] **Step 5: Commit** `fix(markdown): write nested and multi-block list items so they round-trip`.

### Task 4: Tolerant export (damaged shapes, unknown nodes)

**Files:**
- Create: `crates/knot-markdown/src/normalise.rs`.
- Modify: `to_markdown.rs`, `lib.rs`; `crates/knot-export/src/walk.rs` (P3); `crates/knot-server/src/routes/api/markdown.rs` (count unknowns); `crates/knot-obs/src/metrics.rs` (describe `knot_markdown_unknown_nodes_total`).
- Test: `tests/export_tolerant.rs`, `crates/knot-export/tests/walk.rs`.

**Interfaces:**
- `pub fn serialise_with_report(engine: &YrsEngine, doc: &DocHandle) -> Result<(String, Vec<String>), SerError>` (P4).
- `normalise::block_children(txn, parent) -> Vec<Child>`, where `enum Child { Block(XmlElementRef), Para(Vec<XmlOut>) }`. It applies P3, and adjacent runs with equal marks merge (with `{}` treated as no mark).

- [ ] **Step 1: Write the failing tests.**
  - **Markdown tests:**

```rust
#[test] fn damaged_hard_break_item_exports_healed() { /* hand-built list_item[p "first", hard_break, p "second"] → "- first  \n  second\n" */ }
#[test] fn damaged_soft_break_item_exports_healed() { /* list_item[p "line one", text " ", p "continued"] → "- line one continued\n" */ }
#[test] fn damaged_bold_runs_merge() { /* p[**bold**], " " stray, p[**more**] → "**bold more**" */ }
#[test] fn stray_text_at_top_level_and_in_cells_becomes_paragraph() {}
#[test] fn unknown_node_degrades_to_text_and_is_reported() { /* el("callout",[p "hi"]) → "hi\n", report == ["callout"]; serialise() also Ok */ }
```

  - **knot-export test:** `walk_glues_paragraph_stray_paragraph` gives one paragraph. `text_under_blockquote_is_wrapped` stays unchanged and green.
  - **Server test** (`markdown_integration` or new): GET markdown of a hand-built damaged page returns 200 with the healed text. On main it returns 500.
- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-markdown --test export_tolerant`
- [ ] **Step 3: Implement the normaliser, use it in every block container, add `serialise_with_report`, mirror P3 in knot-export, and count unknowns in the server.**
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-markdown -p knot-export`, then `cargo nextest run -p knot-server --test markdown_import_integration`.
  - The compat harness is green with a new named class `damaged_shape`.
- [ ] **Step 5: Commit** `fix(markdown): export damaged and unknown content instead of failing the page`.

### Task 5: Tasks from the Y.Doc, in lockstep with toggles (A11, A12)

**Files:**
- Create: `crates/knot-crdt/src/tasks.rs`.
- Modify:
  - `crates/knot-crdt/src/{lib.rs,room.rs}`: `patch_task_checked` uses `task_items`;
  - `crates/knot-markdown/src/tasks.rs`;
  - `crates/knot-server/src/routes/api/markdown.rs`: `refresh_markdown_inner` uses Y extraction.
- Test:
  - `crates/knot-markdown/tests/tasks_compat.rs`;
  - `crates/knot-server/tests/tasks_lockstep.rs`, using the `tasks_live_reindex.rs` harness.

**Interfaces:**
- `pub fn task_items<T: ReadTxn>(frag: &XmlFragmentRef, txn: &T) -> Vec<XmlElementRef>` in knot-crdt.
  - Pre-order.
  - It counts every `list_item` that has a `checked` attribute, including those inside table cells and ordered lists.
- `pub fn extract_tasks_from_doc(doc: &DocHandle) -> Vec<Task>` in knot-markdown.
  - `item_index` is the position in `task_items`.
  - `checked` is the attribute read as `"true"`.
  - Assignee and due date come from the item's own first paragraph's links, with today's rules.
  - Text follows P1.
- `extract_tasks(md)` stays, used by legacy and tests.

- [ ] **Step 1: Write the failing tests.**
  - **Unit:**
    - `nested_checklist_lists_parent_and_child_separately` gives `[(0,"parent"),(1,"child"),(2,"next")]`;
    - `table_cell_checklists_are_listed`;
    - `ordered_list_checkboxes_are_listed`;
    - `plain_bullet_starting_with_brackets_is_not_a_task`.
  - **Differential (`tasks_compat.rs`):**
    - for corpus docs where the legacy listing agrees with the room (same count, and `task_items[k]` is the item legacy listed as k), the new listing is identical in text, indexes and states;
    - the named class `task_with_nested_list` (P1) is exempt for text only.
  - **Lockstep property:** for every corpus doc and every k, `extract_tasks_from_doc[k]` is the same node `patch_task_checked(k)` flips. Verify by applying the patch on a clone and diffing.
  - **Server:** `toggle_from_tasks_flips_the_listed_item` for a nested checklist and for a checklist in a table. On main the wrong item flips.
- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-markdown --test tasks_compat`, then `cargo nextest run -p knot-server --test tasks_lockstep`
- [ ] **Step 3: Implement `task_items`, switch `patch_task_checked` to it (no behaviour change), implement `extract_tasks_from_doc`, and switch the server refresh.**
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-crdt -p knot-markdown -p knot-server`
- [ ] **Step 5: Commit** `fix(tasks): list checklist items from the page itself, in step with toggles`.

### Task 6: One-time refresh of cached Markdown and task rows

**Files:**
- Create: `migrations/20261024120100_markdown_renderer_version.sql`.
- Modify:
  - `crates/knot-storage/src/markdown_cache.rs`;
  - `crates/knot-markdown/src/lib.rs` (`pub const RENDERER_VERSION: i32 = 2;`);
  - `crates/knot-server/src/{routes/api/markdown.rs,reindex.rs,main.rs}`.
- Test: `crates/knot-storage/tests/markdown_cache.rs`, `crates/knot-server/tests/refresh_sweep.rs`.

**Interfaces (P17):**
- `MarkdownCacheStore::put_rendered(doc_id, seq, md, renderer_version: i32)`.
- `MarkdownCacheStore::stale_ids(current: i32, limit: i64) -> Result<Vec<Uuid>>`, which joins `documents` with `archived_at IS NULL` and orders by `updated_at`.
- `reindex::spawn` takes a `sweep: bool`, enabled in `main.rs`. Each flush appends up to 10 stale ids to `pending`.

- [ ] **Step 1: Write the failing tests:**
  - `stale_ids_returns_old_renders_only`;
  - `sweep_rerenders_stale_rows_and_reindexes_tasks`. Seed a cache row with `renderer_version = 1` and broken nested-checklist Markdown. Within the deadline, the row has version 2, healed Markdown, and correct task rows. A fresh version-2 row is untouched (same `updated_at`).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** the migration (`ALTER TABLE doc_markdown_cache ADD COLUMN renderer_version int NOT NULL DEFAULT 1`), the store methods, the refresh writing `put_rendered`, and the sweep.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-storage --test markdown_cache`, then `cargo nextest run -p knot-server --test refresh_sweep`
- [ ] **Step 5: Commit** `feat(markdown): refresh cached Markdown and tasks once after an upgrade`.

### Task 7: Real-data compatibility check (opt-in)

**Files:**
- Create: `crates/knot-server/tests/markdown_compat.rs`.
- Modify: `crates/knot-server/Cargo.toml` (dev-dependency feature `knot-markdown/legacy-compat`); `deploy/helm/knot/README.md` (one paragraph on how to run it).

**Interfaces:** `#[tokio::test] #[ignore] async fn real_data_compat()` reads `KNOT_COMPAT_DATABASE_URL`. It connects read-only with no migrations, hydrating like `export_check::hydrate`, then:
- for every page, serialises with legacy and new, classifies by named class, and prints counts and the first diff per class;
- fails if any healthy page outside the named classes changes;
- prints the sort-key groups that the heal migration would rewrite, from a `SELECT` mirroring its predicate;
- prints the number of stale cache rows.

- [ ] **Step 1: Write the test.** Also write a non-ignored `real_data_compat_runs_on_a_seeded_db` that points the same function at `fresh_db` seeded with a healthy page and a damaged page, and asserts the report: 1 unchanged, 1 `damaged_shape`.
- [ ] **Step 2: Run, expect FAIL** (function missing).
- [ ] **Step 3: Implement the check.**
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-server --test markdown_compat`
  - The `--ignored` path is documented, and the controller runs it against local databases.
- [ ] **Step 5: Commit** `test(markdown): opt-in real-data compatibility check`.

## Phase S: Sort keys

### Task 8: New sort-key generator

**Files:**
- Modify: `crates/knot-storage/src/{lexorank.rs,lib.rs}`.
- Reference: the scratchpad's `pr42-probes/lexo/newimpl.rs`.

**Interfaces:**
- `pub enum KeyError { Invalid(String), Unordered }`, deriving `Debug, PartialEq, Eq, thiserror::Error`.
- `pub fn is_valid(k: &str) -> bool`.
- `pub fn between(a: Option<&str>, b: Option<&str>) -> Result<String, KeyError>`. `(None, None)` gives `"m"`.
- `pub fn spread(n: usize) -> Vec<String>`.
- `lib.rs`: `pub fn sort_key_between(a, b) -> String` (P5).

- [ ] **Step 1: Write the failing tests** (in `lexorank.rs` `mod tests`):

```rust
#[test] fn top_inserts_never_collide() { /* 2000 × between(None, first) all distinct and descending; fails on main at #7 */ }
#[test] fn inserting_after_the_same_key_never_collides() { /* m, then 50 × between(Some(prev_anchor), Some(next)) — the "insert after first" chain; fails on main at #7 */ }
#[test] fn prefix_family_stays_inside() { /* all a ≤2 chars, b = a + '0'.. + c (≤3 chars): a < r < b */ }
#[test] fn exhaustive_small_alphabet() { /* alphabet ['0','1','2','9','a','m','y','z'], a ≤2, b ≤3, every valid a<b: valid(r) && a<r<b; fails on main */ }
#[test] fn simulation_2000_seeds() { /* xorshift ops: top insert, insert after, move; keys stay unique and ordered */ }
#[test] fn invalid_inputs_are_errors_not_panics() { /* "", "0", "00", "m0", "M", "-", equal and reversed → Err */ }
#[test] fn spread_is_sorted_valid_distinct() { /* n in {1,2,10,35,36,100,5000} */ }
#[test] fn appends_grow_slowly() { /* 1000 × between(last, None) ≤ 210 chars */ }
```

  The existing tests stay. `empty_returns_middle` is still `"m"`.
- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-storage --lib lexorank`
- [ ] **Step 3: Implement the generator,** porting the prototype (midpoint over `[0-9a-z]`, common prefix with `a` padded by `'0'`, no trailing `'0'`).
- [ ] **Step 4: Run, expect PASS** for the whole knot-storage suite: the existing `sort_key_between` callers are unchanged.
- [ ] **Step 5: Commit** `fix(sort-keys): generator that always finds a key between two neighbours`.

### Task 9: Transactional placement and rebalance

**Files:**
- Create: `crates/knot-storage/src/placement.rs`.
- Modify:
  - `doc_store.rs`: new trait methods; restore's top-level path uses placement; `siblings` and `list_alive` order by `sort_key COLLATE "C", id` (P7);
  - `crates/knot-server/src/routes/api/docs.rs`: create, move and from-template use placement; 422 anchors;
  - `web/src/features/docs/DocTree.tsx` (P12: `move.mutationFn` throws on error).
- Test:
  - `crates/knot-storage/tests/placement.rs`;
  - `crates/knot-server/tests/placement_integration.rs`;
  - `docs_integration.rs` (P6).

**Interfaces:**

```rust
pub enum Placement { First, Last, After(Uuid), Before(Uuid) }
// DocStore:
async fn create_placed(&self, ws: Uuid, parent: Option<Uuid>, title: &str, placement: Placement, created_by: Uuid) -> Result<Document, DocStoreError>;
async fn move_placed(&self, ws: Uuid, doc_id: Uuid, actor: Uuid, parent: Option<Uuid>, placement: Placement) -> Result<Document, DocStoreError>;
// DocStoreError gains: BadAnchor
```

- **One transaction, in this order:**
  1. Cycle check (move).
  2. Doc row lock (move).
  3. Group lock (P8).
  4. Read siblings inside the transaction (archived included, templates included).
  5. Validate the anchor: it must be a live, non-template sibling under the target parent and not the moved doc, else `BadAnchor`.
  6. `between`. On `Err` or length > 64, rebalance the group with `spread` (P7 deferred constraint) and retry once.
  7. Insert or update.
  8. Audit and invalidation as today.
- **Route mapping:**
  - create: no `after_id` → `First`; `after_id` → `After`;
  - move: `after_id` → `After`, else `before_id` → `Before`, else `Last`;
  - from-template → `Last`;
  - `BadAnchor` → `422 doc.bad_anchor`.

- [ ] **Step 1: Write the failing tests:**
  - `ten_new_pages_under_a_parent_all_succeed_in_order` (fails on main at #8 with 409);
  - `ten_new_top_level_pages_get_distinct_ordered_keys` (fails on main: duplicate keys);
  - `insert_after_first_ten_times`;
  - `placement_rebalances_invalid_and_duplicate_neighbours` (raw SQL seeds `"0"`, `"00"`, `"m0"`, `"M"` and top-level duplicates);
  - `concurrent_first_creates_get_distinct_keys` (20 `tokio::spawn`, under a parent and at the top level);
  - `move_with_non_sibling_anchor_is_422`;
  - `move_with_self_anchor_is_422`;
  - change `move_with_unknown_anchor_falls_to_end` to expect 422 (P6);
  - web: `move_error_rolls_back_and_toasts` (stub fetch returning 422).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement placement and switch the routes.**
- [ ] **Step 4: Run, expect PASS** for the full `knot-storage` and `knot-server` suites and the web tests.
- [ ] **Step 5: Commit** `fix(sort-keys): place pages in one transaction and rebalance instead of failing`.

### Task 10: Heal migration and top-level uniqueness

**Files:**
- Create: `migrations/20261024120000_heal_sort_keys.sql`.
- Modify: the two tests in P6 (distinct keys).
- Test: `crates/knot-storage/tests/migrations_apply.rs`, using the `VERSION` pattern at 179-233.

**Interfaces:**
- **Migration**, header in the `heal_doc_cycles` style; one transaction (P7):
  1. `ALTER TABLE documents DROP CONSTRAINT documents_workspace_id_parent_id_sort_key_key;`
  2. For every `(workspace_id, parent_id)` group with an invalid key (`NOT (sort_key COLLATE "C" ~ '^[0-9a-z]*[1-9a-z]$')`), a duplicate, or a key over 64 characters: rank the rows by `sort_key COLLATE "C", created_at, id` and assign `spread` keys.
     - Use a `pg_temp` base-36 function, `w` = the smallest width with `36^w > 2(n+1)`, key `i*36^w/(n+1)` with trailing `'0'`s stripped.
  3. `ALTER TABLE documents ADD CONSTRAINT documents_sort_key_unique UNIQUE NULLS NOT DISTINCT (workspace_id, parent_id, sort_key) DEFERRABLE INITIALLY IMMEDIATE;`

- [ ] **Step 1: Write the failing tests:**
  - `heal_sort_keys_leaves_healthy_groups_untouched` (byte-for-byte keys and `updated_at`);
  - `heal_sort_keys_preserves_order_and_fixes_bad_groups` (seed `"0"`, `"00"`, `"m0"`, `"M"`, a 70-character key, top-level duplicates, archived rows);
  - `heal_sort_keys_is_idempotent` (run the SQL again: no changes);
  - `top_level_duplicates_are_rejected_after_migration`.
- [ ] **Step 2: Run, expect FAIL** (migration missing).
- [ ] **Step 3: Write the migration and fix the two tests (P6).**
- [ ] **Step 4: Run, expect PASS** for `cargo nextest run -p knot-storage`, then the full workspace (stale migration embedding: `cargo clean -p knot-test-support -p knot-storage` if needed).
- [ ] **Step 5: Commit** `fix(sort-keys): repair existing page order keys and make top-level keys unique`.

### Task 11: Workspace import assigns fresh keys

**Files:**
- Modify: `crates/knot-server/src/routes/api/export_import.rs` (create loop at 539-570).
- Test: `export_import` integration.

**Interfaces:**
- The manifest `sort_key` is read but used only for ordering: per parent, order by (`sort_key COLLATE "C"`, manifest index).
- Docs are created with `create_placed(.., Placement::Last)` under each new parent, or under the import target.

- [ ] **Step 1: Write the failing tests:**
  - `importing_the_same_export_twice_keeps_every_doc` (top level; on main the second import skips colliding docs and regrafts their children);
  - `import_preserves_manifest_order`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS**, including `contributors_integration.rs:766` and `archive_integration.rs:369` unchanged.
- [ ] **Step 5: Commit** `fix(sort-keys): give imported pages fresh keys in their original order`.

## Phase T: Sidebar

### Task 12: Projection model (pure functions)

**Files:**
- Modify: `web/src/features/docs/tree.ts`, `tree.test.ts`.
- Reference: the scratchpad's `pr42-probes/dnd/projection-proto.mts`.

**Interfaces:**

```ts
export const INDENT = 12;
export type FlatRow = { id: string; parentId: string | null; depth: number; title: string; childCount: number };
export type Zone = "above" | "below" | "into";
export type MoveArgs = { parent_id: string | null; before_id?: string; after_id?: string };
export function flattenVisible(tree: TreeNode[], collapsed: ReadonlySet<string>, activeId?: string): FlatRow[];
export function dropZone(pointerY: number, rect: { top: number; height: number }, isOwnRow: boolean): Zone; // own row never "into"
export function projectDrop(flat: FlatRow[], docs: Doc[], activeId: string, overId: string, zone: Zone, depthDelta: number):
  { depth: number; minDepth: number; maxDepth: number; args: MoveArgs } | null; // null = no-op vs real parent/neighbours (P11)
export function keyboardMove(flat: FlatRow[], docs: Doc[], id: string, cmd: "up" | "down" | "indent" | "outdent"): MoveArgs | null;
export function describeMove(flat: FlatRow[], id: string, args: MoveArgs): string; // "Moved “B” into “A”, position 2 of 3"
```

- [ ] **Step 1: Write the failing tests** (a Vitest table):
  - **Prototype assertions:** un-nest the last child of the last root in place (`depthDelta = -1`); nest by dragging right; first-child clamp under an expanded parent; depths 0..2 below the last child; collapsed parent (`into` vs depth); dragged subtree excluded.
  - **Edge cases:** no-op cases return null (same parent and neighbours); a template's child rendered at the root is not reparented by a drop in place (P11); `into` appends as the last child.
  - **`keyboardMove`:** first and last rows, indent without a previous sibling (null), outdent at the root (null).
  - **Compatibility table:** for each gesture today's `moveArgs` supported (reorder before/after between siblings, `into`), the resulting order from applying `projectDrop` args equals the order from today's `moveArgs` args.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cd web && pnpm test src/features/docs/tree.test.ts`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** for `pnpm test`, `tsc` and `lint`.
- [ ] **Step 5: Commit** `feat(tree): projection model for nesting, un-nesting and reordering`.

### Task 13: DocTree drag rewrite

**Files:**
- Create: `web/src/features/docs/TreeRow.tsx` (split out).
- Modify: `DocTree.tsx`, `web/src/components/ContextMenu.tsx` (P10).
- Test: `DocTree.dnd.test.tsx`, `ContextMenu.test.tsx`.

**Interfaces:**
- **Drag and drop:**
  - `DndContext` with `sensors = [MouseSensor, TouchSensor]` (P9);
  - pointer-first `collisionDetection` that uses `args.pointerCoordinates` and returns no collision outside the tree's rect, so a drop outside cancels;
  - `measuring={{ droppable: { strategy: MeasuringStrategy.Always } }}`;
  - `onDragMove` computes `zone` and `depthDelta = Math.round(delta.x / INDENT)` and sets state only when the projection changes;
  - `onDragEnd` mutates only when the projection is non-null.
- **Rows:** each row `div` uses `useDraggable` + `useDroppable`. The `<li>` holds no dnd attributes. The row has `data-drop-zone` and `data-drop-depth` when hovered. The drop line is drawn at `left: 4 + depth*INDENT`. The DragOverlay ghost has no `doc-row-` testid.
- **Collapsed state:** a `collapsed: Set<string>` in DocTree. The dragged subtree is hidden during a drag.
- **Click suppression:** after a mouse drag, a capture-phase click listener on the tree element is armed at `onDragStart` and removed on the next tick after end or cancel.
- **Accessibility:**
  - `accessibility={{ announcements: titleBased, screenReaderInstructions: { draggable: "Drag to move. Use the row menu or Alt with the arrow keys to move by keyboard." } }}`;
  - no `role="button"` on rows, and no `aria-describedby` clash (P13).
- **ContextMenu (P10):** stops `onMouseDown` and `onTouchStart` propagation too.

- [ ] **Step 1: Write the failing tests** (Vitest, with the stubbed-fetch pattern from `DocTree.archive.test.tsx`):
  - rows have no `role="button"`;
  - the row link keeps `paddingLeft` and `onContextMenu`;
  - a drag simulation via the exported `handleDragMove`/`handleDragEnd` pure handlers (test the reducer, not dnd-kit internals) sends the projected args, and sends nothing for a no-op;
  - a drop outside sends nothing;
  - announcements contain titles, not ids;
  - ContextMenu: a `mousedown` on an item doesn't reach the parent's handler.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement**, keeping the P13 DOM contracts.
- [ ] **Step 4: Run, expect PASS** for the full web suite (the existing DocTree tests unchanged).
- [ ] **Step 5: Commit** `fix(tree): reliable drag and drop for nesting, un-nesting and reordering`.

### Task 14: Keyboard and menu moves

**Files:**
- Modify: `TreeRow.tsx`, `DocTree.tsx`.
- Test: `DocTree.keyboard.test.tsx`.

**Interfaces:**
- Context-menu items (`ctx-move-up`, `ctx-move-down`, `ctx-indent`, `ctx-outdent`) for editors. An item is disabled when `keyboardMove` returns null.
- `onKeyDown` on the row link:
  - Alt+ArrowUp / Alt+ArrowDown / Alt+ArrowRight / Alt+ArrowLeft call `keyboardMove` with `preventDefault`;
  - Shift+F10 and the ContextMenu key open the row menu at the link's bounding rect.
- A polite live region (`data-testid="tree-live"`) announces `describeMove`.
- After a move, focus is restored to `doc-row-<id>` by id after render.

- [ ] **Step 1: Write the failing tests:**
  - each menu item sends the right args and is disabled at the edges;
  - Alt+→ indents and the event's `defaultPrevented` is true;
  - Shift+F10 opens the menu positioned at the link;
  - the live region text;
  - focus is on the moved row after the refetch.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(tree): move pages by keyboard and from the page menu`.

### Task 15: Sidebar e2e

**Files:**
- Modify: `e2e/flows/tree-reorder.spec.ts`. Make it deterministic: seed via API (`createDoc` as in `archive.spec.ts`); drag helper with `steps: 10`; wait for `toHaveAttribute("data-drop-zone", …)` before release; assert with `expect.poll` on `parent_id` through `GET /api/docs`. Keep the five existing scenarios.
- Create: `e2e/flows/tree-moves.spec.ts`.
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

- [ ] **Step 1: Write the specs.** Run them on main, expect the un-nest, menu and Alt tests to FAIL, and record that in the report.
- [ ] **Step 2: Run on the branch, expect PASS 3 times in a row** (flake check): `cd e2e && pnpm playwright test flows/tree-reorder.spec.ts flows/tree-moves.spec.ts --repeat-each=3`. Local stack per the dev-stack notes.
- [ ] **Step 3: Commit** `test(tree): deterministic e2e for drag, keyboard and menu moves`.

## Phase D: Duplicate

### Task 16: `remap_refs`

**Files:**
- Create: `crates/knot-markdown/src/remap.rs`, `tests/remap.rs`.

**Interfaces:**
- `pub struct Remap { pub docs: HashMap<Uuid, Uuid>, pub boards: HashMap<Uuid, Uuid>, pub blobs: HashMap<Uuid, Uuid> }`.
- `pub fn remap_refs(doc: &yrs::Doc, map: &Remap) -> usize` (number of rewrites). It rewrites:
  - `excalidraw_board.board_id`;
  - `image.src` and `attachment.url` of the form `/api/blobs/<id>`;
  - link-mark `href`s `knot://doc/<id>` and `/api/blobs/<id>`, through `XmlText::diff` + `format`.

  Exact-id matching only, never substring: ids are parsed as UUIDs.

- [ ] **Step 1: Write the failing tests:**
  - every reference kind in nested tables and lists is rewritten;
  - unknown ids are left alone;
  - after remapping, the tree equals the source except for the mapped ids;
  - link-mark attributes other than `href` are preserved.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(duplicate): remap page, board and file references in a copied page`.

### Task 17: Storage for Duplicate

**Files:**
- Create: `crates/knot-storage/src/duplicate.rs`.
- Modify:
  - `blobs.rs`: `BlobStore::copy(src, dst) -> Result<()>`, defaulting to `get` + `put`;
  - `blobs/s3.rs`: `copy_object_internal`;
  - `doc_store.rs`: `live_subtree(ws, root) -> Vec<SubtreeRow{id,parent_id,title,icon,sort_key}>`, a CTE that stops at archived and template rows and skips their subtrees;
  - `audit.rs`: `record_in_tx_with_data`.
- Test: `crates/knot-storage/tests/duplicate.rs`.

**Interfaces:**

```rust
pub struct PageCopy { pub new_id: Uuid, pub parent: Option<Uuid>, pub title: String, pub icon: Option<String>, pub state: Vec<u8> }
pub struct BoardCopy { pub new_id: Uuid, pub new_doc_id: Uuid, pub label: Option<String>, pub state: Vec<u8>, pub svg: Option<Vec<u8>> }
pub struct BlobCopy { pub new_id: Uuid, pub new_doc_id: Uuid, pub source: BlobMetadata, pub bytes_in_db: bool }
pub struct DuplicatePlan { pub ws: Uuid, pub actor: Uuid, pub source_id: Uuid, pub top: PageCopy /* placed After(source) */, pub pages: Vec<PageCopy>, pub boards: Vec<BoardCopy>, pub blobs: Vec<BlobCopy> }
pub async fn apply_duplicate(pool: &PgPool, plan: &DuplicatePlan) -> Result<Vec<(Uuid, i64)> /* (page, first update seq) */, DocStoreError>;
```

- **One transaction:**
  1. Place the top page with `Placement::After(source)` (Task 9).
  2. Insert subpages in source order with `spread` keys.
  3. For each page: insert one `doc_updates` row with the actor, plus `doc_contributors` (the SQL from `insert_batch`).
  4. Insert boards plus one `board_updates` row and `svg_cached`.
  5. Insert blob rows. For DB-stored bytes: `INSERT INTO blob_bytes SELECT $new, bytes FROM blob_bytes WHERE blob_id = $old`.
  6. Audit `doc.duplicate` with `{"source_id": …}`, and invalidations.

- [ ] **Step 1: Write the failing tests:**
  - `apply_duplicate_inserts_everything_atomically`;
  - a forced unique conflict leaves no rows (`no_partial_rows_on_failure`);
  - `live_subtree_skips_archived_and_template_subtrees`;
  - `copy_default_impl_copies_bytes`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(duplicate): store a page copy with its subpages, drawings and files in one transaction`.

### Task 18: Duplicate route

**Files:**
- Create: `crates/knot-server/src/duplicate.rs`.
- Modify: `routes/api/docs.rs`: register `POST /api/docs/{id}/duplicate` in `doc_id_routes`.
- Test: `crates/knot-server/tests/duplicate_integration.rs`, based on the `export_integration.rs` harness (`world_with`, `set_content`, `upload`, `archive`, `member`) and on `ScriptedBlobStore` from `archive_integration.rs`.

**Interfaces:**
- **Handler:**
  1. Check roles (P15).
  2. Refuse templates (422).
  3. Read `live_subtree` when subpages are included, else the source alone.
  4. Enforce the caps (413).
  5. For each page: `live_snapshot`, a transient doc, then `remap_refs`, then `encode_state_as_update`.
  6. Boards: merge snapshot + updates, encode once.
  7. Blobs: copy only those whose page the user can read (the rule from `export.rs` `resolve_image`). S3 copies happen before the transaction, and a detached cleanup task runs on failure (P16).
  8. `apply_duplicate`.
  9. After commit, best effort for each copy: serialise + `put_rendered` + Y-walk task extraction from the transient doc. Rooms are not acquired.
  10. Return `201` with `to_response(&top)`.

- [ ] **Step 1: Write the failing tests:**
  - `duplicate_is_lossless` (`colwidth`/`colspan`, multi-block cells, an attachment node, a list line break; markdown export of the copy returns 200);
  - `copy_sits_right_after_the_source`;
  - `copy_title_and_icon`;
  - `board_is_copied_and_independent` (new id, same state and SVG; editing the copy leaves the source untouched);
  - `images_survive_archiving_and_purging_the_original`;
  - `subpages_copied_with_internal_links_remapped`;
  - `subpages_not_copied_by_default`;
  - `unreadable_blobs_are_not_copied`;
  - `archived_source_is_409`;
  - `viewer_is_403`;
  - `template_is_422`;
  - `too_large_is_413`;
  - `duplicate_failure_leaves_no_rows_or_objects` (Review Focus 5);
  - `byline_credits_the_duplicating_user`;
  - `copied_tasks_are_indexed`.
- [ ] **Step 2: Run, expect FAIL** (404).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(duplicate): duplicate a page, optionally with its subpages`.

### Task 19: Duplicate in the sidebar

**Files:**
- Create: `web/src/features/docs/DuplicateDialog.tsx`, `DuplicateDialog.test.tsx`.
- Modify: `docs.api.ts` (`duplicate(id, body): Promise<ApiResult<Doc>>` using `parse(Doc, r.ok)`), `TreeRow.tsx` (menu item `ctx-duplicate` "Duplicate…" for editors), `DocTree.tsx` (renders the dialog outside the `DndContext`).
- Follow the mockup.

**Interfaces:**
- **When the dialog appears:** only when `countDescendants(node) > 0`. Otherwise the copy is made immediately.
- **Dialog:** `role="dialog"`, `aria-modal`, Escape closes it, focus is trapped, and focus returns to the row link. The checkbox is unticked by default.
- **Success:** invalidate `["docs"]`, then `markDocEditMode(id)`, then navigate to `/doc/<id>`.
- **Errors:** toasts per Global Constraints. 413 uses the too-large copy; everything else uses "Couldn't duplicate this page."

- [ ] **Step 1: Write the failing tests:**
  - the item is shown for editors and not for viewers;
  - no dialog appears without subpages (exactly one POST with `include_subpages: false`);
  - the dialog appears with N in its text and an unticked default;
  - ticking sends `true`;
  - Escape closes it and returns focus;
  - a 413 shows the too-large toast;
  - navigation happens on success;
  - a `docsApi.duplicate` contract test.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
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
