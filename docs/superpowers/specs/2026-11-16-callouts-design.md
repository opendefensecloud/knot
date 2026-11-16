# Callout boxes — design

Date: 2026-11-16. Decided with Marco in conversation; items marked
*(builder's call)* or *(my call)* were not decided by him.

## Intent

Authors want to make a passage stand out as a note, tip, important point,
warning or caution — the boxes people know from GitHub and GitLab — and
have them survive a Markdown round trip with both. Marco's bar: they must
look "schick" in every one of knot's 13 skins (`web/src/styles/tokens.css`).

## Goals

- Five kinds, exactly GitHub's and GitLab's: **note, tip, important,
  warning, caution** (Marco: option A, so imports from other repos are
  lossless).
- An optional **custom title** per box (Marco: option B). Empty title shows
  the kind's name (Note, Tip, Important, Warning, Caution).
- Look: **style C, "Kopfzeile"** — a tinted header band with the kind's
  icon and the title, a border in the kind's colour, the body on the page
  surface. Mockup: `.superpowers/brainstorm/9199-1794844175/content/callout-styles.html`
  (gitignored scratch; the CSS there is the reference look).
- Content: any blocks a quote can hold (paragraphs, lists, images, code,
  nested boxes).
- Same look on the public share page; a sensible look in DOCX/ODT.

## Non-goals

- No collapsible/foldable boxes (Obsidian `[!note]-`).
- No kinds beyond the five, no user-defined colours or icons.
- No formatting inside the title (plain text only; GitLab's title is plain
  text).

## Anti-goals

- **A box's content must never be lost** — not by an older knot tab, not by
  a rollback, not by a round trip. This is why the box is a quote with
  attributes (see Approaches).
- Must not look out of place in any skin, including high-contrast (WCAG
  contrast as in `tokens.css`: body text 7:1, muted 4.5:1) and terminal
  (monospace, square corners).

## Approaches considered

- **A — new block `alert`.** Clean separation, but a knot client that does
  not know the node deletes it *with its content*
  (`@tiptap/y-tiptap` `createNodeFromYElement` catch path,
  `dist/y-tiptap.js` ~969), for everyone. Rejected: anti-goal.
- **B — the existing `blockquote` gets attributes `kind` and `title`.**
  Chosen by Marco. An old client builds a plain blockquote (ProseMirror
  ignores attributes its schema does not define) and, if it edits inside
  the box, strips the unknown attributes (`updateYFragment`,
  `dist/y-tiptap.js` ~1552-1567): the box degrades to a plain quote, the
  text stays. Matches Markdown, where an alert *is* a blockquote with a
  marker. Cost: every blockquote path must tell a quote from a box.
- **C — like B, title as a child text node.** Would allow bold/links in the
  title, but GitLab drops them and it costs much more. Rejected.

## Data model

`tools/schema.json`, `blockquote` gains:

- `kind`: string, optional, one of `note|tip|important|warning|caution`;
  absent/null = plain quote. Any other value is treated as absent
  everywhere (rendering, export, Markdown).
- `title`: string, optional, plain text; absent/empty = no custom title.
  Trimmed; newlines replaced by spaces *(my call)*.

Regenerate `crates/knot-markdown/src/schema.rs`, `web/src/features/editor/schema.ts`
and schemagen goldens (`make schema.gen`, then rustfmt — as for the table of
contents). `crates/knot-history/src/pm_json.rs` must carry both attributes
(a fixture with a titled warning box).

No migration: existing quotes simply have no `kind`.

## Markdown

Export (`crates/knot-markdown/src/to_markdown.rs`; the frozen
`legacy/` serializer stays untouched):

- No title: `> [!WARNING]` on its own line, then the content quoted — the
  GitHub form (upper case), renders on GitHub and GitLab.
- With title: `> [!warning] Wartung am Freitag` — the GitLab form (GitLab
  is case-insensitive; GitHub shows this as a plain quote, accepted by
  Marco). *(my call: lower case for the titled form, as GitLab documents
  it.)*
- Title text is escaped so it cannot start Markdown on that line *(builder's
  call how)*. A plain quote whose first line is literally `[!NOTE]` is
  escaped so it stays a quote *(builder's call how)*.
- Boxes in table cells are allowed in the editor (Marco: option A). In
  Markdown a cell is one line of text, so there only the box's text is
  written (as for a plain quote in a cell today); a Markdown import of that
  table has no box. The document itself always keeps the box. This is the
  one accepted exception to "never lost by a round trip": the text is
  kept, the box is not.
- Nested boxes and boxes in list items export as nested/indented alert
  syntax; GitHub may show them as plain quotes, the text is kept.

Import (`from_markdown.rs`), all of these become a box:

- `> [!NOTE]` … any case, alone on its line → kind, no title.
- `> [!note] Some title` → kind + title (pulldown-cmark 0.13's
  `ENABLE_GFM` alert support does not accept trailing text, so this needs
  its own handling — a pre-pass like the TOC's `mark_toc_markers`, or
  reading the first paragraph of a blockquote — *builder's call*).
- An unknown marker (`> [!FOO]`) stays a plain quote with that text.
- A box with no content after the marker gets one empty paragraph
  (`block+`).
- A line `> ---` right after a titled marker is a horizontal rule in the
  box's body, not a setext underline of the title (Marco, 2026-11-16; how
  GitLab shows it is unverified).
- The title is plain text: Markdown in it (`> [!note] **wichtig**`) shows
  its characters as typed, and export escapes them (Marco, 2026-11-16).

Markdown paste in the editor (`markdownPaste.ts`, uses `marked`) must
produce the same boxes.

## Editor (web)

- `web/src/features/editor/extensions.ts`: extend the Blockquote extension
  with `kind`/`title` attributes (parse/render as `data-kind`,
  `data-title` on `<blockquote>`; `lib/sanitize.ts` allows them).
- A node view for boxes: header band (icon, title input, kind select) and
  `contentDOM` for the body. A plain quote (no kind) renders exactly as
  today.
  - Title: plain-text input in the header; placeholder = kind name, muted.
    Typing in it updates `title` through a normal transaction (undo works,
    collaborators see it).
  - Kind: a `<select>` at the right of the header, like the TOC's
    "Levels"; only when editable.
  - Icons: lucide `Info`, `Lightbulb`, `MessageSquareWarning`,
    `TriangleAlert`, `OctagonAlert` (as in the mockup).
- Toolbar: one new button (icon `MessageSquareWarning` or similar —
  *builder's call*) opening a menu of the five kinds. With the caret in a
  block, that block is wrapped in a box of that kind; inside a box, the
  menu changes the kind. Placement must keep the toolbar's button-count
  invariants (see the TOC ruling: `EditorToolbar.indent` test).
- Input rule *(my call)*: StarterKit's `> ` rule stays (plain quotes are
  typed as today). Typing `[!warning] ` (any of the five, any case) at the
  start of the *first paragraph of a quote* gives that quote the kind and
  removes the marker; so `> [!warning] ` typed in one go yields a box. Text
  typed afterwards goes into the body, not the title.
- Toolbar on an existing quote *(my call)*: picking a kind while the caret
  is in a plain quote converts that quote into a box (no nesting); inside a
  box it changes the kind. The Quote button is active inside a box too and,
  pressed there, lifts the content out as it does for quotes (the box goes,
  the text stays).
- Backspace at the start of an empty box (one empty paragraph) unwraps it,
  leaving an empty paragraph. Lifting out of a box otherwise works as for
  quotes.
- History viewer: kind and title changes show like other attribute changes;
  the box renders read-only there (no select, title not editable).

## Look

- Colours: per kind a hue with a light and a dark variant (GitHub's:
  note `#0969DA`/`#4493F8`, tip `#1A7F37`/`#3FB950`, important
  `#8250DF`/`#AB7DF8`, warning `#9A6700`/`#D29922`, caution
  `#CF222E`/`#F85149`), chosen by `[data-theme]`. Header background
  `color-mix(in oklab, hue 14%, surface)`, title text
  `color-mix(in oklab, hue 80%, fg)`, border
  `color-mix(in oklab, hue 45%, surface)`. high-contrast darkens the hue.
  Radius from `--radius-unit` (terminal: square). All in `prose.css` as
  tokens, no per-skin hand-tuning unless a skin fails the contrast check.
- Must be checked in all 13 skins by screenshot (as done for the TOC) and
  title/body text must meet the contrast targets of `tokens.css`.

## Public share page

`crates/knot-server/src/routes/public.rs`: render boxes with the same
structure and inline CSS (light variant; the page has no skins). ammonia
allows only the fixed box classes (and nothing user-controlled besides the
escaped title text). Enable whatever pulldown-cmark option or pre-pass the
import uses so the page and the import agree.

## DOCX / ODT export

`crates/knot-export`: a box becomes a bordered block: a bold title line
(custom title or kind name), then the content, border/shading in the
kind's colour *(builder's call on the exact OOXML/ODF construct — a
one-cell table or paragraph borders — whichever both formats support
reliably; must stay readable in grayscale print)*. A plain quote exports
as today.

## Compatibility and rollout

- Old clients: degrade to plain quote, text kept (see Approaches). No
  "must not roll back" warning needed; the CHANGELOG notes that rolling
  back turns boxes into plain quotes once someone edits them.
- Builds on the table-of-contents branch (`feat/callouts` from
  `feat/table-of-contents`, PR #111) *(my call)*: both touch
  `tools/schema.json`, `pm_json.rs` goldens, `public.rs` and `prose.css`;
  stacking avoids conflicts. The PR says it contains #111 until that is
  merged, then gets rebased onto `main`.

## Testing

Test-first per behaviour, shown to Marco failing before implementing.
At least: Markdown export/import of every kind with and without title,
case-insensitive import, unknown marker stays a quote, nested box, box in a
list item; a box in a table cell exports its text (no box after import); old-client degradation (a doc with `kind` opened
by a schema without it keeps the text); editor node view (title, kind,
toolbar, input rule, backspace unwrap); public page rendering and
sanitising; DOCX/ODT; e2e flow; screenshots in all 13 skins.

## Left to the builder

Exact file split, the import mechanism for titled markers, the toolbar
icon, the DOCX/ODT construct, CSS class names (prefix `knot-callout`
suggested), and test names.
