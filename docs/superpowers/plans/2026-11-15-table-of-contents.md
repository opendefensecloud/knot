# Table of Contents Block Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `table_of_contents` block with a per-box depth (`maxLevel`). It lists the page's headings live as a styled box, jumps to them, and survives Markdown, the history viewer, the public page and DOCX/ODT.

**Architecture:** A new atom node in `tools/schema.json` stores only `maxLevel`. The editor renders the box from the current headings in a React node view and gives headings GitHub-style ids with a decoration plugin (not stored). Rust writes and reads `[[_TOC_]]` (plus `<!-- knot:toc maxLevel=N -->` when N ≠ 3), the public renderer expands it, and knot-export writes a static list. One slug rule exists in TS and Rust, held in step by a shared fixture.

**Tech Stack:** Rust (knot-markdown, knot-export, knot-history, knot-server, pulldown-cmark, ammonia), React 18 + Tiptap 3 + Yjs, vitest, cargo nextest, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-15-table-of-contents-design.md`

## Global Constraints

- Node kind `table_of_contents`, attr `maxLevel`: int, min 1, max 6, default 3. Nothing else is stored.
- Markdown, top level: `[[_TOC_]]` on its own line for maxLevel 3; otherwise `<!-- knot:toc maxLevel=N -->` on the line before. In a table cell everything sits on one line: `<!-- knot:toc maxLevel=N -->[[_TOC_]]`, or `[[_TOC_]]` alone for 3. Import: a missing or invalid comment means 3.
- Slug rule (GitHub/dendronotes): NFKD, lower case, keep `\p{L}\p{N}`, spaces and `-`, trim, whitespace runs become `-`; an empty result becomes `section`; repeats become `x`, `x-1`, `x-2` in document order. If code is copied from dendronotes (`web/src/lib/outline.ts`, Apache-2.0), add a line to `NOTICE`.
- Headings listed: every `heading` node in document order (including in tables and quotes) with level ≤ maxLevel and non-empty text. Indent relative to the shallowest shown level, capped at 4 steps. Empty state: `No headings yet`. Box title: `Contents`. Depth control label: `Levels`.
- Box CSS class family `knot-box`, `knot-box__title`, built only from existing tokens (`--color-muted`, `--color-border`, `--color-fg`, `--color-fg-muted`, `--color-accent`).
- Toolbar button `data-testid="toolbar-toc"`, label `Table of contents`, icon `ListTree`. No typed input rule.
- Public page: ammonia allows only `id` on `h1`–`h6` and `class="knot-box"` on one wrapper `div`. Fixed inline box style there (light grey background, thin border).
- DOCX/ODT: a `Contents` caption, then one paragraph per heading, indented by level, with no links.
- Rollback: if an older client or server drops the node, accept it and say so in the `CHANGELOG.md` upgrade note. No flag or gate.
- Test first (Marco's rule): each task's failing tests are run, shown to Marco and talked through before implementing (**⏸ Marco**).
- Local quirks: the server build needs `RUSTFLAGS="-D warnings -A deprecated"` (Homebrew rustc 1.99). For e2e, start knot-server and `pnpm dev` yourself (toxiproxy is unreachable on Docker Desktop mac). See memory `knot-shortlink-work-state`.

## Review Focus

1. **Duplicate and odd headings** ("Tasks" twice, "Café", "C++ & Rust", emoji-only, empty): ids must be unique and match between the editor and the public page. → Task 1, shared fixture rows for each.
2. **A TOC inside a table cell** must survive Markdown export and import, and expand on the public page. → Task 3 test `toc_in_table_cell_round_trips`, Task 5 test `expands_toc_in_a_table_cell`.
3. **Heading edits by a collaborator** must update the box without the box writing to the document. → Task 6 test `updates live and never writes to the doc`.
4. **Clicking an entry** must land the heading below the sticky dock bar, and must also work in read mode and the history viewer. → Task 6 test `scrolls the heading below the sticky band`; Task 7 e2e.
5. **An invalid or hostile comment** (`maxLevel=99`, `maxLevel=abc`, a `<script>` near the marker) must give depth 3, and the public page must stay sanitised. → Task 3 test `invalid_toc_comment_means_3`, Task 5 test `public_toc_keeps_html_sanitised`.

---

### Task 1: Slug rule in TS and Rust with one fixture

**Files:**
- Create: `crates/knot-markdown/tests/fixtures/heading_slugs.json` (`[{"titles":[...],"slugs":[...]}]`)
- Create: `crates/knot-markdown/src/slug.rs` (+ `pub mod slug;` in `lib.rs`)
- Create: `crates/knot-markdown/tests/slug.rs`
- Create: `web/src/features/editor/headingSlugs.ts`, `headingSlugs.test.ts`

**Interfaces:**
- Produces (Rust): `pub struct Slugger` with `Slugger::default()` and `fn slug(&mut self, title: &str) -> String`.
- Produces (TS): `export function slugger(): (title: string) => string`.

- [ ] **Step 1: Write the fixture and both tests.** Fixture rows: `["Plan","Tasks","Tasks","Café"]→["plan","tasks","tasks-1","cafe"]`; `["C++ & Rust"]→["c-rust"]`; `["  Hallo   Welt  "]→["hallo-welt"]`; `["🚀"]→["section"]`; `["Über uns","über uns"]→["uber-uns","uber-uns-1"]`; `["section","🚀"]→["section","section-1"]`. Rust test `slugs_match_shared_fixture` and TS test `slugs match the shared fixture` both read the JSON (TS: `readFileSync` from `../../../../crates/knot-markdown/tests/fixtures/heading_slugs.json`).
- [ ] **Step 2: Run and confirm they fail** — `cargo nextest run -p knot-markdown --test slug` and `cd web && pnpm vitest run src/features/editor/headingSlugs.test.ts` → FAIL (missing module).
- [ ] **Step 3: ⏸ Marco.**
- [ ] **Step 4: Implement both.** Rust: NFKD via the `unicode-normalization` crate (check it is in `Cargo.lock` first; if it is not, ledger a ruling and add it to the workspace). Strip combining marks (`\p{M}`) after NFKD so `é`→`e`.
- [ ] **Step 5: Run both** → PASS.
- [ ] **Step 6: Commit** `feat: GitHub-style heading slugs in TS and Rust`.

---

### Task 2: Schema, codegen, history and the bare web node

**Files:**
- Modify: `tools/schema.json` (after `excalidraw_board`): `{"kind":"table_of_contents","group":"block","content":"","isBlock":true,"attrs":[{"name":"maxLevel","type":"int","min":1,"max":6,"default":3}],"markdown":{"kind":"table_of_contents"}}`
- Regenerate: `crates/knot-markdown/src/schema.rs`, `web/src/features/editor/schema.ts` (`make schema.gen`), goldens (`UPDATE=1 cargo nextest run -p schemagen`)
- Modify: `crates/knot-history/src/pm_json.rs` `KNOWN_NODES` (schema order); regenerate the pm_json fixtures with the three `KNOT_UPDATE_GOLDEN=1` steps (`tests/pm_json.rs:15-17`) after adding a `["table-of-contents", …]` case to `web/src/features/docs/history/pmJsonGolden.test.ts`
- Create: `web/src/features/editor/nodes/TableOfContents.ts`: `Node.create` with `name: "table_of_contents"`, `group: "block"`, `atom: true`, `selectable: true`, `draggable: true`; attr `maxLevel` (default 3, `parseHTML` from `data-max-level`, clamped 1–6); `parseHTML: div[data-knot-toc]`; `renderHTML` → `div[data-knot-toc][data-max-level]`. No view yet.
- Modify: `extensions.ts` (register after `ExcalidrawBoard`, same in viewer), `schema.test.ts` (`REGISTERED_EXTENSIONS`, `CREATE_EXTENSIONS_ORDER`, the representative tree :176)

**Interfaces:**
- Produces: node `table_of_contents`, export `TableOfContents` from `nodes/TableOfContents.ts`, and the Rust schema variant `TableOfContents`.

- [ ] **Step 1: Failing tests.** In `schema.test.ts`, add `table_of_contents` to the representative tree with `maxLevel: 2` and the expected lists. In pm_json add the case `table-of-contents` (not rendered as "Unsupported content").
- [ ] **Step 2: Run** `cd web && pnpm vitest run src/features/editor/schema.test.ts` and `cargo nextest run -p knot-history --test pm_json` → FAIL.
- [ ] **Step 3: ⏸ Marco.**
- [ ] **Step 4: Implement** as listed under Files.
- [ ] **Step 5: Run** both, plus `cargo nextest run -p schemagen` → PASS.
- [ ] **Step 6: Commit** `feat: table_of_contents node in the schema`.

---

### Task 3: Markdown out and in

**Files:**
- Modify: `crates/knot-markdown/src/to_markdown.rs`: an arm in `write_block` (:168); add the kind to `BLOCKS` (:376) and to the `open` check (:1012) like `horizontal_rule`; the cell path through `write_cell_block` (:528) must give the one-line form.
- Modify: `crates/knot-markdown/src/from_markdown.rs` (contains NUL bytes; use `grep -a`): remember a `<!-- knot:toc maxLevel=N -->` from `Event::Html` (:556) or `Event::InlineHtml` (:542) as `pending_toc_level`. At `TagEnd::Paragraph` (:297), a paragraph whose only text is `[[_TOC_]]` becomes the node (`push_block`, like the board at :297-329), and the same in cells (`end_cell_paragraph` :800). Clear the pending level on any other block.
- Modify: `legacy/*` only if the `legacy-compat` tests demand it (ledger a ruling either way).
- Modify: `tests/common/corpus.rs` (`toc(level)` builder in `leaves()`), `tests/compat.rs` lists (:389, :1684), `tests/round_trip.rs`, new fixture `tests/fixtures/table_of_contents.md`.

**Interfaces:**
- Consumes: the schema variant from Task 2.
- Produces: the Markdown forms above (Task 5 renders them).

- [ ] **Step 1: Failing tests** in `round_trip.rs`:
  - `toc_default_is_bare_marker`: maxLevel 3 → `"[[_TOC_]]\n"`, and back.
  - `toc_depth_writes_comment`: maxLevel 2 → `"<!-- knot:toc maxLevel=2 -->\n[[_TOC_]]\n"`, and back to 2.
  - `toc_in_table_cell_round_trips`: a cell `<!-- knot:toc maxLevel=1 -->[[_TOC_]]` gives the node with 1, and back.
  - `invalid_toc_comment_means_3`: `maxLevel=99` and `maxLevel=abc` give 3.
  - `marker_inside_text_stays_text`: `see [[_TOC_]] here` stays a paragraph.
  - The comment alone, with no marker after it, is dropped as today.
- [ ] **Step 2: Run** `cargo nextest run -p knot-markdown --all-features` → the new tests FAIL.
- [ ] **Step 3: ⏸ Marco.**
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** the crate suite with `--all-features` → PASS.
- [ ] **Step 6: Commit** `feat: table of contents in Markdown as [[_TOC_]]`.

---

### Task 4: DOCX and ODT

**Files:**
- Modify: `crates/knot-export/src/model.rs` (`Block::Toc { entries: Vec<TocEntry> }`, `pub struct TocEntry { pub level: u8, pub text: String }`, level relative to the shallowest shown, capped at 4).
- Modify: `crates/knot-export/src/walk.rs`. The walker needs the headings, so collect all headings first (in `walk`, :63), then build `Block::Toc` in `walk_single` (:396) for `table_of_contents`.
- Modify: `docx/body.rs` `fn block` (:59) and `odt/body.rs` `fn block` (:84): a `Contents` caption paragraph, then one paragraph per entry with left indent `level × 0.75 cm` (DOCX: 425 twips per level).
- Tests: `tests/walk.rs`, `tests/docx.rs`, `tests/odt.rs`; `examples/kitchen_sink.rs` gets one TOC.

**Interfaces:**
- Consumes: Task 2 node, Task 1 nothing (no links in the export).

- [ ] **Step 1: Failing tests:**
  - `toc_lists_headings_up_to_max_level` (walk): headings H1 "A", H2 "B", H3 "C" with maxLevel 2 → entries `[(0,"A"),(1,"B")]`.
  - `toc_writes_contents_caption_and_indented_entries` (docx and odt): checks the caption text and the indent attribute on the second entry.
- [ ] **Step 2: Run** `cargo nextest run -p knot-export` → FAIL.
- [ ] **Step 3: ⏸ Marco.**
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat: table of contents in DOCX and ODT export`.

---

### Task 5: Public share page

**Files:**
- Modify: `crates/knot-server/src/routes/public.rs` `render_markdown` (:121-205):
  - Enable nothing new in pulldown.
  - Collect headings in a first pass with `knot_markdown::slug::Slugger`, and emit `<hN id="slug">` for every heading.
  - Replace a paragraph that holds only `[[_TOC_]]` (top level or cell; honour a preceding `knot:toc` comment) with `<div class="knot-box"><p class="knot-box__title">Contents</p><ul>…<li><a href="#slug">text</a></li>…</ul></div>`, nested `ul` by level.
  - Extend the ammonia builder: `id` on `h1`–`h6`, `class` on `div` and `p` limited to the values `knot-box` / `knot-box__title`; keep everything else at its default.
  - Add `.knot-box{background:#f4f5f7;border:1px solid #d9dde3;border-radius:6px;padding:12px 16px}` and `.knot-box__title` to the inline `<style>` (:193).
- Tests in the same file (`mod tests`, from :388).

**Interfaces:**
- Consumes: `Slugger` (Task 1), the Markdown forms (Task 3).

- [ ] **Step 1: Failing tests:**
  - `headings_get_slug_ids`: `# Plan\n## Tasks\n## Tasks` → `id="plan"`, `id="tasks"`, `id="tasks-1"`.
  - `expands_toc_marker_into_box`: `[[_TOC_]]` with three headings → `class="knot-box"`, three `href="#…"`.
  - `expands_toc_in_a_table_cell`.
  - `toc_respects_max_level_comment`.
  - `public_toc_keeps_html_sanitised`: a `<script>`, an `onclick` and `class="evil"` next to the marker are stripped; `id` is kept only on headings.
- [ ] **Step 2: Run** `RUSTFLAGS="-D warnings -A deprecated" cargo nextest run -p knot-server --lib routes::public` → FAIL.
- [ ] **Step 3: ⏸ Marco.**
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** → PASS, then the `sicherheits-review` check of the ammonia change (ledger the result).
- [ ] **Step 6: Commit** `feat: table of contents and heading anchors on public pages`.

---

### Task 6: Editor box, heading ids, toolbar, paste

**Files:**
- Create: `web/src/features/editor/headingAnchors.ts`: a plugin `HeadingAnchors` (Tiptap `Extension`, name `knotHeadingAnchors`) that decorates each heading with `id` = slug (via `slugger()`), and `export function tocEntries(doc: PMNode, maxLevel: number): { level: number; text: string; slug: string }[]` (level already relative and capped at 4).
- Create: `web/src/features/editor/nodes/TableOfContentsView.tsx`: the node view. It reads the doc with `editor.state.doc` and re-renders on `transaction`. It shows the box (`knot-box`), entries as `<a href="#slug">`, the `Levels` `<select>` (1–6) only when `editor.isEditable`, and `No headings yet`. A click calls `scrollToHeading(slug)` and sets `location.hash` with `history.replaceState`.
- Create: `web/src/features/editor/scrollToHeading.ts`: `export function scrollToHeading(root: HTMLElement, slug: string): boolean`. It uses `stickyCoverBottom` (`features/docs/readingAnchor.ts:23`) to place the heading top below the band plus 8px.
- Modify: `nodes/TableOfContents.ts` (`addNodeView` → `ReactNodeViewRenderer(TableOfContentsView)`), `extensions.ts` (register `HeadingAnchors` in both sets; update `schema.test.ts` lists), `EditorToolbar.tsx` (button next to `toolbar-excalidraw`, inserting `{type:"table_of_contents",attrs:{maxLevel:3}}`), `styles/prose.css` (`.knot-box` family), `markdownPaste.ts` (before `marked`, turn the top-level marker forms into `<div data-knot-toc data-max-level="N"></div>`; allow `data-knot-toc` and `data-max-level` in `lib/sanitize.ts`).
- Modify: `features/docs/DocPage.tsx`. On load, if `location.hash` is set, call `scrollToHeading` once after the first sync.

**Interfaces:**
- Consumes: `slugger` (Task 1), the node (Task 2), `stickyCoverBottom`.
- Produces: `tocEntries`, `scrollToHeading`, `HeadingAnchors`.

- [ ] **Step 1: Failing tests:**
  - `headingAnchors.test.ts`:
    - `gives headings slug ids in the DOM`: two "Tasks" → `#tasks`, `#tasks-1`.
    - `tocEntries respects maxLevel and indents relative, capped at 4`: H2,H3,H6 with max 6 → levels 0,1,4.
    - `skips empty headings`.
  - `TableOfContents.test.tsx` (mounted editor):
    - `lists headings in a box with links`.
    - `updates live and never writes to the doc`: rename a heading through a Yjs update from a second doc; the box text changes, and an `update` listener on the doc sees no update originating from the box.
    - `changes depth via Levels`: select 1 → node attr `maxLevel` 1.
    - `hides Levels when not editable`.
    - `shows No headings yet`.
  - `scrollToHeading.test.ts`: `scrolls the heading below the sticky band` (stub `getBoundingClientRect` and `stickyCoverBottom` → 60; expect `scrollBy` top = heading top − 68).
  - `markdownPaste.test.ts`: `pastes [[_TOC_]] as a table of contents` and `keeps the depth comment`.
  - `EditorToolbar` test: `toolbar-toc inserts a table of contents`.
- [ ] **Step 2: Run** `cd web && pnpm vitest run src/features/editor` → the new tests FAIL.
- [ ] **Step 3: ⏸ Marco.**
- [ ] **Step 4: Implement.** Check the box in all 13 skins by eye in `make dev` (Settings → Appearance), including high contrast, and screenshot two of them for the PR.
- [ ] **Step 5: Run** `cd web && pnpm tsc --noEmit -p . && pnpm lint && pnpm vitest run` → all green.
- [ ] **Step 6: Commit** `feat: table of contents box in the editor`.

---

### Task 7: End-to-end, rollback check, changelog

**Files:**
- Create: `e2e/flows/table-of-contents.spec.ts`
- Modify: `CHANGELOG.md` `## [Unreleased]` (Added, plus an Upgrading note)

- [ ] **Step 1: Write the spec:**
  1. New page; type H1 "Plan", H2 "Tasks", H2 "Tasks", H3 "Details".
  2. `toolbar-toc`. The box lists four entries. Set `Levels` to 2 → three entries.
  3. Click the second "Tasks" → URL hash `#tasks-1`, and the heading's top is below the dock band.
  4. Export Markdown via the API → contains `<!-- knot:toc maxLevel=2 -->\n[[_TOC_]]`.
  5. Share publicly and open `/p/<token>` → `.knot-box a[href="#tasks-1"]` exists and `#tasks-1` is an `h2`.
- [ ] **Step 2: Run** against self-started servers → PASS. Also run `editor-markdown-roundtrip.spec.ts`, `export.spec.ts`, `history.spec.ts` → PASS.
- [ ] **Step 3: Rollback check.** Build `main` (0.10.0) web and server against the same database after the page has a TOC. Record what happens to the box (kept / placeholder / dropped). Ledger it, and write the `CHANGELOG.md` upgrade note to match.
- [ ] **Step 4: Changelog** Added entry, ending with `(#TBD)`.
- [ ] **Step 5: Full suites** — `set -o pipefail; RUSTFLAGS="-D warnings -A deprecated" make test 2>&1 | tail -20` and `make lint` (clippy with the local allowances from memory) → exit 0.
- [ ] **Step 6: Commit** `test: e2e for the table of contents`.

---

### Task 8: Review and PR

- [ ] **Step 1:** Run the `code-reviewer` agent on the whole branch. Fix confirmed findings test-first.
- [ ] **Step 2:** Run `sicherheits-review` (focus: the ammonia allow-list and the ids on the public page, the paste sanitiser change, no stored HTML).
- [ ] **Step 3:** Push to the `fork` remote and run `gh pr create -R opendefensecloud/knot --head marcobockelbrink:feat/table-of-contents`, with tests, review results and the two skin screenshots. Do not merge.
