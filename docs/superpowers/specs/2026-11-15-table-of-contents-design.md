# Table of contents block — design

Date: 2026-11-15 · Status: draft for Marco's approval (builder check done)

## Intent

Long pages need an overview you can click through. Marco wants it as a
**box inside the page**, not a side panel. It should be a block that can be
placed anywhere, including inside a table cell to sit next to other
content, and it should look like a box ("Kasten"). How deep the outline goes
(H1 only, H1–H2, …) is set **per box**.

The box style is the first of two "box" blocks. The callout boxes that
follow (Info/Warning/Tip) are meant to share it, so it is designed once for
all 13 skins.

## Goals

- A block node `table_of_contents` with one attribute, `maxLevel`
  (1–6, default 3). It stores nothing else. The list is built live from the
  page's headings, so it is never stale and never writes to the document
  when headings change.
- Shown everywhere the page is shown: the editor (edit and read mode), the
  history viewer, public share links (`/p/<token>`), Markdown export/import,
  and DOCX/ODT export.
- Clicking an entry scrolls to the heading. The heading must land below the
  sticky dock bar, not behind it.
- Headings get anchor ids that follow GitHub's rule: lower case, letters,
  digits, spaces and `-` kept, spaces become `-`, duplicates numbered
  `x`, `x-1`, `x-2`, and an empty result becomes `section`. This is the
  same rule dendronotes uses (`web/src/lib/outline.ts`, Apache-2.0, credit
  it in `NOTICE` if code is taken over). Ids are computed when rendering;
  they are **not stored**.
- Insert from the editor toolbar (icon `ListTree`). Pasting or importing
  Markdown with `[[_TOC_]]` also creates the box. There is no typed input
  rule: `[[` already opens the page picker (PR #110), and the two would
  compete for the same keys.
- In edit mode the box shows a small control to set the depth
  (`Levels: 1 … 6`). Readers see no control.

## Non-goals

- Links to a heading on another page (`[[Page#Heading]]`). This is a good
  follow-up to the `[[` picker (PR #110), noted for later.
- A side-panel outline.
- Numbering entries (1, 2.1, …). Authors often number their own headings.
- Word's native TOC field. DOCX/ODT get a static list.
- The callout boxes themselves. They come next and reuse the box style.

## Anti-goals

- A box whose content drifts from the headings, or one that writes to the
  shared document by itself. Approach C below was dropped for this reason.
- A box that looks right only in the default skin.

## Constraints

- Schema source of truth is `tools/schema.json`. `make schema.gen`
  regenerates `crates/knot-markdown/src/schema.rs` and
  `web/src/features/editor/schema.ts`, and their golden files under
  `tools/schemagen/tests/fixtures/`.
- Every place that walks nodes must know the new kind. The
  `excalidraw_board` node is the model and touches about 30 files: the
  knot-markdown to/from/remap/legacy code and its corpora, knot-export
  `walk.rs`, knot-history `pm_json.rs` and its fixtures,
  `HistoryDiffExtension`, `schema.test.ts`, and the e2e Markdown round-trip
  spec.
- Public pages are rendered on the server from Markdown with
  `pulldown_cmark` (`crates/knot-server/src/routes/public.rs:121`). That
  renderer must expand the TOC marker into a list and give headings ids
  using **the same slug rule** as the browser. The rule therefore exists
  twice, in Rust and in TS, and a shared test table keeps both in step.
- The colours come from the existing skin tokens (`--color-surface`,
  `--color-muted`, `--color-border`, `--color-fg`, `--color-fg-muted`,
  `--color-accent` in `web/src/styles/tokens.css`), so all 13 skins work
  without per-skin values. The high-contrast skin is checked by eye.
- Rolling back: a 0.10.x server or client that does not know the node must
  not destroy it. The builder checks how unknown nodes behave today (as
  with earlier schema additions) by test. If an old client or server drops
  the box, that is accepted: the `CHANGELOG.md` upgrade note says that
  rolling back below this release removes table-of-contents boxes. No
  feature flag or version gate.
- This branch starts from `main` and does not depend on PR #110.

## Approaches

1. **New block node `table_of_contents { maxLevel }` (chosen).** Clean in
   every view. In GitLab the Markdown form renders as a real TOC. It is the
   same path the callout boxes will need. Cost: a full schema change
   through Rust and TS.
2. Code block with language `toc` (as Mermaid does), content `depth: 2`.
   No schema change, but GitHub, GitLab and the Word export show a code
   block, and search indexes "depth: 2".
3. Store the TOC as a real linked list that a plugin rewrites. Works
   everywhere without special handling, but every heading edit writes to
   the shared document. With several editors that means conflicts and a
   noisy history. Dropped.

Marco chose 1, with the GitHub-style anchors taken from dendronotes.

## Design

### Markdown

- `maxLevel` 3 (the default) is written as a line with only `[[_TOC_]]`.
  GitLab renders that line as its own table of contents.
- Any other depth gets a comment line before it, which GitLab and GitHub
  hide:
  ```
  <!-- knot:toc maxLevel=2 -->
  [[_TOC_]]
  ```
- Inside a table cell everything must fit on one line of a GFM pipe
  table, so the cell form is `<!-- knot:toc maxLevel=2 -->[[_TOC_]]` (or
  `[[_TOC_]]` alone for depth 3). GitLab shows a cell TOC as plain text;
  knot's own import and public page understand it.
- Import reads all these forms. A `[[_TOC_]]` without the comment means
  `maxLevel` 3, and a comment with an invalid value means 3.

### Which headings

- All `heading` nodes of the page, in document order, whose level is no
  greater than `maxLevel`. That includes headings inside tables and
  blockquotes.
- Indentation is relative to the shallowest heading shown, and stops
  growing after 4 steps (as in dendronotes) so a narrow box in a table cell
  does not run off to the right.
- Empty headings are skipped. With no headings the box says
  "No headings yet".

### Box look (shared with the callouts later)

- Background `--color-muted`, a 1px `--color-border` frame, rounded like
  the editor's other blocks, and a small title "Contents" in
  `--color-fg-muted`.
- Entries are links in `--color-fg`, with `--color-accent` on hover and
  focus.
- One CSS class family (`knot-box`, `knot-box__title`) so the callouts can
  reuse it.
- The box fits its container, so in a table cell it is as wide as the cell.

### Behaviour

- Edit mode: the node is an atom block (selectable, deletable, draggable
  like the board). The depth control changes `maxLevel` through a normal
  transaction, so it goes through Yjs and history like any edit.
- Read mode and the history viewer show the same list without the control.
- A click scrolls to `#<slug>` and leaves the heading below the dock bar.
  It updates the URL hash so the link can be copied. Opening a page with a
  hash scrolls there once the content is in.
- Public page: the server replaces the marker (also inside a cell) with a
  list of links to `#<slug>` inside a box, and adds the same ids to its
  `<h1>`–`<h6>`. The output goes through `ammonia`, whose defaults strip
  `id`, `class` and `<nav>`. Allow exactly: `id` on `h1`–`h6`, `class` on
  one wrapper element with the fixed value `knot-box`, and keep links
  as they are. Ids only ever hold slug characters. The page is shown in an
  iframe with `sandbox="allow-same-origin"` and no scripts
  (`web/src/features/public/PublicDoc.tsx:80`), so DOM clobbering through
  ids has nothing to attack; the security review still checks it. The
  public page has its own inline CSS, so it gets a fixed, plain box style
  there (light grey background, thin border), not the skin tokens.
- DOCX/ODT: a "Contents" caption followed by one paragraph per heading,
  indented by level. No links.

## Left to the builder

- Wiring the new kind through each Rust walker and the corpora, following
  `excalidraw_board`.
- Where the scroll-to-heading helper lives, reusing the dock offset that
  `readingCaret` / `stickyCoverBottom` already know.
- The exact control (a `<select>` is fine) and its accessible name.
- How the full-text index treats the node. It should index nothing for it.
- Tests: a shared slug table (TS and Rust read the same cases), the
  Markdown round trip for both forms, live update on a heading edit,
  depth change, the box inside a table cell, the public HTML, the
  DOCX/ODT output, and one Playwright flow.
