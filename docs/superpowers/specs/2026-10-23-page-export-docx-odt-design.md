# Export a page to Word (DOCX) and OpenDocument (ODT) — design

**Date:** 2026-10-23
**Status:** Draft (brainstorm), builder-checked, awaiting approval
**Mockup (approved in the brainstorm):**
[`2026-10-23-page-export-mockup.html`](2026-10-23-page-export-mockup.html).
It is the visual reference for the menu and the overall look, not a pixel spec.

## Intent

The company that uses knot works in Word and LibreOffice documents. People
want to take a knot page, export it, and send the file around internally
without reformatting it by hand. This feature covers that and nothing more:
one page in, one tidy document out, which opens cleanly in Word, LibreOffice
and Google Docs.

PDF was considered and dropped for now. A company look (letterhead, house
fonts, a Word template) is a planned follow-up. This export is built so a
template can slot in later without redoing it (§10).

## Goals

- Export the page you are looking at to **.docx** and **.odt** from the page
  header.
- Bring everything on the page along: text and formatting, headings, lists,
  checklists, tables (column widths, merged cells, lists and images inside
  cells), images, code, links, and **Mermaid diagrams and Excalidraw boards
  as pictures**. People use diagrams, so they are in scope.
- Use real document structure: Word heading styles with outline levels, so
  the navigation pane and a generated table of contents work. In ODT, the
  equivalent outline-level headings.
- Look neutral and tidy. Nothing in the file should look broken or
  half-formatted.
- Tell the user when something couldn't be included.

## Non-goals

- PDF output.
- Company templates or branding (follow-up, §10).
- Classification or sensitivity markings (e.g. "VS-NfD", Microsoft
  sensitivity labels). The user confirmed that marking a document is up to
  whoever sends it. This may change with company templates.
- Exporting several pages, or a page with its subpages, into one document.
  The Markdown zip keeps doing that.
- Importing DOCX/ODT back into knot, or round-tripping edits.
- Comments, history, tracked changes, syntax highlighting, embedded fonts.
- Exporting from public share links (`/p/:token`).
- Export UI in the mobile app. The API should not stand in its way (§3).
- Git sync. That is a separate project, brainstormed later.
- Fixing the existing Markdown export, including its single-page zip
  filename bug (`export_import.rs:428`), and the zip export refusing template
  pages.

## Anti-goals (failure even if it technically works)

- **One odd element sinks the whole export.** The Markdown serializer fails
  the entire page on an unknown node (`SerError::UnsupportedNode`,
  `crates/knot-markdown/src/to_markdown.rs`). This export must degrade per
  element and always produce a file.
- **Fake headings.** Big bold text that looks like a heading but isn't a
  heading style breaks navigation, TOCs and any later template.
- **Leaking content.** An image pasted from a page the exporter cannot read
  must not end up in their file.
- **Taking the server down.** An export of an image-heavy page must not run
  the pod out of memory and drop everyone's live editing sessions (§3,
  limits).
- **Silent omissions.** If a diagram or image couldn't be included, the user
  is told before they send the file on.
- **A sidecar service.** knot ships as one static binary. Requiring
  LibreOffice, Pandoc, Chromium or Gotenberg next to it is out.

## What the user decided

- DOCX and ODT now. PDF later, if at all.
- Neutral styling now. Company templates as a follow-up, with the export
  structured so templating fits in later.
- Diagrams (Mermaid, Excalidraw boards) must be included as pictures.
- No classification marking in the export: that is up to whoever sends the
  file. It may come back with templates.
- The menu and output in the mockup are right:
  - the download button becomes a menu with Word, OpenDocument, and the
    Markdown zip;
  - the file opens with the page title, an "Exported from knot · date" line,
    real headings, and page numbers in the footer;
  - mentions, dates, page links, checklists, tables, attachments and comments
    are handled as listed in §6.

Calls I made that the user did not weigh in on are marked **(my call)**.

## Current state (verified in the code)

- **Content lives in a yrs Y.Doc** (`"default"` XmlFragment, snake_case
  element tags). The schema is `tools/schema.json`. There is no ProseMirror
  JSON on the server. The main server-side reader is the Markdown serializer,
  which walks the XmlFragment directly. The room actor's
  `patch_task_checked` (`crates/knot-crdt/src/room.rs:675`) also walks it,
  looking for list items.
- **Markdown loses information an export needs:** table column widths,
  `colspan`/`rowspan`, block content inside cells, and attachment metadata.
  **This export must walk the Y.Doc, not the Markdown.**
- **Table column widths:**
  - `colwidth` is a per-cell array with one entry per spanned column. It is
    stored in the Y.Doc, though missing from `tools/schema.json`.
  - Values are normalised weights summing to 1000 once a column has been
    dragged, but older pixel widths and null or 0 entries are also valid.
  - The editor works out the final shares in `resolveShares`
    (`web/src/features/editor/columnWidths.ts`). The export must port that
    logic so widths match what users see.
- **Live state:** `refresh_markdown_inner`
  (`crates/knot-server/src/routes/api/markdown.rs:85-108`) asks the room actor
  for a snapshot (`Event::ExportState`) and applies it to a temporary doc. The
  export copies that snapshot step. It must not call `refresh_markdown_and_index`
  or `export_markdown_only`, which also write `doc_markdown_cache` and
  re-index tasks, and it must not read `doc_markdown_cache`, which lags.
- **Existing export:** `GET /api/docs/{id}/export?descendants=` returns a
  Markdown zip (`routes/api/export_import.rs`).
  - Access goes through `acl.effective_role`. Viewers can't see archived
    pages, so they get 403. Editors and owners get 404 on archived *and*
    template pages, because `list_alive` filters both.
  - The page header's download icon (`data-testid="doc-export"`,
    `web/src/features/docs/DocPage.tsx`) is hidden on archived pages.
    Shift-click exports the subtree.
  - Tests click it: `e2e/flows/mobile.spec.ts:81` and
    `web/src/features/docs/DocPage.archived.test.tsx:127`.
- **Per-page routes** mount in `routes/api/docs.rs` `doc_id_routes` behind
  `require_doc_role_mw`. For non-GET requests from editors and owners, that
  middleware returns 409 `doc.archived`. Viewers get 404 on archived pages.
- **Request bodies:** `DefaultBodyLimit` (2 MB, `lib.rs:207`) only affects
  extractors that opt in. Handlers that read the raw request set their own
  caps: blob upload uses `multer` at 10 MiB (`routes/api/blobs.rs:23`), board
  SVG 1 MiB, zip import 50 MiB. Every API route has a 30 s timeout
  (`lib.rs:208`), and it includes reading the request body.
- **Special inline content is link marks with sentinel hrefs:**
  `knot://doc/<uuid>` (page links), `knot://user/<uuid>` (mentions, text
  `@Name` frozen at insertion), `knot://time/<rfc3339>` (dates, text in the
  author's local format at insertion).
- **Images:**
  - `image` nodes carry `src`, `alt` and `title`, with no stored size.
    Uploaded images have `src="/api/blobs/<uuid>"`. Images imported from
    Markdown can have any `src`: external `http(s)`, relative paths, `data:`.
  - Blobs live in Postgres or S3 behind `BlobStore`.
  - Access is checked against **the page the blob was uploaded to**
    (`blobs.doc_id`), not the page that shows it.
  - Uploads are capped at 10 MB. Nothing limits how many images a page has.
- **Attachments** are `attachment` nodes (`url`, `name`, `size`,
  `contentType`).
- **Diagrams exist only in the browser:**
  - Mermaid is a `code_block` with `language="mermaid"`, rendered
    client-side (`web/src/features/editor/nodes/MermaidCodeBlock.tsx`).
    - Mermaid is a global singleton, initialised once with
      `securityLevel: "strict"`.
    - Its output goes through `sanitizeSvg`, which strips `foreignObject`.
    - Mermaid's default HTML labels use `foreignObject`, so flowchart labels
      may already be missing in the editor today. This is unverified and
      outside this feature; see Out of scope.
  - An Excalidraw board is an `excalidraw_board` node (`board_id`, `label`)
    with its own Y.Doc. The browser renders a preview SVG with `exportToSvg`
    when the board editor closes and uploads it (`PUT /api/boards/{id}/svg`,
    1 MiB cap, stored in `boards.svg_cached`). The preview can be missing if
    nobody has opened the board yet. It can also be stale.
- **Runtime:** static musl binary in a `scratch` image with a read-only root
  filesystem and no fonts or external binaries. The Helm default is 1
  replica with a 512 Mi memory limit and 1 CPU, shared with the live
  collaboration rooms.
- **CSP** allows `img-src data: blob:` and `worker-src blob:`, so browser-side
  rasterizing through a canvas and downloading through a `blob:` link work.
  There is no `wasm-unsafe-eval`.
- **Dependency policy** (`deny.toml`): permissive licenses plus MPL-2.0.
  Unmaintained advisories fail the check. `zip` 8 and `multer` are already
  dependencies. Probes during the brainstorm found `docx-rs` passes; typst,
  krilla and resvg don't.
- **Error codes** follow `<area>.<reason>` (`import.not_zip`,
  `blob.too_large`). This feature adds the `export` area.

## Design

### 1. UI

- The page header's download icon becomes a small menu (my call: same spot,
  same icon, `data-testid="doc-export"` on the trigger) with these items:
  - **Word document (.docx)**
  - **OpenDocument text (.odt)**
  - a separator
  - **Markdown (zip)**: today's single-page zip
  - **Markdown with subpages (zip)**: today's shift-click, now a visible
    item for keyboard and touch users (my call: the shift-click shortcut goes
    away). The tests at `mobile.spec.ts:81` and
    `DocPage.archived.test.tsx:127` are updated to match.
- **Visibility:** anyone who can read the page (viewer, editor, owner) sees
  the menu, on regular and template pages. It stays hidden on archived
  pages.
- **Progress:** the trigger shows "Preparing…" or a spinner while diagrams
  render and the server builds the file. A second export can't start while
  one is running.
- **Unsaved edits** (my call): the file is built from the server's copy. If
  the page still has unsynced local edits (the `pendingBytes` the page
  already tracks), the export first waits up to 5 s for them to sync. If
  they're still pending, it exports anyway and the success toast adds
  "Your latest edits weren't saved yet and may be missing."
- **Outcome:**
  - Success: the file downloads as `<page title>.docx` / `.odt`. Characters
    that file systems reject are replaced, and an empty title becomes
    "Untitled".
  - If anything fell back (§4), a toast says what: "Exported. 2 diagrams
    and 1 image couldn't be included."
  - Failures use the existing toast (`useUi().notify`) with an actionable
    message.
- The menu is keyboard-accessible (opens with Enter/Space, arrow keys move,
  Escape closes) and works at phone width.

### 2. Flow

```text
browser                                         knot-server
  │ GET /api/docs/{id}/export/diagrams  ───────►  walk live Y.Doc, list diagrams
  │ ◄─── {limits, diagrams:[{key, kind:"mermaid", source} | {key, kind:"board", board_id, label}]}
  │ render each to PNG at 2× (§8), fit within limits
  │ POST /api/docs/{id}/export/{docx|odt}  ────►  walk live Y.Doc → export model
  │   multipart: one PNG per diagram key          resolve images, mentions, links
  │                                               write DOCX or ODT (in memory)
  │ ◄─── 200 file bytes + Content-Disposition + fallback counts
  │ save via blob: link, toast about fallbacks
```

The server says which diagrams the page has (my call), so the browser
doesn't depend on the editor's local state. That keeps the flow usable from
places without a loaded editor: the doc tree later, or the mobile app. A
diagram whose picture doesn't arrive, because it failed to render, was added
between the two calls, or was dropped to fit the limits, falls back (§6). It
never fails the export.

### 3. API

- **Mounting:** both routes mount in `routes/api/docs.rs` `doc_id_routes`
  behind `require_doc_role_mw`, so access matches the rest of the page API.
  Viewer or higher can use them, on regular and template pages.
- **Archived pages:** refused on both routes with the middleware's semantics,
  so viewers get 404 and editors and owners get 409 `doc.archived`. The GET
  route checks this itself, because the middleware lets GETs through.
- `GET /api/docs/{id}/export/diagrams` → `200` with:
  - `limits`: max total body bytes, max bytes per part, max parts, and the
    scale (2).
  - `diagrams`, each with:
    - `key`: Mermaid is `"mermaid-" + hex(sha256(source))` over the code
      block's text exactly as stored; a board is `"board-" + board_id`.
      Identical sources collapse to one entry.
    - `kind`, plus `source` for Mermaid, or `board_id` and `label` for boards.
- `POST /api/docs/{id}/export/{format}`, where `format` is `docx` or `odt`:
  - **Body:** `multipart/form-data`. Each part is named by a diagram key and
    holds an `image/png` rendered at 2× CSS pixels. An empty body is valid
    and means no diagram pictures.
  - **Auth:** session plus CSRF, like every POST.
  - **Response:** `200` with
    `application/vnd.openxmlformats-officedocument.wordprocessingml.document`
    or `application/vnd.oasis.opendocument.text`,
    `Content-Disposition: attachment` with an RFC 6266 `filename*` UTF-8
    name, and a header `X-Knot-Export-Fallbacks` (e.g.
    `diagrams=2, images=1`) for the toast.
  - POST, not GET, because it carries the pictures (my call).
- **Limits** (my call; values live as named constants and are returned by
  the diagrams endpoint):
  - **Diagram upload:**
    - The total body is capped at **10 MB**, the same as blob uploads, so any
      reverse proxy that already allows image uploads also allows exports.
    - Each part is capped at 2 MB, with at most 100 parts.
    - The client downsizes pictures to fit.
    - The server treats a part over the per-part cap, or past the count, as
      missing (fallback). It returns `413 export.too_large` only when the
      whole body exceeds 10 MB.
    - On `413` or `408` the client retries once with no pictures, and the
      toast reports the diagrams as not included.
  - **Embedded images:** at most **40 MB** of image bytes per export. Images
    past the budget become the placeholder "[Image too large to include]",
    linking to the image in knot, and count as fallbacks.
  - **Concurrency:** at most **2 exports at a time per server process**
    (a semaphore). A further request waits up to 5 s, then gets
    `503 export.busy` with `Retry-After`. The toast says "Export is busy,
    try again in a moment."
  - With these numbers, worst-case export memory stays around 200 MB
    (two exports, each holding its images, its pictures and the output),
    well inside 512 Mi. The builder may stream the zip into the response to
    lower that, but doesn't have to.
  - Everything has to finish inside the 30 s timeout, including the upload.
- **Errors:** unknown format → `400 export.bad_format`. Other `export.*`
  codes as above.
- **Pictures:** unknown part names are ignored. Pictures are only ever used
  for diagram keys that are actually on the page, so a client can't smuggle
  in arbitrary images.

### 4. Server architecture

- **New pure crate `knot-export`** (my call), sitting next to
  `knot-markdown`. It does not touch the database or the network.
  1. **Walk** the XmlFragment into an export document model: blocks and
     inlines with resolved attributes, the table grid with spans and resolved
     column shares (the port of `resolveShares`), and list nesting.
  2. **Write** that model with two writers, `docx` and `odt`. Both share the
     model and one style vocabulary (§7).
- **knot-server** owns the routes. It:
  - takes the live snapshot (copying the step in `markdown.rs:85-108`);
  - passes the crate a **resolver** for things that need IO:
    - image bytes through `BlobStore` and blob metadata, checking read
      access on each blob's own page, within the image budget;
    - mention display names from `users`;
    - page links as `{base_url}/doc/<uuid>`;
    - diagram PNGs from the request;
  - returns the bytes along with the fallback counts.
- **Hand-write OOXML and ODF** as zip plus XML (my call; no `docx-rs`):
  - template support later means taking control of `styles.xml`, numbering
    and headers/footers anyway (§10);
  - no ODT crate is mature;
  - one walker feeding two thin writers keeps the formats consistent;
  - `zip` is already a dependency.

  The builder may use an XML writer crate if it passes `deny.toml`.
- **Degrade per element:**
  - An unknown element renders its text content as plain paragraphs.
  - An unknown mark is dropped and its text kept.
  - A malformed structure, such as a text node directly under `list_item`
    (a known shape from Markdown imports), is repaired or flattened, never
    rejected.
  - Every fallback (unknown element, diagram without a picture, unavailable
    or over-budget image) is counted by kind, logged at `warn` with the doc
    id, and included in the response's fallback counts.

### 5. Export check over real pages

A CLI subcommand, `knot-server export-check` (my call), next to the existing
`migrate` and admin commands.

- It exports every live page in the database to both formats, in memory and
  without diagram pictures.
- It reports pages that failed, plus fallback counts by kind.
- It changes nothing.

Run it against a staging copy or a restored backup before release. This is
the strongest check of the "one odd element" anti-goal, since hand-built
fixtures only cover shapes we already know about.

### 6. Content mapping

| Page content | In the document |
| --- | --- |
| Page title | Title style, first paragraph |
| (export line) | Subtitle style: `Exported from knot · 23 Oct 2026` (date in `D Mon YYYY`, server's UTC date; my call) |
| `heading` 1–6 | Heading 1–6 styles with outline levels 1–6 |
| `paragraph`, empty paragraph | Normal; empty ones kept as empty paragraphs |
| bold / italic / underline / strike | Direct run formatting |
| inline `code` | "Inline Code" character style (monospace) |
| `link` (http/https/mailto) | Hyperlink (Hyperlink character style) |
| `knot://doc/<id>` | Hyperlink to `{base_url}/doc/<id>`, link text unchanged |
| `knot://user/<id>` | Plain text `@<current display name>`; the stored text if the user is gone (my call) |
| `knot://time/<iso>` | Plain text as stored (what readers saw in the page) (my call) |
| any other `knot://` href, or other schemes | Plain text, no link. A `knot://` URL never reaches the file. |
| `hard_break` | Line break |
| `bullet_list` / `ordered_list` (nested, `start`) | Real list numbering, bullets or decimal per level, nesting kept, `start` honored |
| `list_item` with `checked` | No bullet; `☐` / `☑` (U+2610 / U+2611) before the text, at the item's nesting level (my call: characters, not Word content controls, so Word and LibreOffice look the same) |
| `blockquote` | Quote style (indented, left rule) |
| `code_block` (not Mermaid) | "Code" paragraph style: monospace, light shading, whitespace, tabs and line breaks preserved; no highlighting |
| Mermaid `code_block` | The picture for its key; if missing, the source as a Code block with an italic note "Mermaid diagram (not included)" (my call) |
| `excalidraw_board` | The picture for its key; if missing, an italic placeholder "Diagram: [board label] (not included)" (my call) |
| `horizontal_rule` | Empty paragraph with a bottom border |
| `image`, `/api/blobs/<id>`, readable, within budget | Embedded (§9), scaled to fit the text width and never enlarged past natural size at 96 dpi; `alt` used as the picture's description |
| `image`, blob unreadable or missing | Italic placeholder "[Image not available]" with the alt text |
| `image`, over the image budget | Italic placeholder "[Image too large to include]", linked to the image in knot |
| `image`, external `http(s)` URL | Not fetched (no outbound requests, no SSRF); a hyperlink with the alt text, or the URL (my call) |
| `image`, any other `src` (relative path, `data:`, other schemes) | Italic placeholder "[Image not available]" with the alt text; never a link (my call) |
| diagram pictures (Mermaid, boards) | Placed at half their pixel size (they arrive at 2×), i.e. their on-screen size at 96 dpi, scaled down to the text width if larger |
| `attachment` | `📎 name (size)`, hyperlinked to `{base_url}/api/blobs/<id>`, which needs a login to open |
| `table` | Real table. Column widths from the resolved shares as percentages of the text width. `colspan`/`rowspan` become real merges. Block content in cells (lists, images, several paragraphs) is kept. Header cells form a repeating header row in a Table Header style. Cell `align` honored. Thin grey borders. |
| comments, presence, history | Left out |

### 7. The neutral look (template-ready)

- **Page:** A4 portrait (my call: German company context), 2.5 cm margins,
  footer `Page X of Y` centered (DOCX `PAGE`/`NUMPAGES` fields; ODT
  `text:page-number` / `text:page-count`). No header.
- **Fonts:** named, not embedded (my call): body and headings
  Calibri-compatible ("Calibri", which LibreOffice maps to the
  metric-compatible Carlito), code "Consolas" with fallbacks. The builder may
  pick another widely available pair if it renders better in both suites.
- **DOCX compatibility:** the file declares Word 2013+ compatibility mode
  (`w:compat` with `compatibilityMode` 15), so Word doesn't open it in
  Compatibility Mode.
- **Styles:** every piece of formatting goes through a **named style**. Body
  text gets no direct font or size formatting. The style set is fixed and
  small:
  - Normal
  - Title and Subtitle
  - Heading 1–6
  - Quote
  - Code, plus the Inline Code character style
  - List Paragraph, plus one bullet and one numbered definition
  - Table Grid, plus Table Header
  - Hyperlink
  - Caption / placeholder
  - Footer

  In DOCX, built-in styles use Word's built-in names ("heading 1", "Title",
  …) with `w:outlineLvl`, so Word treats them as its own. In ODT, the names
  are LibreOffice's (`Heading_20_1` …) with `text:outline-level`.
- **Metadata:** document title = page title. Creating application = "knot".
  No author field (my call: keeps the exporter's name out of forwarded files;
  revisit with templates).

### 8. Diagrams in the browser

- **Mermaid:**
  - Render the source and rasterize the SVG to PNG on a canvas at 2× CSS
    pixels.
  - HTML labels (`foreignObject`) taint the canvas in some browsers and
    can't be drawn by Word. Export renders therefore use SVG text labels
    (`htmlLabels: false`, which currently also needs `flowchart.htmlLabels:
    false`), with `securityLevel: "strict"`.
  - Mermaid is a global singleton. The builder must make sure the export
    config doesn't leak into editor renders, for example by re-initialising
    around export renders and restoring afterwards.
- **Boards:** fetch the stored preview (`GET /api/boards/{id}/svg`) and
  rasterize it the same way. With no preview (`board.no_preview`), send
  nothing, and the placeholder applies. Rendering from the board's Y.Doc with
  Excalidraw's `exportToBlob` is allowed if it proves simple, but isn't
  required.
- **Running the renders:** in parallel, with a small concurrency cap and a
  per-diagram timeout. A failure skips that diagram, not the export.
- **Size:** PNGs are capped at 2000 px on the long side, then downsized
  further as needed to fit the limits the diagrams endpoint returned.
- **Browsers:** automated e2e runs in Chromium, as CI does today. Before
  release there is one manual pass of the diagram step in Firefox and Safari
  (my call). Canvas tainting and SVG-as-image font loading differ by engine.

### 9. Image formats

- PNG, JPEG and GIF embed as they are.
- **WebP is always converted to PNG on the server** (my call). Older Word
  versions can't show WebP, and converting removes the question. Use a
  pure-Rust decoder and PNG encoder that pass `deny.toml` (e.g. the
  `image-webp` and `png` crates). The builder confirms the license check and
  the binary-size cost.
- Other types render as the "[Image not available]" placeholder. SVG uploads
  are already `attachment` nodes.
- Picture dimensions are read from the image header to size the frame.

### 10. Ready for company templates (follow-up, not built now)

The follow-up lets a workspace owner upload a Word template (.docx/.dotx) and
an ODT template (.ott). The export then:

- keeps the template's styles, theme, numbering, header/footer and page
  setup;
- replaces the body;
- maps our style vocabulary onto the template's styles by built-in name.
  Word localizes style IDs (a German template has `berschrift1` where English
  has `Heading1`) but keeps the built-in `w:name`, so mapping goes by name.

This design prepares for that by:

1. keeping all formatting in named styles (§7);
2. keeping style definitions, numbering definitions, footer and section setup
   each in one place in each writer, so they can be swapped;
3. making the writers take their "package defaults" (styles.xml and friends)
   as an input with built-in defaults, rather than hard-wiring them through
   the code.

### 11. Observability

- A counter `knot_export_total{format, outcome}` (outcome `ok`,
  `client_error`, `busy` or `server_error`) and a duration histogram,
  registered and described in `knot-obs` like the existing metrics.
- `knot_export_fallback_total{kind}`, with kind one of `diagram`,
  `image_unavailable`, `image_over_budget` or `unknown_element`.
- Add an SLO line to `docs/SLO.md` only if the builder can state a target
  backed by measurement. Otherwise note it as a follow-up.

## Testing

Write tests first where practical (repo norm: TDD).

- **`knot-export` unit tests:**
  - **Fixture docs:** built in code with a `DocBuilder` like the one in
    `crates/knot-markdown/tests/round_trip.rs`. Cover every node and mark in
    §6, deep nesting, mixed checklists, and merged cells whose grid needs
    computing.
  - **Column widths:** pixel, null and normalised `colwidth` values resolve
    like the editor's `resolveShares`.
  - **Degradation cases:** an unknown element, a text node under
    `list_item`, an empty table.
- **Writer tests:** unzip and parse the output, then assert:
  - **Package integrity:** every relationship id resolves, every media part
    has a content type, every style and numbering id referenced exists. ODT:
    `mimetype` is the first entry and stored uncompressed, and the manifest
    lists every file.
  - **Structure:** headings use heading styles with outline levels, table
    grids add up, spans are encoded, list nesting is right, the
    compatibility mode is declared.
  - **Well-formed XML everywhere.**
- **Server integration tests** (`crates/knot-server/tests`, `fresh_db`):
  - **Access:** viewer gets 200; template pages export; non-member and
    anonymous are refused.
  - **Archived pages:** viewer gets 404; editor gets 409 on both routes.
  - **Request errors:** missing CSRF returns 403; bad format returns 400;
    a body over 10 MB returns 413; an oversized part or extra parts fall
    back instead.
  - **Busy:** with the semaphore held, a request returns 503.
  - **Content:**
    - an image from a page the user can't read becomes the placeholder;
    - the image budget is enforced, with a placeholder;
    - WebP is converted;
    - mentions use the current display name;
    - page links use `base_url`;
    - diagram parts are used only for keys on the page;
    - the diagrams endpoint lists keys that match what the writer looks up;
    - fallback counts appear in the header.
- **`export-check` CLI:** a test over a small seeded workspace, including a
  malformed page.
- **Web (Vitest):**
  - **Menu:** items and visibility (readers, templates, hidden when
    archived), keyboard behavior.
  - **Diagram step:** with Mermaid mocked, a render failure skips that
    diagram; downsizing respects the limits; a 413 triggers a retry without
    pictures.
  - **Unsaved edits:** the export waits for pending edits, then warns.
  - **Results:** the fallback toast wording; the busy and error toasts; the
    file is named after the page title.
- **e2e** (required for new user-visible surface, Chromium): export a page
  containing a heading, a checklist, a table and a Mermaid block to DOCX and
  ODT. Assert the download is a zip containing `word/document.xml` /
  `content.xml` with the title, the heading style, and a picture part for
  the diagram. Update `mobile.spec.ts:81` for the menu.
- **Manual acceptance before merge** (my call; knot has no feature flags, and
  releases are cut from main):
  - Export a "kitchen sink" page to both formats and attach the files to
    the PR.
  - The project owner opens them on company machines in:
    - Word for Microsoft 365 (Windows and macOS);
    - Word 2019 or newer;
    - Word for the web (Teams/SharePoint preview);
    - LibreOffice 7.6 or newer.

    Google Docs is best-effort, and only with non-sensitive sample content.
  - **Pass means:**
    - the file opens without a repair prompt or a Compatibility Mode banner;
    - the navigation pane lists the headings;
    - an inserted TOC lists the headings;
    - "Page X of Y" is right;
    - tables keep their widths and merges;
    - the pictures are present;
    - checklists show their boxes.
  - Run `export-check` against a staging copy or restored backup if one is
    available.

## Constraints

- One static binary; no new runtime processes or services; works within 512
  Mi memory and the 30 s timeout. Exports are built in memory; no disk.
- `cargo deny check` must not gain new license or advisory exceptions for
  this feature.
- No outbound network calls during export.
- No change to the existing Markdown export's server behavior or to the CSP.
- The binary should grow by low single-digit MB at most. The image-size
  target in the foundation spec stands.
- Documentation:
  - a CHANGELOG entry under Unreleased (Added);
  - the knot-api skill (`.claude/skills/knot-api/SKILL.md`) lists the new
    routes;
  - `ARCHITECTURE.md`'s crate table gains `knot-export`;
  - the Helm README notes that exports upload up to 10 MB, the same as
    images.

## Left to the builder

- Crate layout and naming inside `knot-export`. The XML writing approach.
  Whether DOCX and ODT share more code than the model.
- Exact style metrics (sizes, spacing, colors), within "neutral and tidy"
  and the mockup's look.
- Whether the zip is streamed into the response.
- The multipart parsing details (`multer`, as blob upload uses).
- How the Mermaid export config is applied and restored; the rasterizing
  details.
- The image-header parsing approach and the WebP crates (within §9).
- Whether the menu is a new small component or reuses an existing
  menu/popover pattern in `web/src/components`.

## Out of scope / follow-ups

- **Company templates** (§10). Next in line for export. Revisit
  classification markings there (header/footer text or sensitivity labels).
- **PDF.** Options from the brainstorm research:
  - typst in-process: about +35 MB binary, plus advisory exceptions;
  - browser print;
  - an optional sidecar.
- **Page plus subpages** in one document.
- **Mobile app export** through the same endpoints.
- **The zip export's filename bug and template 404** (Current state).
- **Mermaid flowchart labels possibly missing in the editor**, because
  `sanitizeSvg` strips `foreignObject` (Current state). Worth checking
  separately.
- **Git sync** of Markdown files, as a separate brainstorm.
