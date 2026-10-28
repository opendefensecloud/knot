# Markdown lists, sort keys, sidebar drag and drop, and Duplicate: design

**Date:** 2026-10-24
**Status:** Approved (brainstorm)
**Origin:** PR #42 by @szEvEz. It reports these four problems and proposes partial fixes. We are not merging it, because its commit signature is unverified and the author hasn't responded. This is our own reimplementation, and its PR credits the report.
**Mockup (Duplicate, approved):**
[`2026-10-24-duplicate-mockup.html`](2026-10-24-duplicate-mockup.html).
(Amended in Task F5; this first linked a private copy of the same mockup on
claude.ai.)

## Intent

Fix four real problems that PR #42 surfaced, completely and at the root, and
do it better than the PR did:

1. Pages with certain lists can't be exported or indexed, and restores or
   templates can damage pages.
2. Page ordering keys run out or collide, so creating or moving pages fails.
3. Nesting and un-nesting pages by drag and drop in the sidebar is unreliable,
   and impossible by keyboard.
4. There is no way to duplicate a page.

Investigation showed each bug is wider than the PR assumed. The PR's fixes
cover one path per area.

## Requirements from the owner (binding)

- **Duplicate** (approved mockup):
  - The right-click menu gets "Duplicate…". When the page has subpages, a
    dialog asks whether to include them.
  - If they are included, every subpage is copied with everything: content,
    drawings and files.
  - The copy is fully independent of the original.
- **Tests for everything not yet covered.** These are real bugs our test
  setup missed. Every bug in this document gets a test that fails on today's
  code and passes after the fix (RED shown in the PR). Each area's
  missing-coverage class also gets a systematic test, not only the single
  case.
- **Existing pages and content must not break, only improve.** This is the top
  constraint. See "Compatibility guarantees", which is a deliverable, not a
  hope.

## Goals

- Every bug listed under "Problems" is fixed and pinned by a test.
- No stored page, ordering or export changes, except where today's behaviour
  is broken or lossy. The compatibility harness proves it.
- Pages already damaged by the old importer export correctly again, without a
  data migration.
- Sidebar moves (nest, un-nest, reorder) work deterministically by pointer,
  touch and keyboard.
- Duplicate with optional subpages, lossless and independent.

## Non-goals

- Markdown escaping of literal syntax (`\*x\*`, `\[x\]`, `1\.`). It is a real
  issue but a separate one; file a follow-up issue.
- Copying comments, page history, share links or per-page permission grants on
  Duplicate.
- Persisting the sidebar's expanded/collapsed state across reloads.
- Changing the docx/odt export output beyond sharing the stray-content repair
  rule.

## Anti-goals

- **Any change to a healthy page's stored content, sort order, or Markdown
  output.** Here "healthy" means a page that exports and round-trips correctly
  today.
- A migration that reorders pages, loses rows, or can fail on real data.
- `/tasks` checkbox toggles hitting the wrong item after the change.
- A Duplicate whose copy silently depends on the original: shared drawings,
  images that disappear when the original is archived, links into the
  original's subtree.

## Problems (verified on main with probes)

### A. Markdown list round trip (`crates/knot-markdown`)

**Root cause.** pulldown-cmark omits paragraph tags inside tight list items.
`from_markdown.rs` patches this per event type, and HardBreak, SoftBreak,
Image and HTML were missed.

**Failures.**

1. **A hard or soft break inside a tight list item stores**
   `list_item[paragraph, hard_break | " ", paragraph]`, and `serialise` then
   fails with `UnsupportedNode`. This happens in bullet, ordered, task and
   nested lists, and in lists inside quotes.
   - Every consumer breaks: GET markdown (500), the `markdown_cache` (search,
     public share, zip export, `/tasks`), history preview and restore (500),
     and creating a page from such a template (500).
2. **Images and board sentinels in tight list items are dropped**, because
   promotion needs a paragraph on the stack. `- ![a](x)` stores an empty
   `list_item`, and the whole list disappears on export.
3. **An HTML comment between item lines merges the two lines:** `a…b` becomes
   `"ab"`.
4. **Nested checklists are indented by the width of `"- [x] "`** (6 spaces).
   Re-import turns the children into literal text in the damaged shape, so the
   *second* export fails.
5. **List items with several blocks are written with no blank lines:**
   - two paragraphs collapse;
   - a paragraph followed by an image drops the image;
   - a paragraph followed by an ordered list starting at 3 turns into text;
   - a trailing paragraph moves inside the nested list or quote.
6. **Blockquotes** have a narrower multi-block problem.
   - Two paragraphs, and a paragraph followed by an image, round-trip today and
     must stay byte-identical.
   - What fails: a list followed by a paragraph (the paragraph moves into the
     list, damaged shape), a paragraph followed by an ordered list starting at 3
     (it turns into text), and a paragraph followed by a rule (it turns into a
     level-2 heading).
7. **Empty list items vanish on the *second* pass.** The first export writes
   `- ` / `- [ ] ` / `2. `. Re-import stores `list_item[]` with no paragraph,
   and the next export drops it. The fix is on the import side. The
   serializer's output for empty items stays byte-identical.
8. **Checkboxes in ordered lists are dropped on export.** The room still counts
   them for `/tasks` toggles, so on such pages a toggle can hit the wrong item
   today.
9. **Horizontal rules in list items or quotes break both ways.**
   - Import puts a rule inside a quote or list item at the top level, right
     after the enclosing container (`from_markdown.rs:402-405`).
   - Export writes an editor-made `[paragraph, horizontal_rule]` in an item or
     quote as `"- a\n  ---"`, which re-imports as a level-2 heading "a", and
     the rule is lost.
10. **History restore, from-template and zip import all go serialise → parse,**
    so they *create* damaged pages.
    - The most common trigger is (1). An editor line break (Shift+Enter) in a
      list item exports fine as `- a  \n  b`, re-imports in the damaged shape,
      and then fails to export.
    - (2), (4), (5) and (9) do the same.
11. **`/tasks` shows the merged text `"parent - [  ]  child"` today.** That
    comes from (4): the child is written as a continuation of the parent's
    line. `extract_tasks` also keeps only one current item, so with correctly
    indented input it drops the parent:
    `"- [ ] parent\n  - [ ] child\n- [ ] next"` gives `[(0,"child"),(1,"next")]`.
    Fixing (4) alone would therefore make parent tasks vanish.
12. **`/tasks` listing and toggling disagree on several shapes.** The listing
    reads serialised Markdown. Toggles walk the Y.Doc (`patch_task_checked`,
    pre-order, counting every `list_item` with a `checked` attribute).
    - Checklists in table cells are flattened into cell text, so they're not
      listed but are counted.
    - Checkboxes in ordered lists (8) are not listed but are counted.
    - A plain bullet whose text starts with `[ ] ` is listed but not counted.

    On those pages a toggle from `/tasks` can flip the wrong item today.

### B. Sort keys (`crates/knot-storage/src/lexorank.rs`)

1. **`decrement` returns all-`'0'` keys** (`"1"` → `"0"`), and nothing sorts
   before `"0"`.
   - "New page" inserts first at its level, so new keys at a level go
     `m, b, 5, 2, 1, 0, 0m(wrong order), 0m(dup)`.
   - Under a parent, the 8th new page fails with 409 `doc.sort_key_conflict`
     ("Couldn't create document"), and the level stays blocked.
   - At the top level the UNIQUE constraint treats NULL parents as distinct.
     The 8th page silently gets a duplicate `0m` (201), and order becomes
     nondeterministic.
2. **`interpolate` ignores `b`'s suffix when `a` is a prefix of `b`.**
   - Inserting after the same page 7 times returns its own neighbour on the
     7th: a duplicate, so 409.
   - 595 of 30.2M exhaustive pairs fall outside the interval. Random
     simulations fail with or without PR #42.
3. **Duplicate keys and panics.**
   - The UNIQUE constraint treats NULL parents as distinct, so top-level
     duplicates are silently accepted and ordering becomes nondeterministic.
   - Archive migration and workspace import can also create duplicates.
   - Duplicate neighbours hit `assert!(a < b)`. An invalid key panics at
     `assert!(is_base36…)` with two neighbours, or in `ord()` when only one is
     given. Both **panic the request**.
   - Keys with characters below `'0'` don't panic but sort wrong:
     `between(None, "-")` returns `"-m"`.
4. **Every append adds one character** (1000 appends give a 1001-character
   key).
5. **Concurrent placements race,** because siblings are read outside the
   transaction.
6. **Workspace import trusts the manifest's keys.** On a collision it skips
   the doc and reparents its children.

### C. Sidebar drag and drop (`web/src/features/docs/DocTree.tsx`, `tree.ts`)

1. **The drop intent is computed only in `onDragOver`, which fires when the
   hovered row changes.** Smooth drags enter rows at an edge, so "into" (nest)
   happens only by luck of mouse speed. The e2e test teleports the pointer to
   compensate, and the "nest" test is flaky.
2. **The droppable is the whole `<li>` including its expanded subtree,** so
   hit boxes and zones are wrong for parents.
3. **A child can't be moved to its parent's level after its parent** when that
   parent is the last row (un-nesting).
4. **Sortable transforms slide rows using server order,** not screen order.
   What the user sees doesn't match the hit boxes.
5. **Keyboard drags can only nest,** and Left/Right do nothing.
6. **Releasing a drag on the row you started from can follow the link's
   `href`** (full page load). dnd-kit only stops propagation.
7. **Screen readers hear UUIDs.** The rows also misuse `role="button"`.
8. **Touch drags fight scrolling.**
9. **No-op drops still POST** a move and re-key the page.

### D. Duplicate (new)

- **Missing feature.** PR #42's client-side `createFromTemplate` + `move`:
  - is lossy (it goes through Markdown);
  - is not atomic (the move result is ignored);
  - keeps drawings and images linked to the original;
  - doesn't copy subpages.

## Design

### A. Markdown

**Importer (`from_markdown.rs`):**
- **One event adapter instead of per-event special cases.** Wrap pulldown's
  events: on the first inline event directly under an `Item`, emit
  `Start(Paragraph)`. Before the next block-level event or `End(Item)`, emit
  `End(Paragraph)`. Remove the `list_item` special cases in Text and Code.
  This fixes A1, A2 and A3 at the root. A new inline event type can't
  reintroduce the bug.
- **Schema-valid items.** At the end of each item, an item that doesn't start
  with a paragraph gets an empty one at index 0, because the schema requires
  `list_item: paragraph block*`.
- **Rules are written into their container,** not the top level (A9).

**Serializer (`to_markdown.rs`):**
- **Indentation (A4).** Nested content is indented by the list *marker* width
  only (2 for `- `, `n. `'s width for ordered). The checkbox is item content.
- **Checkboxes (A8).** Written for ordered items too.
- **Containers (A5, A6).** One container writer is shared by list items and
  blockquotes. It puts a blank line between child blocks unless the next block
  can safely follow a paragraph directly. The exact rule set is in the
  prototype (see "Left to the builder").
- **Empty items (A7)** keep today's exact output (`- ` / `- [ ] ` / `n. `).
  The fix is the importer's empty paragraph.
- **Tolerant export (repairs damaged pages on the way out).**
  - In every block container (top-level fragment, list item, blockquote, table
    cell), group stray `XmlText` / `hard_break` runs. Glue the shape
    `[paragraph, stray-run, paragraph]` back into one paragraph: that is the
    line the old importer split.
  - A stray run with no neighbouring paragraph becomes its own paragraph.
  - Adjacent text runs with identical marks merge, so `**a**** ****b**` is
    never emitted.
  - Damaged pages therefore export correctly *and* heal on the next restore or
    import. **No data migration.**
- **Unknown nodes** degrade to their text plus a `warn` log and a counter
  instead of failing the whole export. Today one bad node breaks caching,
  search, restore and templates for that page. Mirror the knot-export
  behaviour, and share the stray-grouping helper with `crates/knot-export`
  (its walker currently makes three paragraphs where the glue rule makes one).
- **One-time refresh of derived data (my call).**
  - **Why it's needed:** zip export, public shares and search read
    `doc_markdown_cache` regardless of its age, and task rows come from the
    last reindex. Without a refresh, untouched pages keep their broken or
    lossy cached Markdown and wrong task indexes after the upgrade.
  - **How it works:** add a `renderer_version` column to `doc_markdown_cache`;
    a row whose version is older is stale. After startup, the existing reindex
    worker re-renders stale rows gradually: rate-limited, oldest first,
    skipping archived pages. It goes through the same path as an edit, so
    tasks are reindexed too.
  - **What it touches:** derived data only (cache, search text, task rows).
    Page content is never rewritten.
  - The real-data check is this refresh's dry run.

**Tasks (A11, A12).**
- **Extraction walks the Y.Doc,** using the same pre-order walk and counting
  rule as `patch_task_checked`: every `list_item` with a `checked` attribute.
  It no longer reads the serialised Markdown, so listing and toggling cannot
  disagree on any shape: nested items, checklists in table cells, checkboxes in
  ordered lists.
- **Task text, assignee and due date follow today's rules.** The text is the
  item's own paragraph text, without its children's. The assignee is the
  item's first inline mention, and the due date its first inline date link.
- **No change where today's listing is right.** Task texts and indexes stay
  the same for pages where today's listing agreed with the room. They change
  only where it was wrong.
- **Required lockstep property test** over the whole corpus: listing index *k*
  and toggle index *k* hit the same node.
- **Storage unchanged.** Row ids stay `doc_id:item_index`, and how
  `completed_at` behaves on reindex doesn't change.

### B. Sort keys

- **Generator.** Replace it with a fractional-index midpoint (rocicorp-style,
  without its integer head) over `[0-9a-z]`.
  - `between(a, b) -> Result<String, KeyError>`. It never panics.
  - It validates neighbours as `^[0-9a-z]*[1-9a-z]$` and never emits a
    trailing `'0'`.
  - Appends grow about 5× slower.
  - `spread(n)` gives n evenly spaced distinct keys.
  - A prototype passes the exhaustive and random checks with zero failures.
- **Placement in the store, in one transaction.**
  - `DocStore::create_placed` and `move_placed` take
    `Placement::{First, Last, After(id), Before(id)}`.
  - They lock the sibling group: the parent row `FOR NO KEY UPDATE`, or a
    transaction-scoped advisory lock per workspace for the top level. A move
    locks the target parent and the moved page together. (Amended in
    Task 10; this first read the parent row `FOR UPDATE`. With the
    deferrable constraint below, neither `parent_id` nor `sort_key` is a key
    column, so `FOR NO KEY UPDATE` is enough.)
  - A move into a page first takes a per-workspace move lock, so two
    concurrent moves can't both pass their cycle check and leave pages in a
    parent cycle. (Amended in Task F2.)
  - They read siblings, archived included, inside the transaction.
  - On `KeyError`, or a key over 64 characters, they **rebalance the
    group** with `spread`, in one `UPDATE` statement: the unique constraint
    is `DEFERRABLE INITIALLY IMMEDIATE`, so it is checked at the end of that
    statement, and keys may repeat while it runs. Then they generate again.
    (Amended in Task 10; this first read "in two steps because UNIQUE is not
    deferrable".)
  - Routes (create, move, from-template, restore, import) use these.
- **Heal migration** (`migrations/20261024120000_heal_sort_keys.sql`, in the
  `heal_doc_cycles` style):
  - It rewrites only (workspace_id, parent_id) groups that contain an invalid
    key (empty, outside `[0-9a-z]`, trailing `'0'`), a duplicate, or a key
    over 64 characters.
  - It preserves the order `sort_key COLLATE "C", parent_id NULLS FIRST,
    created_at, id`: the sidebar's own order, which lists top-level pages
    before template children on a key tie. `parent_id` is the same within a
    group, so it only decides between the groups of a root list. (Amended in
    Task 9; this first read `sort_key COLLATE "C", created_at, id`.)
  - Archived rows are included. It is idempotent and a no-op on healthy data.
  - Then it swaps the constraint to `UNIQUE NULLS NOT DISTINCT (workspace_id,
    parent_id, sort_key) DEFERRABLE INITIALLY IMMEDIATE`. (Amended in Task 9;
    this first had no `DEFERRABLE`.)
  - Sibling and list queries add `id` as a final tie-breaker.
- **Workspace import** ignores manifest keys. It assigns fresh keys in the
  manifest's per-parent order.

### C. Sidebar drag and drop

dnd-kit's SortableTree projection model, as pure functions in `tree.ts`:

- `flattenVisible(tree, collapsed, activeId?)` returns rows in on-screen
  order. It skips children of collapsed rows and the dragged row's subtree.
- `projectDrop(flat, docs, activeId, overId, zone, depthDelta)` returns
  `{ depth, args } | null`. (Amended in Task F6, as the plan's P11 has it;
  this first read `projectDrop(flat, activeId, overId, pointerY, rowRect,
  deltaX)`.)
  - Vertical position picks the gap: `zone` is
    `dropZone(pointerY, rowRect, isOwnRow)`.
  - `depthDelta = Math.trunc(deltaX / INDENT)` (shared constant, 12px) picks
    the depth, clamped between the next row's depth and the previous row's
    depth + 1. It is truncated toward zero, so a level takes a full INDENT
    of sideways movement, and the slip that starts a drag during a click
    (the mouse sensor starts a drag past 6px of travel) can't change the
    level. (Amended in Task F4; Task 13 first rounded, which made a 7px slip
    nest or un-nest the page.)
  - `docs` gives each page's real parent, which anchors come from.
  - Hovering one's own row means "same gap, new depth" (un-nest in place).
  - It returns `null` for no-ops.
- `keyboardMove(flat, id, "up" | "down" | "indent" | "outdent")` and
  `describeProjection()` (title-based announcements).
- **`DocTree.tsx`:**
  - `useDraggable`/`useDroppable` on the row `div`, not the `<li>`.
  - A memoized flat list.
  - Pointer-first collision, using the collision detector's coordinates so
    intent and `over` agree.
  - The drag ghost (`DragOverlay`), and the dragged subtree hidden while
    dragging.
  - The drop line is drawn at the projected depth.
  - No DOM queries during a drag, and state updates only when the projection
    changes.
  - The post-drag click is suppressed only after pointer drags, only inside
    the tree.
  - A drop released less than one INDENT from the press (the straight,
    Euclidean distance) moves nothing, and while the pointer is that near
    no drop line shows. Such a drop counts as a click only when the
    pointer's whole path, from the press through every position to the
    release, is under one INDENT too: a plain press on a row's link then
    opens that page in the app (no reload, no move request), replacing the
    history entry when the page is already open. A drag that goes out and
    comes back near the press moves nothing and opens nothing, as does any
    longer drag that moves nothing. On touch this applies after the 250ms
    hold. (Amended in Task F4: the browser sends no click after a drag, so
    the drag's end opens the page. Amended in Task F6: the rule first
    covered only drags that moved nothing, and a click pressed at a row's
    top edge that slipped a few px up could still nest the page into the
    row above. Amended in the F6 review's fix round: F6 measured the path
    alone, so a hand that jittered on the way made the path one INDENT long
    and an 8px slip up still nested the page; the distance from the press
    now decides whether a drop moves anything, the path whether it is a
    click.)
  - No-op drops send nothing.
  - Dropping outside the tree cancels.
  - Custom drag announcements use titles.
  - A TouchSensor with a 250ms press delay.
  - Collapsed state is lifted to the tree (not persisted).
- **Keyboard (my call): explicit commands instead of dnd-kit's keyboard
  drag.**
  - **Commands:** context-menu items "Move up", "Move down", "Indent",
    "Outdent" (WCAG 2.5.7), plus Alt+↑/↓/→/← while a row's link has focus.
  - **Announcements:** each move is read out by title in a polite live region.
  - **Opening the menu:** Shift+F10 or the ContextMenu key opens the row menu.
  - **KeyboardSensor removed.** Its only working gesture today (Space,
    ArrowDown, Space = nest) is replaced by Alt+→.
  - **Focus** stays on the moved row's link after a move.
- **Semantics (my call):**
  - drop the incorrect `role="button"` on rows;
  - keep the list structure, with the row link as the focus target;
  - describe each row's level and number of subpages through
    `aria-describedby` ("level 2, 3 subpages");
  - a full WAI-ARIA `role="tree"` keyboard model is a follow-up issue, not
    part of this PR.
- **Server:** `move` rejects an anchor that isn't a sibling under the target
  parent (`422 doc.bad_anchor`) instead of silently appending.

### D. Duplicate

- **`POST /api/docs/{id}/duplicate` `{ include_subpages: bool }` → 201
  `DocResponse`.**
  - It requires editor on the source and the right to create under its
    parent.
  - An archived source gives 409.
- **One transaction** (`knot_storage::duplicate`):
  - the new page sits right after the source;
  - its title is `"<title> (copy)"`, and subpages keep their titles;
  - the icon is copied;
  - the content is the **CRDT state copied losslessly** from the live
    snapshot, with ids remapped by a pure `remap_refs` function;
  - each board gets a new row and copied state + SVG;
  - blob rows and bytes are copied (S3: copy before commit, clean up on
    failure);
  - the duplicating user is credited as creator and contributor;
  - an audit event `doc.duplicate` records the source.
- **With subpages:** all live descendants are copied in the same structure,
  each fully. Links between copied pages are remapped to the copies; links
  elsewhere stay as they are.
- **Not copied:** comments, history, share links, grants.
- **Tasks:** checklist assignees are copied with the content (a copy is a
  copy), so the tasks appear again in `/tasks` for their assignees.
- **Client:**
  - `docsApi.duplicate`, and a "Duplicate…" context-menu item for editors.
  - The dialog appears only when the page has subpages, with the checkbox
    "Include its N subpages" unticked by default.
  - The page is then refreshed and the app navigates to the copy (edit mode
    like other creates).
- Files referenced from pages the user can't read are not copied; the
  reference is left as it is. `from_template` stays unchanged.
- **Templates (my call):**
  - duplicating a template page is refused (`422 doc.is_template`);
  - descendants that are templates are skipped, since they aren't part of the
    visible tree;
  - copies are never templates.
- **Size limits and atomicity (my call):**
  - **Caps:** the subtree is limited to 200 pages and 200 MiB of files.
    Beyond that the server returns `413 doc.duplicate_too_large`, and the
    dialog shows the message.
  - **Order of work:** all snapshots, board states and file copies are
    prepared *before* the transaction opens. The transaction only inserts
    rows, so it stays well under the 30 s request timeout and the
    idle-transaction limit.
  - **Cleanup:** copied S3 objects are recorded first. A detached cleanup task
    removes them if the transaction doesn't commit, so a dropped request can't
    orphan them. (Amended in Task 17: they are recorded in the table
    `blob_copy_pending`, committed before the first copy, and the hourly
    purge job removes those whose copy never committed once they are an hour
    old. Nothing is deleted on the request path, so a copy whose commit
    answer was lost keeps its files.)
  - **Concurrency:** a server runs two duplicates at a time. A request that
    gets no slot within 5 s answers `503 doc.duplicate_busy` with
    `Retry-After: 5`. The answer comes once the copy commits; the refresh of
    the copies' derived data runs after it, still holding the slot.
    (Amended in Task F3.)

## Compatibility guarantees (deliverable)

1. **Markdown differential harness.**
   - **Frozen reference.** Today's `to_markdown.rs` and `from_markdown.rs`
     are frozen as test-only reference copies (`crates/knot-markdown/tests/legacy/`).
     (Amended in Task 1, per the plan's P20: they live in
     `crates/knot-markdown/src/legacy/`, behind the `legacy-compat` feature,
     so the server's real-data check can use them. Production builds don't
     enable it.)
   - **Corpus:**
     - all fixtures;
     - all probe cases, copied into the repo as fixtures (amended in Task 1,
       per P21: as `crates/knot-markdown/tests/probes/*.md`, used only by
       the harness, since every fixture must round-trip byte for byte);
     - a generated set of documents: combinations of every node and mark,
       nesting up to 4, editor-shaped attributes (link `target`/`rel`/`class`,
       table `colwidth`/`colspan`, Bool `checked`, `{}` marks) and
       import-shaped ones;
     - a large randomized set from a seeded generator.
   - **"Healthy" is defined at the Markdown level (my call).** A document is
     healthy when its legacy export `m` succeeds and is a fixed point:
     `legacy_serialise(legacy_parse(m)) == m`. This includes editor pages with
     attributes Markdown can't express, since those don't affect the output.
   - **Serializer rule 1 (healthy).** The new export is **byte-identical**.
     The only exceptions are documents matching an explicit, named predicate of
     a documented bug class: A4, the failing A5/A6 shapes, A8, A9, and damaged
     shapes.
   - **Serializer rule 2 (everything else).** The new export must be a fixed
     point under the *new* parse and serialise, and must contain every text
     character of the legacy export's text content. No text is ever lost.
   - **Importer rule.** For every Markdown input whose legacy import is
     schema-valid and legacy-exports to a fixed point, the new import is
     identical under a normaliser comparing tags, attribute values as strings,
     text, and mark keys *and* values. Only named bug classes may differ.
   - **Depth limit.** The writer and the importer follow a page at most 128
     block containers deep (blockquotes, list items, table cells:
     `knot_crdt::nesting::MAX_BLOCK_DEPTH`, the writer's `MAX_DEPTH`). The
     writer writes a deeper blockquote, list or table as one paragraph of
     its text, in order, and the importer stores such Markdown the same way.
     So a page nested thousands of levels deep, which aborted the server on
     main too, can't overflow the stack in a walk of the writer or the
     importer, and no text is lost. Rooms refuse an update that nests a
     page's types past `MAX_NESTING` (4 × 128). The named class
     `depth_limit`, for exports and for imports, covers every document and
     input the limit changes. It is checked before the other classes and
     before legacy, which recurse once per element. (Amended in Task F1.)
   - **Room refusals.** A room, page or board, refuses an inbound update and
     closes its sender with 4422 when the update nests the page's types past
     `MAX_NESTING`, or doesn't decode. An update decodes when yrs's decoder
     reads it: knot's reader, which reads it first, reads exactly what yrs
     reads (a differential test over the repo's update corpora pins it),
     and also refuses clocks past `u32::MAX`. Nothing of a refused update is
     persisted or fanned out. Only an update the room had already applied
     (the check after apply) also evicts the room, closing every other
     connection with 1012, so the next join reloads the page from storage.
     An update yrs rejects part-way is measured over what the apply
     integrated: past the limit it is refused as above; within it, the room
     does what main does: it neither persists nor fans out the update, and
     keeps the sender and the rest of the room. yrs's error need not be the
     sender's: within the same apply it retries the blocks it parked
     earlier for a missing dependency and passes on their error. (Amended
     in Task F6, and in the F6 review's fix round: the reader also reads
     the delete set, as yrs does.)
2. **Real-data check (opt-in).**
   - It runs the old and new serializers over every page in a database:
     `cargo test -p knot-server --test markdown_compat -- --ignored`, with
     `KNOT_COMPAT_DATABASE_URL` set.
   - It is read-only, with no migrations. It reports pages whose output would
     change, by class, with diffs.
   - It fails on a page whose export changes and would lose text, or whose
     new export fails. It lists, reported only, the pages past the depth
     limit and the pages whose types nest past `MAX_NESTING`. (Amended in
     Task F1.)
   - It's for running against a restored backup or staging copy before
     deploying.
3. **Sort keys.**
   - **Migration tests.** On a seeded database with every bad case plus
     healthy groups:
     - healthy groups are byte-for-byte unchanged;
     - every group keeps its order;
     - keys become valid and unique;
     - a second run changes nothing.
   - **Generator tests.** For every valid pair the generator returns a key
     strictly between; it never emits an invalid key, and it never panics.
4. **Tasks.**
   - **End-to-end differential:** Y.Doc → legacy (serialise +
     `extract_tasks`) against Y.Doc → the new Y-walk extraction, over the
     whole corpus.
   - **Pages where today's listing agrees with the room** (same count, same
     node per index): texts, indexes and states are identical.
   - **All other pages:** these are the only ones allowed to change, and the
     lockstep property must hold for them.
5. **Moves.** Same-depth reorders and "into" are gesture-identical to main:
   each such gesture that was reachable before lands the page in the **same
   final position** as before. The unit table compares resulting orders, not
   request bodies.
   - "Into" still appends as the last child.
   - Intentional changes:
     - the gap directly under an expanded parent now means "first child". It
       wasn't reachable before;
     - **the general rule:** a before/after drop next to a row at another
       depth keeps the dragged page's depth unless the pointer moves
       sideways (one INDENT per level, clamped to the depths the gap
       allows), and the drop line shows that depth before release. Main
       took the hovered row's depth. Worked examples, pinned by
       `tree.test.ts` ("projectDrop with no sideways move keeps the page's
       depth next to rows at other depths"), both dragged straight up or
       down:
       - in `A ─ A1 ; B ; C`, A1 dropped on C's top gap nests under B
         (`{parent_id: "B"}`). Main put it at the top level, before C;
       - in `A ─ A1 ─ A1a ; B ; X`, X dropped on A1a's bottom gap goes to
         the top level, after A (`{parent_id: null, after_id: "A"}`). Main
         made it a sibling of A1a, after it.

       Task 15's un-nest case is one instance of the rule: a child dropped
       on the bottom gap of the next top-level page, without moving left,
       nests under that page, where main put it at the top level after that
       page (reached only by luck of mouse speed, C.1). Moving one level
       left puts it there now. (Ratified in Task 15; amended in Task F5,
       after the final review found the rule general and not one case: this
       first listed only that case and the gap under an expanded parent.)
6. **Existing suites stay green unchanged:** all Rust, web and e2e tests. Any
   change to an existing test must be justified in the PR as a bug the old
   test encoded, or as a mechanical adaptation (the plan's P6 lists them).
   Tests of code this PR removes are removed with it: main's
   `describe("dropIntent")` in `tree.test.ts` went with `dropIntent`, which
   had no caller left, and `dropZone`'s tests probe the same geometry.
   (Amended in Task 21a.)

## Testing (new coverage)

Each item fails on main (RED evidence in the PR) and passes after.

- **knot-markdown:**
  - fixtures `list_breaks.md`, `nested_tasklists.md`, `list_blocks.md`;
  - `round_trip_all_fixtures` reads the directory instead of a hard-coded
    list;
  - structural idempotence `parse(serialise(parse(md))) == parse(md)`;
  - a schema-validity checker run after every parse;
  - damaged-shape documents serialise to healed Markdown;
  - `extract_tasks` on nested checklists;
  - unknown-node degradation;
  - the differential harness.
- **knot-storage:**
  - lexorank exhaustive pairs with `a` up to 2 and `b` up to 3 characters
    (length ≤ 2 alone passes on main), the prefix family, invalid inputs
    returning `Err`, a 2000-seed × 400-op simulation, 2000 top inserts, the
    "insert after first ×8" regression, and `spread`;
  - `create_placed` / `move_placed` over seeded bad keys;
  - 20 concurrent `First` creates (under a parent and at top level) are
    distinct;
  - rebalance;
  - the heal migration's invariants.
- **knot-server integration:**
  - 10× "New page" at one level: under a parent, every request returns 201;
    at the top level, the keys are distinct and in creation order (both fail
    on main);
  - the one-time refresh re-renders stale cache rows and task indexes, and
    leaves fresh ones alone;
  - `/tasks` toggles match the listing for checklists in table cells and in
    ordered lists;
  - 10× insert after the first page;
  - GET markdown and history restore of a damaged page and of a nested
    checklist;
  - from-template of such a page;
  - `/tasks` toggle on nested checklists;
  - importing the same export twice at top level;
  - move with a non-sibling anchor returns 422;
  - the Duplicate suite: lossless content (column widths, spans, multi-block
    cells, attachments, a list line break); placement right after the source;
    a new board id with the same state; images survive archiving or purging
    the original; subpage copying with internal links remapped; unreadable
    blobs not copied; archived returns 409; viewer returns 403; no partial
    rows on failure; byline.
- **Web (Vitest):**
  - `projectDrop` tables: un-nest the last child of the last root in place,
    nest by dragging right, the clamp under an expanded parent, collapsed
    parents, dragged subtree excluded, a non-last child can't climb, no-ops;
  - `keyboardMove` edge cases;
  - announcements;
  - context-menu items by role;
  - the Duplicate dialog (shown only with subpages, unticked default, one
    POST, navigation).
- **e2e (Playwright):**
  - the nest test made deterministic: pages seeded via API; realistic
    multi-step moves; it waits for `data-drop-zone` / `data-drop-depth` before
    release; `expect.poll` on `parent_id`;
  - un-nest under the last root by dragging left;
  - nest by dragging right;
  - drop into a collapsed parent;
  - keyboard and menu indent/outdent;
  - releasing on your own row doesn't reload the page;
  - drop outside cancels;
  - Duplicate with and without subpages: the copy's image and board still
    render after archiving the original.
  - `tree-dnd.spec.ts` (it only asserted "no toast") is replaced.

## Constraints

- **Order of operations.** The migration and constraint change run under the
  Helm migrate hook. The migration must succeed on any existing data:
  duplicates are healed *before* the constraint swap.
- **No new runtime services.** New dependencies only if they pass
  `deny.toml`. None are expected; no proptest is needed (use deterministic
  loops).
- **Repo checks:** `cargo fmt`, `clippy -D warnings`, `nextest --workspace`,
  `cargo deny`, web `tsc`, `lint --max-warnings 0`, `test`, and e2e.
- **Docs:**
  - CHANGELOG (Upgrading: one migration that repairs page order where
    needed; Added: Duplicate, Move/Indent menu items; Fixed: each area);
  - the knot-api skill (duplicate route, move 422);
  - the follow-up issue for Markdown escaping.

## Left to the builder

- **Markdown:**
  - the exact "can follow a paragraph directly" rule set for the container
    writer, within the round-trip and compatibility requirements;
  - the generator design for the differential corpus.
- **Sort keys:** the advisory-lock key derivation, and the rebalance key
  width.
- **Web:** the component split in `DocTree.tsx`.
- **Duplicate:** S3 copy implementation details.

## Prototypes and probes

Historical: these were in a temporary session scratchpad, `pr42-probes/`,
not part of the repo. The probe inputs are in the repo as
`crates/knot-markdown/tests/probes/*.md` (the plan's P21), and the paths
below are kept only as a record of what was prototyped. (Amended in
Task F5.)

The probe cases went into the repo as probes, not fixtures: every fixture
must round-trip byte for byte, which a probe input need not (the plan's
P21). (Amended in Task F6; this first said to copy them in as fixtures.)
What was in `pr42-probes/`:
- **Markdown:** `knot-markdown-fixed/` (importer adapter + serializer, 23/23
  existing tests pass, the 40-input matrix round-trips) and `out-*.txt`.
- **Sort keys:** `lexo/newimpl.rs`, the exhaustive/random harnesses, and
  `pg_probe.sql`.
- **Drag and drop:** `dnd/projection-proto.mts`.
- **Duplicate:** `dup-probe/` (CRDT state copy with id remap, lossless).
