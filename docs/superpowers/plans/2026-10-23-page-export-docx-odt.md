# Page Export to DOCX and ODT Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Export the open knot page to a neutral, well-structured .docx or .odt, with diagrams included as pictures and every element degrading gracefully.

**Architecture:**
- A new pure crate `knot-export` walks the page's yrs XmlFragment into an export model.
- Two hand-written writers (OOXML, ODF) turn that model plus resolved assets into zip bytes.
- `knot-server` adds two routes behind `require_doc_role_mw`:
  - one lists the diagrams on the page;
  - one takes PNG pictures as multipart and returns the file.
- It also adds an offline `export-check` CLI.
- The SPA replaces the download icon with a menu. It renders Mermaid and board previews to PNG and saves the response through a `blob:` link.

**Tech Stack:**
- Server: Rust 2024 (toolchain 1.97.1), yrs 0.27, zip 8, sha2 0.10, image-webp 0.2, png 0.18, imagesize 0.15, axum 0.8, multer 3.
- Web: React + TanStack Query + valibot, Mermaid 11.16.
- Tests: Vitest, Playwright (Chromium).

**Spec:** `docs/superpowers/specs/2026-10-23-page-export-docx-odt-design.md`
Read it before any task, including its "Planning clarifications" section at
the end. The mockup is `docs/superpowers/specs/2026-10-23-page-export-mockup.html`.

## Global Constraints

- **Footprint:** one static binary, no new runtime process or service, no outbound network calls during export, everything in memory.
- **Dependency policy:** `cargo deny check` gains no new license or advisory exception. Don't call `zip::ZipArchive::extract`. Keep `zip = { version = "8", default-features = false, features = ["deflate"] }`.
- **Limits** (binary MB, as named constants):
  - body 10 MiB;
  - per picture part 2 MiB;
  - at most 100 parts;
  - image budget 40 MiB per export;
  - 2 concurrent exports per process;
  - busy wait 5 s, then `503 export.busy` with `Retry-After: 5`;
  - diagram scale 2;
  - diagram PNG long side ≤ 2000 px;
  - WebP pixel cap 16,777,216 (4096×4096). The image budget counts the bytes after preparation too (see Task 12).
- **Page:** A4 (DOCX `w:w="11906" w:h="16838"`), margins 2.5 cm (`1417` twips), text width 16 cm (9072 twips, 5,760,720 EMU). Footer `Page X of Y`, centred.
- **Fonts:** body and headings "Calibri", code "Consolas". Named, never embedded.
- **Error codes:** `export.bad_format` (400), `export.too_large` (413), `export.busy` (503), plus the existing `doc.archived` (409), `doc.not_found` (404), `acl.no_grant` (403), `auth.csrf` (403) and `auth.session_required` (401).
- **Response header:** `X-Knot-Export-Fallbacks: diagrams=<n>, images=<m>, unknown=<k>`. The client reads `diagrams` and `images` and ignores other keys. `unknown` counts unknown elements and is not shown to users.
- **Placeholders and labels (exact copy):**
  - "[Image not available]"
  - "[Image too large to include]"
  - "Mermaid diagram (not included)"
  - "Diagram: <label> (not included)", with label falling back to "Diagram"
  - subtitle "Exported from knot · <D Mon YYYY>"
  - footer "Page X of Y"
- **Toast copy (exact):**
  - "Exported. 2 diagrams and 1 image couldn't be included." (singular "1 diagram" / "1 image"; only the non-zero parts; "and" joins two)
  - unsynced suffix " Your latest edits weren't saved yet and may be missing."
  - "Export is busy, try again in a moment."
  - "Couldn't export this page. Try again, or export as Markdown."
- **Filename:** replace `/ \ : * ? " < > |` and control characters with `-`, trim spaces and dots, cap at 120 **code points** (Rust `chars().take(120)`, JS `Array.from(s).slice(0, 120)`), trim spaces and dots again, use "Untitled" when empty, then append `.docx` / `.odt`. Same rule on server and client.
- **Repo checks:**
  - **Rust:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo nextest run --workspace --all-features`, `cargo deny check`.
  - **Web** (in `web/`): `pnpm tsc`, `pnpm lint`, `pnpm test`.
  - **e2e** (in `e2e/`): `pnpm playwright test flows/export.spec.ts`. See CONTRIBUTING.md and the dev-stack notes for the local stack.
- **Metrics labels:** `format` ∈ {`docx`, `odt`, `unknown`}. `outcome`: 200 → `ok`; 400, 413 and other 4xx → `client_error`; 503 → `busy`; 500 → `server_error`. Each non-zero fallback kind is also logged with `tracing::warn!(%doc_id, kind, count)`.
- **Off the async runtime:** image preparation and the walk-and-write step run in `tokio::task::spawn_blocking`, with the export permit held. The pod has one CPU, shared with the live rooms.
- **Commits:** Conventional Commits, scope `export`. The user signs commits through 1Password. If signing fails, stop and ask; never pass `-c commit.gpgsign=false`.
- **Docs:**
  - CHANGELOG `## [Unreleased]` → `### Added`;
  - `.claude/skills/knot-api/SKILL.md` endpoint table;
  - `ARCHITECTURE.md` crate table;
  - `deploy/helm/knot/README.md` "Operational notes": the proxy body size must allow 10 MiB for image uploads and exports.

## Review Focus

1. **Hostile or odd text.** Titles and text with `& < > " '`, control characters (U+0001, U+000B), emoji and RTL text must yield well-formed XML and a usable filename. Tests: Task 6 (`text_with_xml_specials_and_controls_is_well_formed`), Task 13 (`filename_is_sanitized_on_both_sides`), Task 16 (`exportFileName` vectors).
2. **Malformed or dangerous link targets.** `javascript:`, relative paths, URLs with spaces, `data:`. These must become plain text, never a relationship or `text:a`, because Word reports corruption on unparsable targets. Tests: Task 1 (`link_targets_are_classified`), Task 6 (`unsafe_links_have_no_relationship`).
3. **Nesting beyond the formats' limits.** Lists 12 levels deep (Word has 9 levels, ODF 10), a table inside a list item, and a list inside a table cell must stay valid. Tests: Task 7 (`deep_list_clamps_to_level_9`), Task 10 (`table_inside_list_item_is_valid_odf`).
4. **Double-clicking export, or exporting while another export runs.** The UI must not start a second request, and the server must answer busy rather than queue without limit. Tests: Task 17 (`second_click_while_exporting_does_nothing`), Task 13 (`third_concurrent_export_gets_503`).
5. **Very large pages.** 2,000 paragraphs plus a 50×20 table must still export valid, and quickly enough to fit 30 s on the pod. Test: Task 8 (`large_page_exports_valid_docx_quickly`, release-independent budget: under 5 s in debug).

---

## File structure

```text
crates/knot-export/                      NEW pure crate (yrs, zip, sha2, image-webp, png, imagesize)
├── Cargo.toml
├── src/lib.rs                           pub API re-exports
├── src/model.rs                         ExportDoc, Block, Inline, Run, Marks, LinkTarget, List, Table, Cell, ImageSource
├── src/walk.rs                          yrs XmlFragment → ExportDoc (+ degradations), diagrams(), blob_ids(), user_ids()
├── src/tables.rs                        grid computation, resolve_shares / read_column_weights port
├── src/assets.rs                        Meta, Assets, Picture, PicFormat, ImageAsset, Fallbacks, Output, ExportError
├── src/media.rs                         prepare_image(): sniff, dimensions, WebP→PNG; fit_emu(); fit_cm()
├── src/xml.rs                           esc_text/esc_attr (strips XML-invalid chars), ZipOut helper
├── src/docx/{mod,styles,numbering,body,table,picture}.rs
├── src/odt/{mod,styles,body,table,picture}.rs
├── examples/kitchen_sink.rs             writes sample .docx/.odt for manual acceptance
└── tests/{common/mod.rs (#![allow(dead_code)]: fixture builder, unzip, xml, fallback_fixture), walk.rs, tables.rs, media.rs, docx.rs, odt.rs}
crates/knot-server/src/export.rs         snapshot + resolve assets + build (shared by route and CLI)
crates/knot-server/src/routes/api/export.rs   GET diagrams, POST export/{format}
crates/knot-server/src/export_check.rs   offline CLI
crates/knot-server/tests/{export_integration.rs, export_check_integration.rs}
web/src/lib/mermaid.ts                   shared getMermaid() (moved from MermaidCodeBlock)
web/src/features/export/{export.api.ts, exportFile.ts, renderDiagrams.ts, useExport.ts, ExportMenu.tsx} (+ *.test.ts[x])
e2e/support/zip.ts, e2e/flows/export.spec.ts
```

---

### Task 1: `knot-export` crate, model, and text-block walker

**Files:**
- Create: `crates/knot-export/Cargo.toml`, `src/lib.rs`, `src/model.rs`, `src/walk.rs`, `tests/common/mod.rs`, `tests/walk.rs`
- Modify: `Cargo.toml` (workspace `members` += `"crates/knot-export"`)

**Interfaces:**
- Produces (`model.rs`, all `#[derive(Debug, Clone, PartialEq)]`):

```rust
pub struct ExportDoc { pub blocks: Vec<Block> }
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading { level: u8, content: Vec<Inline> },          // level clamped 1..=6
    Quote(Vec<Block>),
    Code { language: String, text: String },
    Mermaid { key: String, source: String },
    Rule,
    List(List),
    Table(Table),
    Image { source: ImageSource, alt: String },
    Board { key: String, board_id: String, label: String },
    Attachment { url: String, name: String, size: Option<u64> },
}
pub enum Inline { Run(Run), Link { target: LinkTarget, runs: Vec<Run> }, Break }
pub struct Run { pub text: String, pub marks: Marks }
#[derive(Default)] pub struct Marks { pub bold: bool, pub italic: bool, pub underline: bool, pub strike: bool, pub code: bool }
pub enum LinkTarget { Web(String), Doc(String), User(String), Plain }
pub enum ImageSource { Blob(String), External(String), Unsupported }
pub struct List { pub ordered: bool, pub start: u32, pub items: Vec<ListItem> }
pub struct ListItem { pub checked: Option<bool>, pub blocks: Vec<Block> }
pub struct Table { pub shares: Vec<f64>, pub rows: Vec<Vec<Cell>> }
pub struct Cell { pub col: usize, pub colspan: usize, pub rowspan: usize, pub header: bool, pub align: Align, pub blocks: Vec<Block> }
#[derive(Default)] pub enum Align { #[default] Left, Center, Right }
```

- Produces (`walk.rs`):
  - `pub struct Walked { pub doc: ExportDoc, pub unknown_elements: Vec<String> }`
  - `pub fn walk(doc: &yrs::Doc) -> Walked`
- Produces (`tests/common/mod.rs`, the fixture tree builder used by every knot-export test):
  - `pub enum N { El(&'static str, Vec<(&'static str, yrs::Any)>, Vec<N>), Text(Vec<(String, Vec<(&'static str, yrs::Any)>)>) }`
  - `pub fn build(blocks: Vec<N>) -> yrs::Doc`
  - `tests/common/mod.rs` starts with `#![allow(dead_code)]`, because each test file uses only some helpers.
  - helpers `p(&str) -> N`, `h(u8, &str) -> N` (level as editor `f64`), `el(tag, attrs, children) -> N`, `t(&str) -> N`, `marked(&str, &[&'static str]) -> N` (each mark as `Any::Map({})`, editor shape), `linked(&str, href) -> N`

- [ ] **Step 1: Write the failing tests** in `tests/walk.rs`:

```rust
#[test] fn empty_doc_walks_to_no_blocks() { assert_eq!(walk(&build(vec![])).doc.blocks, vec![]); }
#[test] fn headings_accept_number_and_string_levels() {
    // h(2,..) writes 2.0_f64; el("heading",[("level","3".into())],..) writes a string
    // assert Heading{level:2..}, Heading{level:3..}; level "9" clamps to 6
}
#[test] fn marks_are_read_by_key_presence() {
    // marked("x", &["bold","italic","underline","strike","code"]) with Any::Map({}) values
    // and a Bool(true) variant: both give Marks{all true}
}
#[test] fn link_targets_are_classified() {
    // https://a.example → Web; mailto:x@y → Web; knot://doc/<id> → Doc(id); knot://user/<id> → User(id);
    // knot://time/2026-10-23T10:00:00Z → Plain; javascript:alert(1) → Plain; "/relative" → Plain;
    // "https://bad host/with space" → Plain (must parse with url::Url); "HTTPS://A.example" → Web("https://a.example/") (normalized);
    // knot://doc/not-a-uuid and knot://user/a b → Plain; adjacent runs with the same href merge into one Link
}
#[test] fn code_quote_rule_and_hard_break() { /* code_block language "rust" text "a\n\tb"; blockquote of two paragraphs; horizontal_rule; paragraph "a"<hard_break/>"b" → [Run a, Break, Run b] */ }
#[test] fn mermaid_key_hashes_plain_text_content() {
    // code_block language "mermaid" with text "graph TD; A-->B" split across two Text children, one bold
    // → Block::Mermaid{key: "mermaid-" + hex(sha256("graph TD; A-->B")), source: "graph TD; A-->B"}
}
```

- [ ] **Step 2: Run, expect a compile failure**
  - Run: `cargo nextest run -p knot-export`
  - Expected: FAIL. `knot_export::walk` does not exist yet.

- [ ] **Step 3: Implement `model.rs` and `walk(doc: &yrs::Doc) -> Walked`**
  - **Source of the content:** read `txn.get_xml_fragment("default")`. `None` means no blocks.
  - **Attributes:** read with `get_attribute(..).map(|v| v.to_string(&txn))`, which handles both the editor's numbers and the import's strings.
  - **Text:** read with `XmlTextRef::diff(&txn, YChange::identity)`. Keep only `Out::Any(Any::String)` inserts.
  - **Marks:** a mark counts when its key is present, whatever its value.
  - **Links:** take `href` from the `link` attribute's `Any::Map`, then classify it:
    - an href that parses with `url::Url::parse` (add `url = "2"`) and has scheme `http`, `https` or `mailto` becomes `Web(url.as_str().to_owned())`, the normalized form;
    - `knot://doc/<rest>` becomes `Doc(rest)`, and `knot://user/<rest>` becomes `User(rest)`, but only when `rest` parses as a `uuid::Uuid` (add `uuid.workspace`). Otherwise it is `Plain`;
    - `knot://time/…` and everything else becomes `Plain`.
  - **Mermaid key:** `format!("mermaid-{:x}", Sha256::digest(source))`.
  - **This task's scope:** text blocks only. Lists, tables and media fall to the generic unknown-element path until Tasks 2–4.
  - **Unknown elements:**
    - at block level, emit a `Paragraph` of their flattened text and push the tag onto `unknown_elements`;
    - inline, keep their text.
  - **Never panic and never return an error.**
  - **Cargo.toml:** copy `crates/knot-markdown/Cargo.toml`'s header. Deps: `yrs.workspace`, `sha2.workspace`, `url = "2"`, `thiserror.workspace`. Dev-deps: `yrs.workspace`.

- [ ] **Step 4: Run, expect PASS**
  - Run: `cargo nextest run -p knot-export`
  - Expected: all tests in `walk.rs` pass.

- [ ] **Step 5: Commit**
  - `git add crates/knot-export Cargo.toml Cargo.lock`
  - `git commit -m "feat(export): knot-export crate with model and text walker"`

### Task 2: Lists, checklists and structural repair

**Files:**
- Modify: `crates/knot-export/src/walk.rs`
- Test: `crates/knot-export/tests/walk.rs`

**Interfaces:**
- Consumes: Task 1 model.
- Produces: `Block::List` populated. No new public functions.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn ordered_list_start_from_number_or_string() { /* start 5.0_f64 and start "3" → List{ordered:true,start:5/3} */ }
#[test] fn checked_accepts_bool_and_string() { /* checked true (Bool), "false" (String), absent → Some(true), Some(false), None */ }
#[test] fn nested_mixed_lists_keep_structure() { /* bullet > ordered(start 2) > bullet(checked items) — exact tree */ }
#[test] fn text_directly_under_list_item_is_wrapped() {
    // list_item containing a raw Text child (the import's soft-break shape) → ListItem{blocks:[Paragraph[Run]]}, unknown_elements empty
}
#[test] fn text_under_blockquote_is_wrapped() { /* same repair for blockquote */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export walk`
- [ ] **Step 3: Implement list walking.**
  - **`checked`:** `"true"` gives `Some(true)`, any other present value gives `Some(false)`, and absent gives `None`.
  - **Repair:** a raw `XmlOut::Text`, or an inline element, directly under a block container (`list_item`, `blockquote`, cells) is wrapped into a `Paragraph`. Consecutive raw inline children share one paragraph.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): walk lists, checklists and repair stray text"`

### Task 3: Tables: grid, spans, and column shares

**Files:**
- Create: `crates/knot-export/src/tables.rs`, `tests/tables.rs`
- Modify: `crates/knot-export/src/walk.rs`

**Interfaces:**
- Produces:
  - `pub fn resolve_shares(weights: &[Option<f64>]) -> Vec<f64>`: a port of `web/src/features/editor/columnWidths.ts` `resolveShares`, `WEIGHT_TOTAL = 1000.0`.
  - `pub fn header_rows(table: &Table) -> usize`: the number of leading rows in which every cell has `header`.
- `Block::Table` populated:
  - `Cell.col` is the grid column after earlier rowspans;
  - `shares.len()` is the grid width;
  - missing `colspan`/`rowspan` default to 1;
  - `colwidth` is read as `Out::Any(Any::Array)` of `Number`/`Null`.

- [ ] **Step 1: Write the failing tests** (`tests/tables.rs`), porting the vectors from `columnWidths.test.ts`:

```rust
#[test] fn shares_all_unset_are_equal() { assert_eq!(resolve_shares(&[None;4]), vec![0.25;4]); }
#[test] fn shares_normalise_known() { assert_eq!(resolve_shares(&[Some(200.),Some(600.)]), vec![0.25,0.75]); }
#[test] fn shares_fill_unset_from_leftover() { /* [400,None,600] → [400/1500,500/1500,600/1500] ; [300,None,None,None] → [0.3, 0.7/3 ×3] ; [500,250,None] → [0.5,0.25,0.25] */ }
#[test] fn shares_ignore_non_positive() { /* [Some(0.),Some(300.),Some(-5.),Some(f64::NAN)] == resolve_shares(&[None,Some(300.),None,None]) */ }
#[test] fn shares_empty() { assert!(resolve_shares(&[]).is_empty()); }
#[test] fn grid_places_cells_after_rowspan() {
    // row0: A(rowspan 2), B ; row1: C  → row1 C.col == 1 ; shares.len()==2
}
#[test] fn colspan_weights_come_from_first_row() { /* first row cell colspan 2 colwidth [300,700] → shares [0.3,0.7] */ }
#[test] fn header_rows_counts_leading_all_header_rows() { /* 2 header rows then body → 2 ; header column only → 0 */ }
#[test] fn cell_blocks_are_kept() { /* cell with bullet_list + code_block + two paragraphs → 4 blocks in order */ }
#[test] fn empty_table_walks_without_panic() { /* table with no rows, and a table whose only row has no cells → Table{shares:[], rows} with no panic */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export tables`
- [ ] **Step 3: Implement `resolve_shares`, the grid build, and `header_rows`.**
  - **Grid width:** the maximum number of columns any row covers, counting rowspans carried down from earlier rows.
  - **Weights:** read from the first grid row. Each column's weight is `colwidth[col - cell.col]` of the cell covering it, and invalid values count as `None`.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): table grid, spans and column shares"`

### Task 4: Media nodes and the asset listings

**Files:**
- Modify: `crates/knot-export/src/walk.rs`, `src/lib.rs`
- Test: `crates/knot-export/tests/walk.rs`

**Interfaces:**
- Produces:
  - `pub enum DiagramRef { Mermaid { key: String, source: String }, Board { key: String, board_id: String, label: String } }`
  - `pub fn diagrams(doc: &ExportDoc) -> Vec<DiagramRef>`: document order, deduplicated by key.
  - `pub fn blob_ids(doc: &ExportDoc) -> Vec<String>`: `ImageSource::Blob` ids in document order, deduplicated.
  - `pub fn user_ids(doc: &ExportDoc) -> Vec<String>`: `LinkTarget::User` ids, deduplicated.
- `image`:
  - `src` `/api/blobs/<uuid>` gives `Blob(uuid)`;
  - `http(s)` gives `External`;
  - anything else gives `Unsupported`.
- `excalidraw_board`: `key = "board-" + board_id`, and the label is `label` or "Diagram".
- `attachment`: `size` parsed as `u64` when > 0.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn image_sources_are_classified() { /* "/api/blobs/<uuid>", "https://x/y.png", "data:image/png;base64,AA", "img/a.png" */ }
#[test] fn board_and_attachment_attributes() { /* board without label → "Diagram"; attachment size 2048.0 → Some(2048), 0 → None */ }
#[test] fn diagrams_are_listed_once_in_order() { /* two identical mermaid blocks + one board inside a table cell → 2 refs, mermaid first */ }
#[test] fn blob_and_user_ids_are_collected_from_nested_blocks() { /* image in list item, mention in table cell */ }
#[test] fn unknown_block_element_degrades_and_is_reported() { /* el("callout",[],[p("hi")]) → Paragraph "hi"; unknown_elements == ["callout"] */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export walk`
- [ ] **Step 3: Implement the three node kinds and the three listing functions.** Recursive over all blocks.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): media nodes and asset listings"`

### Task 5: Assets, media preparation, and XML/zip helpers

**Files:**
- Create: `crates/knot-export/src/assets.rs`, `src/media.rs`, `src/xml.rs`, `tests/media.rs`
- Modify: `crates/knot-export/Cargo.toml`:
  - `zip = { version = "8", default-features = false, features = ["deflate"] }`
  - `image-webp = "0.2"`
  - `png = "0.18"`
  - `imagesize = { version = "0.15", default-features = false, features = ["png","jpeg","gif","webp"] }`

**Interfaces:**
- Produces (`assets.rs`):

```rust
pub struct Meta { pub title: String, pub exported_on: String /* "23 Oct 2026" */, pub base_url: String }
pub enum PicFormat { Png, Jpeg, Gif }
pub struct Picture { pub format: PicFormat, pub bytes: Vec<u8>, pub width_px: u32, pub height_px: u32, pub scale: u32 /* 1 images, 2 diagrams */ }
pub enum ImageAsset { Embedded(Picture), Unavailable, OverBudget }
#[derive(Default)] pub struct Assets {
    pub images: HashMap<String, ImageAsset>,      // key: blob id; absent ⇒ Unavailable
    pub diagrams: HashMap<String, Picture>,       // key: diagram key; absent ⇒ fallback
    pub user_names: HashMap<String, String>,      // user id → display name
}
#[derive(Default, Debug, Clone, Copy, PartialEq)] pub struct Fallbacks { pub diagram: u32, pub image_unavailable: u32, pub image_over_budget: u32, pub unknown_element: u32 }
pub struct Output { pub bytes: Vec<u8>, pub fallbacks: Fallbacks }
#[derive(Debug, thiserror::Error)] pub enum ExportError { #[error("zip: {0}")] Zip(String), #[error("io: {0}")] Io(String) }
```

- Produces (`media.rs`):
  - `pub fn prepare_image(bytes: &[u8]) -> Option<Picture>`:
    - sniffs PNG, JPEG, GIF and WebP by magic bytes;
    - reads dimensions with `imagesize`;
    - turns WebP into PNG with `image_webp::WebPDecoder` (first frame) and `png::Encoder` (`Compression::Fast`). It refuses anything over 16,777,216 pixels (4096×4096) **before** decoding;
    - returns `None` for anything else or on error;
    - always sets `scale: 1`.
  - `pub fn picture_from_png(bytes: Vec<u8>, scale: u32) -> Option<Picture>`: for diagram parts. PNG only, dimensions checked.
  - `pub fn fit_emu(p: &Picture, max_width_emu: u64) -> (u64, u64)`. The natural size is `px / scale × 9525` EMU, scaled down proportionally to the max width and never scaled up.
  - `pub fn fit_cm(p: &Picture, max_width_cm: f64) -> (f64, f64)`. Same rule, at 96 dpi.
- Produces (`xml.rs`):
  - `pub fn esc_text(s: &str) -> String` and `pub fn esc_attr(s: &str) -> String`. Both drop XML-1.0-invalid characters (U+0000–0008, 000B, 000C, 000E–001F, 007F–009F, FFFE, FFFF) and escape `& < >`, plus `" '` in attributes.
  - `pub struct ZipOut`, with:
    - `new() -> Self`;
    - `stored(&mut self, name, &[u8])`. Also used for media files, which are already compressed;
    - `deflated(&mut self, name, &[u8])`;
    - `finish(self) -> Result<Vec<u8>, ExportError>`.

    It wraps `ZipWriter<Cursor<Vec<u8>>>` with `SimpleFileOptions` and no `large_file`.

- [ ] **Step 1: Write the failing tests** (`tests/media.rs`). Build fixtures in-test with the `png` encoder. For WebP, add a committed 2×2 lossless fixture `tests/fixtures/2x2.webp`, generated once with any encoder and checked by its dimensions.

```rust
#[test] fn png_jpeg_gif_dimensions_are_read() { /* 3x2 PNG → Picture{Png,3,2,scale 1}; minimal JPEG/GIF fixtures likewise */ }
#[test] fn webp_is_converted_to_png() { /* 2x2.webp → format Png, width 2, height 2, bytes start with \x89PNG */ }
#[test] fn webp_over_pixel_cap_is_refused() { /* craft a VP8L header declaring 5000x5000 → None, without allocating */ }
#[test] fn unknown_bytes_are_refused() { assert!(prepare_image(b"<svg/>").is_none()); }
#[test] fn fit_scales_down_never_up() { /* max 5_760_720: 800x400 scale 1 (natural 7_620_000 wide) → (5_760_720, 2_880_360); 100x50 → (952_500, 476_250); diagram 400x200 scale 2 → (1_905_000, 952_500) */ }
#[test] fn escaping_drops_invalid_chars() { assert_eq!(esc_text("a\u{1}<b>&\u{B}"), "a&lt;b&gt;&amp;"); }
#[test] fn zip_first_entry_can_be_stored() { /* ZipOut: stored("mimetype"), deflated("a.xml"); read back with zip::ZipArchive: index 0 name "mimetype", compression Stored */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export media`
- [ ] **Step 3: Implement `assets.rs`, `media.rs` and `xml.rs`** with the signatures above.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-export`, then `cargo deny check`.
  - Expected: tests pass; `licenses ok`, and the advisories show no new entries. `ctutils` RUSTSEC-2026-0385 is pre-existing on main, so note it but don't fix it here.
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): assets, image preparation and xml/zip helpers"`

### Task 6: DOCX writer: package, styles, text blocks

**Files:**
- Create: `crates/knot-export/src/docx/{mod,styles,body}.rs`, `tests/docx.rs`
- Modify: `crates/knot-export/src/lib.rs` (re-export `write_docx`)
- Dev-deps: `roxmltree = "0.20"`. Read zips back with `zip::ZipArchive::by_name` only.
- Add to `tests/common/mod.rs`: `pub fn unzip(bytes: &[u8]) -> HashMap<String, Vec<u8>>`, `pub fn xml(s: &str) -> roxmltree::Document<'_>`, and `pub fn fallback_fixture() -> (ExportDoc, Assets)`. The fixture holds a Mermaid block and a board without pictures, plus images that are `Unavailable`, `OverBudget` and `Unsupported`. Tasks 9 and 11 use it.

**Interfaces:**
- Consumes: `ExportDoc`, `Meta`, `Assets`, `Fallbacks`, `Output`, `ExportError`, `esc_*`, `ZipOut`.
- Produces: `pub struct DocxDefaults { pub styles_xml: Cow<'static, str>, pub numbering_xml: Cow<'static, str>, pub footer_xml: Cow<'static, str> }` with `DocxDefaults::builtin()`. These are the template-ready package defaults (spec §10).
- Produces: `pub fn write_docx_with(doc: &ExportDoc, meta: &Meta, assets: &Assets, defaults: &DocxDefaults) -> Result<Output, ExportError>`, and `pub fn write_docx(doc, meta, assets)`, which calls it with `DocxDefaults::builtin()`.
- The writer generates list instances (`w:num`) on top of the defaults' `abstractNum` definitions (Task 7).
- **Style ids, with their built-in `w:name`:**

  | Style id | `w:name` |
  | --- | --- |
  | `Normal` | "Normal" |
  | `Title` | "Title" |
  | `Subtitle` | "Subtitle" |
  | `Heading1`…`Heading6` | "heading 1"…"heading 6", with `w:outlineLvl` 0–5 |
  | `Quote` | "Quote" |
  | `Code` | custom |
  | `InlineCode` | custom, character style |
  | `ListParagraph` | "List Paragraph" |
  | `TableGrid` | "Table Grid" |
  | `TableHeader` | custom |
  | `Hyperlink` | "Hyperlink", character style |
  | `Caption` | "caption" |
  | `Footer` | "footer", centred |
  | `HorizontalRule` | custom, bottom border |
  | `TableNormal` | "Normal Table", the default table style |

- **Package parts:**
  - `[Content_Types].xml` with png/jpeg/gif defaults, `_rels/.rels`, and `docProps/core.xml`: `dc:title`, `dcterms:created`/`modified`, no `dc:creator`;
  - `docProps/app.xml` with `Application` = knot;
  - `word/document.xml`, `styles.xml`, `numbering.xml`, `settings.xml` (`compatibilityMode` 15, no `updateFields`);
  - `word/footer1.xml` with complex `PAGE`/`NUMPAGES` fields;
  - `word/_rels/document.xml.rels`.
- **Body order:**
  1. the Title paragraph;
  2. the Subtitle paragraph "Exported from knot · {exported_on}";
  3. the blocks;
  4. `sectPr` last, with the A4 size, the margins and the footer reference.
- **Child order** (Word rejects files with children out of schema order):
  - pPr: `pStyle, keepNext, keepLines, numPr, pBdr, shd, spacing, ind, jc, outlineLvl`;
  - rPr: `rStyle, rFonts, b, i, strike, noProof, sz, u`;
  - inside `w:style`: `name, basedOn, next, uiPriority, qFormat, pPr, rPr, tblPr`.
- `Caption` is italic (`w:i`). Every placeholder uses it.
- **Available width:** block emitters take an `avail_twips: u32` argument, starting at 9072. It shrinks inside quotes (by the Quote indent, 567), inside list items (by `720*(depth+1)`) and inside table cells (cell width minus 216). Tables (Task 8) and pictures (Task 9) size themselves to it.

- [ ] **Step 1: Write the failing tests** (`tests/docx.rs`), with helpers `unzip(&[u8]) -> HashMap<String,String>` and `xml(&str) -> roxmltree::Document`:

```rust
#[test] fn package_parts_and_relationships_resolve() {
    // every r:id in document.xml exists in document.xml.rels; every non-External rels Target, resolved relative to word/, exists in the zip;
    // every Override PartName exists; core.xml has dc:title == meta.title and no dc:creator
}
#[test] fn empty_page_has_title_subtitle_and_sectpr() { /* body children: p(Title), p(Subtitle "Exported from knot · 23 Oct 2026"), sectPr; pgSz w=11906 h=16838; pgMar all 1417 */ }
#[test] fn headings_use_heading_styles_with_outline_levels() { /* Heading{3} → pStyle "Heading3"; styles.xml Heading3 has name "heading 3" and outlineLvl 2 */ }
#[test] fn every_referenced_style_exists() { /* collect pStyle/rStyle/tblStyle vals → each has a w:style in styles.xml */ }
#[test] fn runs_marks_and_breaks() { /* bold+italic+underline+strike → b,i,u single,strike; code mark → rStyle InlineCode; Break → w:br; "\t" → w:tab; w:t xml:space="preserve" */ }
#[test] fn links_become_hyperlinks_with_external_targets() {
    // Web("https://a.example/x?y=1&z") → w:hyperlink r:id → Relationship TargetMode="External" Target escaped; rStyle Hyperlink
    // Doc("d1") → Target "{base_url}/doc/d1"; User("u1") with user_names{u1:"Ada"} → text "@Ada", no hyperlink
}
#[test] fn unsafe_links_have_no_relationship() { /* LinkTarget::Plain runs → plain w:r; rels contain only styles/numbering/settings/footer */ }
#[test] fn code_block_is_one_shaded_paragraph() { /* "a\n  b" → one p pStyle Code with w:br between, spaces preserved */ }
#[test] fn quote_and_rule_styles() { /* Quote → pStyle Quote; Rule → empty p pStyle HorizontalRule */ }
#[test] fn settings_declare_compat_15_and_footer_fields() { /* compatSetting compatibilityMode val 15; footer1.xml instrText contains " PAGE " and " NUMPAGES " */ }
#[test] fn text_with_xml_specials_and_controls_is_well_formed() { /* title and text "Q4 <draft> & \"final\" \u{1}😀 שלום" → every part parses with roxmltree */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export docx`
- [ ] **Step 3: Implement `write_docx`** for `Paragraph`, `Heading`, `Quote`, `Code`, `Rule`, and inlines (`Run`, `Link`, `Break`).
  - Other blocks emit an empty `Normal` paragraph for now. Tasks 7–9 fill them in.
  - Hyperlink relationship ids are allocated per unique target.
  - `docPr` ids come from one document-wide counter (used in Task 9).
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): docx package, styles and text blocks"`

### Task 7: DOCX writer: lists and checklists

**Files:**
- Create: `crates/knot-export/src/docx/numbering.rs`
- Modify: `crates/knot-export/src/docx/body.rs`
- Test: `crates/knot-export/tests/docx.rs`

**Interfaces:**
- Produces: `numbering.xml` with `abstractNum` 0 (bullets "•", "◦", "▪" cycling, 9 levels) and `abstractNum` 1 (decimal `%N.`, 9 levels, `w:ind left=720*(ilvl+1) hanging=360`). All `abstractNum` elements come before any `num`.
- **Every list instance gets its own `w:num`.** An ordered one carries a `lvlOverride` at its `ilvl` with `startOverride` = `start`, even when that is 1. `ilvl` is the nesting depth, clamped to 8.
- **Checklist items get no `numPr`.** They use `ListParagraph` with `w:ind left=720*(depth+1)`, and their first run is prefixed with "☐ " or "☑ ".

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn each_ordered_list_restarts_with_its_start() { /* two ordered lists (start 1, start 5) → 2 distinct numIds; second has startOverride 5 */ }
#[test] fn nested_mixed_lists_use_depth_as_ilvl() { /* bullet>ordered>bullet → ilvl 0,1,2 with bullet/decimal numIds */ }
#[test] fn checklist_items_use_box_characters() { /* checked Some(true) → first w:t starts "☑ ", no numPr; Some(false) → "☐ " */ }
#[test] fn deep_list_clamps_to_level_9() { /* 12-deep nesting → max ilvl 8; document parses */ }
#[test] fn list_item_with_several_blocks() { /* item [Paragraph, Code] → first p has numPr, Code p indented without numPr */ }
#[test] fn every_num_id_exists_in_numbering() { /* collect w:numId vals in document.xml → each has a w:num in numbering.xml */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export docx`
- [ ] **Step 3: Implement the numbering and list emission** as described above.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): docx lists and checklists"`

### Task 8: DOCX writer: tables

**Files:**
- Create: `crates/knot-export/src/docx/table.rs`
- Test: `crates/knot-export/tests/docx.rs`

**Interfaces:**
- Consumes: `Table`, `Cell`, `header_rows`.
- Produces a table with:
  - `tblStyle TableGrid`, `tblW = avail_twips` (dxa) and `tblLayout fixed`;
  - `tblGrid` with integer `gridCol` twips proportional to `shares`, summing exactly to `avail_twips`, with the rounding remainder on the last column;
  - header rows (`header_rows`) carrying `trPr/tblHeader`;
  - header cells shaded `F2F2F2` with paragraphs in `TableHeader`;
  - `gridSpan` for `colspan`, and `vMerge restart`/continue with the same `gridSpan` for `rowspan`;
  - each row padded with empty cells to the grid width;
  - every `tc` ending in a `w:p`;
  - an empty paragraph between adjacent tables, and after a table that ends a cell;
  - `align` mapped to `w:jc`.
- Content inside a cell gets `avail_twips` = the cell width minus 216.
- A table whose grid width is 0, or that has no rows, writes nothing.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn grid_columns_follow_shares_and_sum_to_text_width() { /* shares [0.3,0.7] → gridCol 2722, 6350; sum 9072 */ }
#[test] fn spans_are_encoded() { /* colspan 2 → gridSpan 2; rowspan 2 → vMerge restart + continuation tc with <w:vMerge/> and <w:p/> */ }
#[test] fn header_rows_repeat_and_are_shaded() { /* 1 header row → tblHeader on row 0 only; shd fill F2F2F2; pStyle TableHeader */ }
#[test] fn rows_are_padded_and_cells_end_with_paragraph() { /* ragged row → padded; cell with only a nested table ends with w:p */ }
#[test] fn adjacent_tables_are_separated() { /* [Table, Table] → body has p between the two tbl */ }
#[test] fn nested_table_fits_its_cell() { /* outer shares [0.5,0.5] → inner tblW == 4536-216 and its gridCols sum to it */ }
#[test] fn empty_tables_write_nothing_and_parse() { /* no rows; a row with no cells → no w:tbl; document parses */ }
#[test] fn large_page_exports_valid_docx_quickly() { /* 2000 paragraphs + 50x20 table; Instant elapsed < 5s (debug); parses */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export docx`
- [ ] **Step 3: Implement the table emission** as described above.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): docx tables with spans and widths"`

### Task 9: DOCX writer: pictures, placeholders, attachments, fallbacks

**Files:**
- Create: `crates/knot-export/src/docx/picture.rs`
- Test: `crates/knot-export/tests/docx.rs`

**Interfaces:**
- Consumes: `Assets`, `fit_emu`, `ImageAsset`, `Picture`.
- `Output.fallbacks`:
  - `diagram` counts Mermaid or Board blocks without a picture;
  - `image_unavailable` counts `Blob` images that are `Unavailable` or absent, plus `Unsupported` sources;
  - `image_over_budget` counts `OverBudget` images;
  - `unknown_element` is left at 0 here. The server adds `Walked.unknown_elements.len()`.
- **Picture XML** (an inline drawing; leaving out `pic:spPr` makes Word report corruption):

  ```xml
  <w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0">
    <wp:extent cx="CX" cy="CY"/><wp:effectExtent l="0" t="0" r="0" b="0"/>
    <wp:docPr id="N" name="Picture N" descr="ALT"/>
    <wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr>
    <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic>
      <pic:nvPicPr><pic:cNvPr id="N" name="imageN.EXT" descr="ALT"/><pic:cNvPicPr/></pic:nvPicPr>
      <pic:blipFill><a:blip r:embed="RID"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
      <pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="CX" cy="CY"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>
    </pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>
  ```

  The `w:document` root declares `w`, `r`, `wp` (`http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing`), `a` (`…/drawingml/2006/main`) and `pic` (`…/drawingml/2006/picture`).
- **Pictures** also need:
  - media at `word/media/image{n}.{png|jpeg|gif}`;
  - a `docPr id` that is unique and positive;
  - `descr` set to the alt text, or "Diagram: label" / "Mermaid diagram" for diagrams;
  - the max width set to the emitter's `avail_twips × 635` EMU.
- **Fallback copy:**
  - Mermaid without a picture: a `Code` paragraph with the source, then a `Caption` paragraph "Mermaid diagram (not included)";
  - Board without a picture: a `Caption` paragraph "Diagram: {label} (not included)";
  - `Unavailable` or `Unsupported`: a `Caption` paragraph "[Image not available]", then " — {alt}" when there is alt text;
  - `OverBudget`: a `Caption` paragraph "[Image too large to include]", hyperlinked to `{base_url}/api/blobs/{id}`;
  - `External(url)`: a hyperlink with the alt text, or the URL;
  - `Attachment`: a `Normal` paragraph "📎 {name} ({size human, e.g. 48 KB})", hyperlinked to `{base_url}{url}` when `url` starts with `/`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn embedded_image_has_media_part_and_unique_docpr() { /* two images → media/image1.png, image2.jpeg; docPr ids distinct > 0; content type defaults present */ }
#[test] fn diagram_picture_is_half_size_and_capped() { /* diagram 400x200 scale 2 → extent cx 1_905_000; 4000x1000 scale 2 → cx 5_760_720 */ }
#[test] fn image_in_table_cell_fits_the_cell() { /* shares [0.5,0.5], 3000px image → cx <= (4536-216)*635 */ }
#[test] fn fallbacks_are_rendered_and_counted() { /* common::fallback_fixture() → captions with exact copy, in Caption style (italic); Fallbacks{diagram:2,image_unavailable:2,image_over_budget:1,unknown_element:0} */ }
#[test] fn attachment_line_links_back_to_knot() { /* size 49152 → "📎 budget.xlsx (48 KB)"; Target "{base}/api/blobs/<id>" */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export docx`
- [ ] **Step 3: Implement the pictures, placeholders, attachments and fallback counting** as described above.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): docx pictures, placeholders and fallbacks"`

### Task 10: ODT writer: package, styles, text, lists, tables

**Files:**
- Create: `crates/knot-export/src/odt/{mod,styles,body,table}.rs`, `tests/odt.rs`
- Modify: `crates/knot-export/src/lib.rs` (re-export `write_odt`)

**Interfaces:**
- Produces: `pub struct OdtDefaults { pub styles_xml: Cow<'static, str> }` with `OdtDefaults::builtin()`, plus `pub fn write_odt_with(doc, meta, assets, defaults: &OdtDefaults)` and `pub fn write_odt(doc, meta, assets)`. Same `Output` and fallback semantics as the DOCX writer.
- **Package:**
  - `mimetype` first, stored, with `application/vnd.oasis.opendocument.text`;
  - `META-INF/manifest.xml` listing every file except `mimetype` and `META-INF/*`;
  - `content.xml`, `styles.xml`, and `meta.xml` with `meta:generator` knot, `dc:title`, and no creator;
  - `office:version="1.3"` on every root.
- **Styles** (`OdtDefaults::builtin().styles_xml`, template-ready):
  - `Standard`, `Title`, `Subtitle`, `Heading_20_1`…`6` (`default-outline-level` N);
  - `Quotations`, `Preformatted_20_Text` (Consolas, background `#f5f5f5`), `Source_20_Text` (character style);
  - `List_20_Paragraph`, `Table_20_Contents`, `Table_20_Heading`;
  - `Internet_20_link`, `Caption` (italic, `fo:font-style="italic"`), `Footer` (centred), `Horizontal_20_Line`;
  - an outline style with an empty `num-format`, so headings aren't numbered;
  - list styles `List_20_Bullet`, `Numbering_20_123` and `List_20_Check`, each with 10 levels and the same indents (1.27 cm per level). `List_20_Check` levels are `text:list-level-style-number` with `style:num-format=""`, so they show no label.
- **Page layout:** A4 portrait, 2.5 cm margins. The bottom margin plus the footer height and spacing come to 2.5 cm. Master page `Standard` has the footer "Page <text:page-number/> of <text:page-count/>".
- **Blocks:**
  - headings are `text:h` with `text:outline-level`;
  - each list is its own `text:list` (no continue-numbering), with `text:start-value` on the first item of an ordered list and nesting clamped to 10 levels;
  - checklist items stay inside their `text:list`, with `text:style-override="List_20_Check"` on the `text:list-item`. Their first paragraph starts with "☐ " or "☑ ". This keeps the nesting level, and lets a bullet list mix checked and plain items;
  - a table inside a list item closes the list, emits the table, and reopens with `text:continue-list` pointing at the list's `xml:id`;
  - **all text** (not just code) goes through one encoder, because ODF collapses whitespace everywhere. The encoder writes:
    - `\n` as `text:line-break`;
    - `\t` as `text:tab`;
    - space runs as one literal space followed by `text:s text:c="N-1"`;
    - leading spaces in a paragraph entirely as `text:s`;
  - links are `text:a` for `Web` and `Doc` only.
- **Tables:**
  - `table:table` with per-column automatic styles carrying both `style:column-width` (cm of 16 cm) and `style:rel-column-width` (shares × 65535, as `N*`);
  - `table:table-header-rows` for `header_rows`;
  - `number-columns-spanned` / `number-rows-spanned` plus `table:covered-table-cell`, so every row has exactly the grid width;
  - a rowspan crossing from the header rows into the body is clipped to the header group;
  - **cell styles:** every cell has an automatic cell style with `fo:border="0.5pt solid #bfbfbf"`. Header cells also get `fo:background-color="#f2f2f2"`;
  - **alignment:** `align` maps to automatic paragraph styles parented to `Table_20_Contents` or `Table_20_Heading`, with `fo:text-align` `start`, `center` or `end`;
  - **widths:** tables take `avail_cm` (starting at 16.0, shrinking the same way as Task 6's `avail_twips`) for `style:width` and the column widths;
  - a table with grid width 0, or with no rows, writes nothing.

- [ ] **Step 1: Write the failing tests** (`tests/odt.rs`; same `unzip`/`xml` helpers):

```rust
#[test] fn mimetype_is_first_and_stored_and_manifest_is_complete() { /* index 0 "mimetype" Stored, exact content; manifest lists content/styles/meta (+ Pictures/* when present) */ }
#[test] fn empty_page_has_title_subtitle_and_footer_fields() { /* text:p Title, Subtitle copy; styles.xml footer contains text:page-number and text:page-count; page-width 21cm */ }
#[test] fn headings_have_outline_levels_and_styles() { /* Heading{2} → text:h outline-level 2 style Heading_20_2; style display-name "Heading 2" */ }
#[test] fn lists_restart_and_checklists_use_boxes() { /* ordered start 5 → first text:list-item start-value 5; checklist "☑ " */ }
#[test] fn mixed_nested_checklist_keeps_levels() { /* bullet[plain, checked[child checked]] → checked items have style-override List_20_Check at depth 1 and 2; plain item has none */ }
#[test] fn tables_have_columns_spans_and_covered_cells() { /* shares [0.3,0.7]; colspan 2 → covered-table-cell; each row cell+covered count == 2; header rows wrapped; cell styles have fo:border; align center → paragraph style with fo:text-align center */ }
#[test] fn empty_tables_write_nothing_and_parse_odt() { /* no rows; a row with no cells → no table:table; content.xml parses */ }
#[test] fn table_inside_list_item_is_valid_odf() { /* list item [Paragraph, Table] → no table:table inside text:list-item; continue-list references xml:id; parses */ }
#[test] fn code_preserves_spaces_and_tabs() { /* "  a\tb   c" → text:s c=2, text:tab, "c" preceded by literal space + text:s c=2 */ }
#[test] fn normal_text_preserves_tabs_and_double_spaces() { /* Paragraph "a\tb  c" → text:tab and one literal space + text:s c=1 */ }
#[test] fn every_referenced_style_exists_odt() { /* all text:style-name / table:style-name values declared in styles.xml or content automatic-styles */ }
#[test] fn text_with_xml_specials_and_controls_is_well_formed_odt() { /* same hostile text as the docx test → all parts parse */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export odt`
- [ ] **Step 3: Implement `write_odt`** for all blocks except media. Until Task 11, `Image`, `Mermaid`, `Board` and `Attachment` blocks write an empty `Standard` paragraph (as Task 6 does).
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): odt writer for text, lists and tables"`

### Task 11: ODT writer: pictures, placeholders, and the kitchen-sink sample

**Files:**
- Create: `crates/knot-export/src/odt/picture.rs`, `examples/kitchen_sink.rs`
- Test: `crates/knot-export/tests/odt.rs`

**Interfaces:**
- **Pictures:** a `draw:frame` (`text:anchor-type="as-char"`, a unique `draw:name`, `svg:width`/`height` in cm from `fit_cm`, max `avail_cm`) containing a `draw:image` pointing at `Pictures/img{n}.{ext}`, followed by `svg:desc` with the alt text. Each picture gets a manifest entry. Placeholders, copy and fallback counting are identical to Task 9.
- **Example:** `cargo run -p knot-export --example kitchen_sink -- <out_dir>` writes `kitchen-sink.docx` and `kitchen-sink.odt`. It builds an `ExportDoc` covering every `Block`/`Inline` variant directly, generates PNGs with the `png` crate, and includes every fallback kind. This is the manual-acceptance file set.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn odt_pictures_have_frames_and_manifest_entries() { /* 2 pictures → Pictures/img1.png, img2.jpeg in zip and manifest; draw:name unique; svg:width "4.233cm" for 160px scale 1 */ }
#[test] fn odt_fallbacks_match_docx() { /* common::fallback_fixture() → same Fallbacks and caption texts as the DOCX test */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-export odt`
- [ ] **Step 3: Implement the ODT pictures and placeholders, and write `examples/kitchen_sink.rs`.**
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-export`, then `cargo run -p knot-export --example kitchen_sink -- target/export-samples`
  - Expected: tests pass, and both files exist and are non-empty.
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): odt pictures and kitchen-sink sample"`

### Task 12: Server export service (snapshot and resolving assets)

**Files:**
- Create: `crates/knot-server/src/export.rs`
- Modify:
  - `crates/knot-server/src/lib.rs`: `pub mod export;` plus the AppState fields below, set in both `in_memory()` and `with_pool()`;
  - `crates/knot-server/Cargo.toml`: `knot-export = { path = "../knot-export" }`;
  - `crates/knot-obs/src/metrics.rs`: an `// Export` block of descriptions.
- Test: `crates/knot-server/tests/export_integration.rs`. Create it here; Task 13 extends it.

**Interfaces:**
- AppState gains `pub export_permits: Arc<tokio::sync::Semaphore>` (default `Semaphore::new(2)`) and `pub export_busy_wait: std::time::Duration` (default 5 s).
- Produces, in `export.rs`:

```rust
pub const IMAGE_BUDGET_BYTES: i64 = 40 * 1024 * 1024;
pub enum Format { Docx, Odt }                         // Format::parse(&str) -> Option<Format>; content_type(); extension()
pub struct Built { pub bytes: Vec<u8>, pub fallbacks: knot_export::Fallbacks, pub title: String }
#[derive(Debug, thiserror::Error)] pub enum ExportFailure { #[error("room: {0}")] Room(String), #[error("store: {0}")] Store(String), #[error("write: {0}")] Write(String) }
pub async fn snapshot(state: &AppState, doc_id: Uuid) -> Result<knot_crdt::DocHandle, ExportFailure>   // copy of markdown.rs:85-108 snapshot step only
pub async fn resolve_assets(state: &AppState, user_id: Uuid, doc: &knot_export::ExportDoc, diagrams: HashMap<String, Vec<u8>>) -> Result<knot_export::Assets, ExportFailure>   // = resolve_assets_with_budget(.., IMAGE_BUDGET_BYTES)
pub async fn resolve_assets_with_budget(state: &AppState, user_id: Uuid, doc: &knot_export::ExportDoc, diagrams: HashMap<String, Vec<u8>>, budget: i64) -> Result<knot_export::Assets, ExportFailure>
pub async fn build(state: &AppState, user_id: Uuid, doc_id: Uuid, format: Format, diagram_pngs: HashMap<String, Vec<u8>>) -> Result<Built, ExportFailure>
pub fn file_name(title: &str, format: Format) -> String       // Global Constraints filename rule
pub fn content_disposition(file_name: &str) -> String          // attachment; filename="<ascii, '_' for non-ascii>"; filename*=UTF-8''<percent-encoded>
pub fn exported_on(now: chrono::DateTime<chrono::Utc>) -> String   // "%-d %b %Y" → "23 Oct 2026"
```

- **`resolve_assets`:**
  - **Images:** for each `blob_ids` entry, in document order:
    - `blob_meta.find(id)` returns `None` → `Unavailable`;
    - `acl.effective_role(meta.workspace_id, meta.doc_id, user_id)` returns `None` → `Unavailable`. This checks the blob's own page, not the exported page;
    - the running sum of `byte_size` would pass `IMAGE_BUDGET_BYTES` → `OverBudget`, without fetching;
    - otherwise `blob_store.get`, then `prepare_image` inside `spawn_blocking`, where `None` → `Unavailable`;
    - after preparing, charge the difference between the prepared `picture.bytes.len()` and `byte_size` to the running total too. A converted WebP can grow. If that pushes the total past the budget, the image becomes `OverBudget` and its bytes are dropped.
  - **Diagrams:** only keys that `knot_export::diagrams(doc)` lists, run through `picture_from_png(bytes, 2)`.
  - **Mentions:** `users.find_by_id` once per `user_ids` entry.
- **`build`:**
  1. `snapshot`, then `walk(handle.inner())` (inside `spawn_blocking`), `resolve_assets`, and `docs.get(doc_id)` for the title;
  2. `Meta { exported_on: exported_on(Utc::now()), base_url: state.base_url.clone() }`;
  3. the writer, inside `spawn_blocking`;
  4. `fallbacks.unknown_element = walked.unknown_elements.len() as u32`;
  5. one `tracing::warn!(%doc_id, kind, count)` per non-zero fallback kind.
- **Metrics:**
  - `knot_export_total{format,outcome}` and `knot_export_duration_seconds`, recorded by the route in Task 13;
  - `knot_export_fallback_total{kind}`, recorded in `build`, with `kind` ∈ `diagram`, `image_unavailable`, `image_over_budget`, `unknown_element`.

- [ ] **Step 1: Write the failing tests.**
  - **Unit tests in `export.rs`:**
    - `file_name("Q4 / Plan: <draft>?", Docx) == "Q4 - Plan- -draft--.docx"`, and `file_name(" .. ", Odt) == "Untitled.odt"`;
    - `content_disposition("Über.docx")` contains `filename*=UTF-8''%C3%9Cber.docx`;
    - `exported_on(2026-10-23T23:59Z) == "23 Oct 2026"`.
  - **Integration tests in `export_integration.rs`.** Adapt the harness from `archive_integration.rs` (`Session`, `login`, the owner/member setup) and `wire_rooms` from `markdown_import_integration.rs`. Two changes are needed:
    - **Harness setup:** `async fn world_with(configure: impl FnOnce(&mut AppState)) -> World` keeps `fresh_db()`'s `url`, calls `wire_rooms(&mut state, &db.url)`, applies `configure` (for example the export permit and wait overrides), and only then builds `router_with_state`;
    - **Full responses:** `async fn send_raw(app: &Router, who: Option<&Session>, method: &str, uri: &str, content_type: Option<&str>, body: Vec<u8>) -> axum::response::Response` returns the whole response, so tests can read headers and bytes. Pass `None` for anonymous calls; include the CSRF header only when `who` is set.
    - Copy `const TINY_PNG: &[u8]` from `blobs_integration.rs:149`.
    - `resolve_assets_checks_blob_access_and_budget`:
      - a blob in another workspace (pattern `blobs_integration.rs:428-492`) → `Unavailable`;
      - a readable 3×2 PNG blob → `Embedded`;
      - `resolve_assets_with_budget` with a budget smaller than the PNG → `OverBudget`.
    - `webp_blob_is_converted_to_png`: upload the 2×2 WebP fixture as a blob, put `![w](/api/blobs/<id>)` in the page, and build. The DOCX has `word/media/image1.png`.
    - `page_links_use_base_url`: `[x](knot://doc/<other id>)` → the hyperlink Target is `{state.base_url}/doc/<other id>`.
    - `build_uses_live_state_and_current_display_names`:
      - seed with `POST /api/docs/{id}/markdown?mode=replace` (`# Title\n\nHi [@Old](knot://user/<owner id>)`);
      - rename isn't possible through the API, so assert the mention resolves to the owner's display name "Olive Owner";
      - the result is non-empty and starts with `PK`.

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-server export`
- [ ] **Step 3: Implement `export.rs` and the AppState fields** with the signatures above.
- [ ] **Step 4: Run, expect PASS.** `cargo nextest run -p knot-server export`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): server export service with asset resolution"`

### Task 13: Export routes

**Files:**
- Create: `crates/knot-server/src/routes/api/export.rs`
- Modify:
  - `crates/knot-server/src/routes/api/mod.rs`: `pub mod export;`
  - `crates/knot-server/src/routes/api/docs.rs`: in `doc_id_routes`, `.merge(crate::routes::api::export::routes())`
  - `CHANGELOG.md`, `.claude/skills/knot-api/SKILL.md`, `ARCHITECTURE.md`, `deploy/helm/knot/README.md`
- Test: `crates/knot-server/tests/export_integration.rs`

**Interfaces:**
- Produces `pub fn routes() -> Router<AppState>` with:
  - `GET /api/docs/{id}/export/diagrams` → `{ "limits": { "max_body_bytes": 10485760, "max_part_bytes": 2097152, "max_parts": 100, "scale": 2 }, "diagrams": [ {"key","kind":"mermaid","source"} | {"key","kind":"board","board_id","label"} ] }`.
    - It returns `doc_archived()` when the `DocArchived` extension is present.
  - `POST /api/docs/{id}/export/{format}`.
    - **Format:** read `format` as a `String`. Anything other than `docx`/`odt` gives `400 export.bad_format`.
    - **Body:** an empty body, or a non-multipart Content-Type, means no pictures. Otherwise use `multer` with `whole_stream(10 MiB)` only, so exceeding it gives `413 export.too_large`.
      - Read each field with a `chunk()` loop: stop keeping bytes after 2 MiB (the part is dropped) and drain the rest.
      - Keep at most 100 parts, and ignore names that aren't diagram keys.
    - **Concurrency:** take a permit with `tokio::time::timeout(state.export_busy_wait, permits.acquire_owned())`. On timeout, return `503 export.busy` with `Retry-After: 5`.
    - **Response:** `200` with the content type, `Content-Disposition` from `content_disposition(file_name(..))`, and `X-Knot-Export-Fallbacks: diagrams={diagram}, images={image_unavailable+image_over_budget}, unknown={unknown_element}`.
  - Errors from `build` → `500 internal`.
  - Record `knot_export_total{format,outcome}` and `knot_export_duration_seconds`, with label values from Global Constraints.

- [ ] **Step 1: Write the failing integration tests:**

```rust
#[tokio::test(flavor = "multi_thread")] async fn viewer_can_export_docx_and_odt() { /* viewer session; POST empty body → 200; content-type docx/odt; body starts PK; Content-Disposition has filename* */ }
#[tokio::test(flavor = "multi_thread")] async fn template_pages_export() { /* docs.set_template(..true) → 200 */ }
#[tokio::test(flavor = "multi_thread")] async fn archived_pages_are_refused() { /* viewer GET diagrams/POST → 404; editor GET diagrams → 409 doc.archived; editor POST → 409 */ }
#[tokio::test(flavor = "multi_thread")] async fn request_errors() { /* no CSRF → 403 auth.csrf; format "pdf" → 400 export.bad_format; anonymous → 401; foreign-workspace doc id → 403 acl.no_grant */ }
#[tokio::test(flavor = "multi_thread")] async fn body_over_limit_is_413_but_big_part_falls_back() { /* 11 MiB body → 413 export.too_large; one 3 MiB part for a real key + valid small part → 200, header diagrams=1 */ }
#[tokio::test(flavor = "multi_thread")] async fn diagrams_listing_matches_writer_keys() { /* seed ```mermaid\ngraph TD; A-->B\n``` → GET lists key mermaid-<sha>; POST with that key's TINY_PNG → header diagrams=0; unknown part name ignored */ }
#[tokio::test(flavor = "multi_thread")] async fn third_concurrent_export_gets_503() { /* world_with(|s| { s.export_permits = Arc::new(Semaphore::new(0)); s.export_busy_wait = Duration::from_millis(50); }) → 503 export.busy, Retry-After 5 */ }
#[tokio::test(flavor = "multi_thread")] async fn parts_beyond_one_hundred_fall_back() { /* page with 101 distinct mermaid blocks; send 101 TINY_PNG parts → 200, header diagrams=1 */ }
#[tokio::test(flavor = "multi_thread")] async fn filename_is_sanitized_on_both_sides() { /* title "Q4 / Plan" → filename*=UTF-8''Q4%20-%20Plan.docx */ }
```

- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-server export_integration`
- [ ] **Step 3: Implement the routes** and update the four docs:
  - **CHANGELOG** `### Added`: Word and OpenDocument export from the page header's download menu, with diagrams as pictures. List `GET /api/docs/{id}/export/diagrams` and `POST /api/docs/{id}/export/{docx|odt}`.
  - **SKILL.md** endpoint table: both routes, plus a curl recipe for `POST` with an empty body.
  - **ARCHITECTURE.md** crate row: `` `knot-export` | Page → DOCX/ODT (pure; walks the Y.Doc) ``.
  - **Helm README** operational note: the proxy body size must allow 10 MiB.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-server export`, then `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - Expected: pass, no warnings.
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): docx/odt export routes"`

### Task 14: `knot-server export-check` CLI

**Files:**
- Create: `crates/knot-server/src/export_check.rs`, `crates/knot-server/tests/export_check_integration.rs`
- Modify:
  - `crates/knot-server/src/main.rs`: `Cmd::ExportCheck` → `export_check::run(cfg)`, which exits 1 on failures or errors;
  - `crates/knot-server/src/lib.rs`: `pub mod export_check;`
  - `deploy/helm/knot/README.md`: one line on running it against a staging copy.

**Interfaces:**
- Produces:

```rust
pub struct CheckReport { pub pages: usize, pub failed: Vec<(Uuid, String)>, pub fallbacks: knot_export::Fallbacks }
pub async fn hydrate(pool: &PgPool, doc_id: Uuid) -> anyhow::Result<knot_crdt::DocHandle>   // latest snapshot + updates since; errors are returned, not swallowed
pub async fn check_all(pool: &PgPool, base_url: &str) -> anyhow::Result<CheckReport>
pub async fn run(cfg: knot_config::Config) -> anyhow::Result<()>
```

- **`run`:** connects with `sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(&cfg.database_url)`. It **does not** call `knot_storage::connect`, which migrates.
- **`check_all`:**
  1. pages = the singleton workspace's `list_alive` plus `list_templates`;
  2. for each page, `hydrate`, then `walk`, then both writers with `Assets::default()`. Offline mode doesn't resolve images, so they count as `image_unavailable`. Add `walked.unknown_elements.len()` to **each** writer's `Output.fallbacks.unknown_element` before summing;
  3. any `Err`, or a panic caught with `std::panic::catch_unwind(AssertUnwindSafe(..))`, goes into `failed`;
  4. the fallbacks are summed.
- **Output:** `run` prints `checked {pages} pages, {failed} failed` plus one line per fallback kind, and one line per failure.

- [ ] **Step 1: Write the failing test.** `check_all_reports_pages_failures_and_fallbacks`:
  - seed two pages through `PgSnapshotStore::insert(doc_id, 1, &state_bytes, &state_vector)` (pattern `history_integration.rs:287-294`). Build the bytes with yrs directly, since `snapshot_bytes_from_md` can't express the unknown element;
  - one page holds a mermaid block and an unknown `callout` element, written as a hand-built yrs update;
  - assert `pages == 2`, `failed.is_empty()`, `fallbacks.diagram == 2` (one per format), and `fallbacks.unknown_element == 2`;
  - assert the database's migration count is unchanged.
- [ ] **Step 2: Run, expect FAIL.** `cargo nextest run -p knot-server export_check`
- [ ] **Step 3: Implement `export_check.rs`** and wire the subcommand.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cargo nextest run -p knot-server export_check`, then `cargo run -q --bin knot-server -- export-check --help`
  - Expected: pass; the help prints.
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): offline export-check command"`

### Task 15: Web: shared Mermaid loader and diagram rendering

**Files:**
- Create: `web/src/lib/mermaid.ts`, `web/src/features/export/renderDiagrams.ts`, `renderDiagrams.test.ts`
- Modify: `web/src/features/editor/nodes/MermaidCodeBlock.tsx`, which now imports `getMermaid` from `lib/mermaid`

**Interfaces:**
- Produces:
  - `export type MermaidApi` and `export async function getMermaid(): Promise<MermaidApi>`. Moved verbatim, behaviour unchanged. `render` returns `Promise<{ svg: string }>`.
  - Types:

    ```ts
    export type DiagramLimits = { max_body_bytes: number; max_part_bytes: number; max_parts: number; scale: number };
    export type DiagramRef = { key: string; kind: "mermaid"; source: string } | { key: string; kind: "board"; board_id: string; label: string };
    export type RenderDeps = {
      renderMermaid: (source: string) => Promise<string>;          // svg markup
      fetchBoardSvg: (boardId: string) => Promise<string | null>;   // null on any non-200
      rasterize: (svg: string, scale: number, maxSide: number) => Promise<Blob>;  // image/png
    };
    export const defaultRenderDeps: RenderDeps;
    export async function renderDiagrams(diagrams: DiagramRef[], limits: DiagramLimits, deps?: RenderDeps): Promise<Map<string, Blob>>;
    ```

- **`defaultRenderDeps`:**
  - `renderMermaid(source)` returns `(await (await getMermaid()).render(id, source + '\n%%{init: {"htmlLabels": false}}%%')).svg`. The directive goes after the source, and there is no re-initialise.
  - `fetchBoardSvg` uses `boardsApi.getSvg`.
  - `rasterize` draws the SVG through an `Image` from a `blob:` URL onto a canvas at `scale`, clamped so the long side ≤ `maxSide` (2000), then wraps `canvas.toBlob((b) => …, "image/png")` in a Promise that rejects on `null`.
- **`renderDiagrams`:**
  - runs at most 3 at a time, with a 10 s timeout each;
  - a failure skips that diagram;
  - a PNG over `max_part_bytes` is re-rasterized once at half `maxSide`, and dropped if still too big;
  - parts are added in order until the next one would pass `max_body_bytes - 64 KiB` or `max_parts`.

- [ ] **Step 1: Write the failing tests** (Vitest, with fake deps):

```ts
it("renders mermaid and boards into keyed PNG blobs", ...)              // map keys equal input keys
it("skips a diagram whose render throws or times out", ...)             // vi.useFakeTimers; 10 s
it("treats a missing board preview as no picture", ...)                 // fetchBoardSvg → null ⇒ key absent
it("re-rasterizes once at half size when a part is too big, then drops it", ...) // rasterize called with 2000 then 1000
it("stops adding parts at the body and count limits", ...)              // max_parts 2 ⇒ 2 entries
it("never runs more than three renders at once", ...)
```

- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cd web && pnpm test src/features/export/renderDiagrams.test.ts`
  - Expected: FAIL, module not found.
- [ ] **Step 3: Implement `lib/mermaid.ts` and `renderDiagrams.ts`**, and switch `MermaidCodeBlock` to the shared loader.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cd web && pnpm test && pnpm tsc && pnpm lint`
  - Expected: all green, and the existing Mermaid tests are unchanged.
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): render diagrams to PNG in the browser"`

### Task 16: Web: export API client and file helpers

**Files:**
- Create: `web/src/features/export/export.api.ts`, `exportFile.ts`, `export.api.test.ts`, `exportFile.test.ts`
- Modify: `web/src/lib/validators.ts` (valibot `DiagramList` schema)

**Interfaces:**
- Produces:

```ts
export const exportApi = {
  diagrams(docId: string): Promise<ApiResult<{ limits: DiagramLimits; diagrams: DiagramRef[] }>>,   // via apiFetch + parse
  file(docId: string, format: "docx" | "odt", parts: Map<string, Blob>): Promise<ApiResult<{ blob: Blob; fallbacks: Fallbacks }>>,
};
export type Fallbacks = { diagrams: number; images: number };
export function parseFallbacks(header: string | null): Fallbacks;          // "diagrams=2, images=1, unknown=3" → {2,1}; other keys ignored; null/garbage → {0,0}
export function exportFileName(title: string, ext: "docx" | "odt"): string; // Global Constraints rule
export function fallbackMessage(f: Fallbacks, unsynced: boolean): string | null; // exact toast copy; null when nothing to say
export function saveBlob(blob: Blob, fileName: string): void;              // createObjectURL + <a download> + revoke
```

- **`file`:** a hand-rolled `fetch` with a `FormData` body: one field per key, with the blob named `<key>.png`. It sends the `X-CSRF-Token` header from `readCookie("csrf")` and `credentials: "include"`. It parses the error envelope like `blobsApi.upload`. A network error gives `{ code: "network", status: 0 }`.

- [ ] **Step 1: Write the failing tests:**

```ts
it("exportFileName follows the shared rule", () => {
  expect(exportFileName("Q4 / Plan: <draft>?", "docx")).toBe("Q4 - Plan- -draft--.docx");
  expect(exportFileName(" .. ", "odt")).toBe("Untitled.odt");
  expect(exportFileName("x".repeat(200), "docx")).toHaveLength(125);
  expect(exportFileName("Über 😀", "docx")).toBe("Über 😀.docx");
  expect(exportFileName("a".repeat(119) + "😀😀", "docx")).toBe("a".repeat(119) + "😀.docx"); // cap counts code points
});
it("parseFallbacks", ...)                    // "diagrams=2, images=1, unknown=3" → {diagrams:2, images:1}; null → zeros
it("fallbackMessage copy", () => {
  expect(fallbackMessage({diagrams:2, images:1}, false)).toBe("Exported. 2 diagrams and 1 image couldn't be included.");
  expect(fallbackMessage({diagrams:1, images:0}, false)).toBe("Exported. 1 diagram couldn't be included.");
  expect(fallbackMessage({diagrams:0, images:0}, true)).toBe("Exported. Your latest edits weren't saved yet and may be missing.");
  expect(fallbackMessage({diagrams:0, images:0}, false)).toBeNull();
});
it("file() posts multipart with CSRF and returns blob + fallbacks", ...)   // fetch spy: method POST, X-CSRF-Token, FormData has key; headers X-Knot-Export-Fallbacks parsed
it("file() maps the error envelope and network errors", ...)               // 503 export.busy → error.code; reject → code "network"
```

- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cd web && pnpm test src/features/export`
- [ ] **Step 3: Implement the client and helpers** with the signatures above.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cd web && pnpm test && pnpm tsc && pnpm lint`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): export api client and file helpers"`

### Task 17: Web: the `useExport` flow

**Files:**
- Create: `web/src/features/export/useExport.ts`, `useExport.test.ts`

**Interfaces:**
- Consumes: `exportApi`, `renderDiagrams`, `exportFileName`, `fallbackMessage`, `saveBlob`, and `useUi().notify`.
- Produces:
  - `export function useExport(opts: { docId: string; title: string; status: ConnStatus; pendingBytes: number; deps?: Partial<ExportDeps> }): { exporting: boolean; run: (format: "docx" | "odt") => Promise<void> }`
  - `type ExportDeps = { exportApi; renderDiagrams; exportFileName; fallbackMessage; saveBlob; notify }` (the types of those functions), injectable in tests.
- **`run`** is guarded by a ref that is set synchronously (`busyRef.current`). React state would be stale for two calls in the same tick. The ref is mirrored into `exporting` state for the UI. While the ref is set, `run` returns at once. Otherwise:
  1. wait up to 5 s for `status === "connected" && pendingBytes === 0`, mirrored into a ref inside an effect (the React Compiler rules forbid reading a ref during render); `unsynced` is whether that wait timed out;
  2. call `exportApi.diagrams`, then `renderDiagrams`;
  3. call `exportApi.file`;
  4. on error `413`, `408` or `0`, retry `file` once with an empty map;
  5. on success, `saveBlob(blob, exportFileName(title, format))`, and if `fallbackMessage` is non-null, `notify("warn", msg)`;
  6. on `export.busy`, `notify("warn", "Export is busy, try again in a moment.")`;
  7. on any other error, `notify("error", "Couldn't export this page. Try again, or export as Markdown.")`;
  8. finally, clear `exporting`.

- [ ] **Step 1: Write the failing tests** (`renderHook`, fake timers, injected deps):

```ts
it("waits for pending edits, then exports and saves with the sanitized name", ...)
it("exports anyway after 5 s of unsynced edits and appends the warning", ...)   // status "offline" for 5 s
it("retries once without pictures on 413", ...)                                // file called twice; second with empty map
it("shows the busy toast on export.busy", ...)
it("second_click_while_exporting_does_nothing", ...)                            // two run() calls in the same tick → diagrams called once
it("shows the generic error toast for other failures", ...)                     // 500 internal → notify("error", "Couldn't export this page. Try again, or export as Markdown.")
```

- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cd web && pnpm test src/features/export/useExport.test.ts`
- [ ] **Step 3: Implement `useExport`.**
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cd web && pnpm test && pnpm tsc && pnpm lint`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): export flow hook"`

### Task 18: Web: `ExportMenu` in the page header

**Files:**
- Create: `web/src/features/export/ExportMenu.tsx`, `ExportMenu.test.tsx`
- Modify: `web/src/features/docs/DocPage.tsx`. Replace only the `doc-export` `IconButton` (lines 328–343) with `<ExportMenu docId={id} title={meta.title} isTemplate={meta.is_template} status={status} pendingBytes={pendingBytes} />`. Keep the `{!archived && (` guard (lines 327 and 344) and update the comment on line 326.

**Interfaces:**
- Consumes: `useExport`, `IconButton`.
- **Trigger:** `IconButton` with `data-testid="doc-export"`, label "Export", `aria-haspopup="menu"` and `aria-expanded`. It shows the `Download` icon, or `LoaderCircle` with `animate-spin` while exporting.
- **Popover:** `role="menu"`, using the DocByline outside-click pattern (it ignores the trigger, and Escape returns focus to the trigger), right-aligned under the trigger, `max-w-[calc(100vw-2rem)]`.
- **Items** (`role="menuitem"`; ArrowUp/Down, Home/End and Enter/Space work):

  | testid | Label | Sub-label |
  | --- | --- | --- |
  | `export-docx` | "Word document" | ".docx · opens in Word, LibreOffice, Google Docs" |
  | `export-odt` | "OpenDocument text" | ".odt · LibreOffice's own format" |

  Then a separator (`role="separator"`), then, **only when `!isTemplate`**:

  | testid | Label | Action |
  | --- | --- | --- |
  | `export-md` | "Markdown (zip)" | today's anchor download, `descendants=false` |
  | `export-md-subtree` | "Markdown with subpages (zip)" | `descendants=true` |

  The shift-click shortcut is removed.
- Choosing a Word or OpenDocument item closes the menu and calls `run(format)`. The items are disabled while exporting.

- [ ] **Step 1: Write the failing tests:**

```ts
it("opens a menu with Word, OpenDocument and both Markdown items", ...)
it("hides the Markdown items on template pages", ...)
it("supports arrow keys, Home/End and Escape returning focus to the trigger", ...)
it("disables export items and spins the icon while exporting", ...)
it("runs docx export on select", ...)            // useExport mocked via vi.mock
```

  Also, in `DocPage.archived.test.tsx`, the existing assertion that `doc-export` is absent stays green unchanged. Add `it("readers see the export menu")` using the viewer server from `DocPage.byline.test.tsx`.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cd web && pnpm test src/features/export/ExportMenu.test.tsx`
- [ ] **Step 3: Implement `ExportMenu`** and wire it into DocPage.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cd web && pnpm test && pnpm tsc && pnpm lint`
- [ ] **Step 5: Commit.**
  - `git commit -am "feat(export): export menu in the page header"`

### Task 19: e2e: export flow

**Files:**
- Create: `e2e/support/zip.ts`, `e2e/flows/export.spec.ts`

**Interfaces:**
- Produces `export function zipEntries(buf: Buffer): Map<string, Buffer>`. It reads the central directory and inflates `deflate` entries with `zlib.inflateRawSync`, and returns stored entries as they are. No new dependency.

- [ ] **Step 1: Write the failing spec.**
  - **Seed:**
    - sign in (pattern `mobile.spec.ts`);
    - create a page through `POST /api/docs` with body `{"title": "Export test"}` (the Markdown import doesn't set the title);
    - `POST /api/docs/{id}/markdown?mode=replace` with:

      ````markdown
      # Export test

      - [x] done
      - [ ] todo

      | A | B |
      |---|---|
      | 1 | 2 |

      ```mermaid
      graph TD; A-->B
      ```
      ````

  - **DOCX:** open the page, wait for `status-dot` to be connected, click `doc-export` then `export-docx`, and take the file from `page.waitForEvent("download")`. Assert:
    - `zipEntries` has `word/document.xml`, which contains `Export test` and `w:val="Heading1"`;
    - an entry matches `word/media/image1.png`;
    - the download's `suggestedFilename()` is `Export test.docx`.
  - **ODT:** the same for ODT: `content.xml` has `text:outline-level="1"`, and there is a `Pictures/` entry.
- [ ] **Step 2: Run, expect FAIL.**
  - Run: `cd e2e && pnpm playwright test flows/export.spec.ts`
  - Expected: FAIL until the full stack is built. Follow CONTRIBUTING.md; for the local Apple-container stack, Dex must run.
- [ ] **Step 3: Fix anything the e2e surfaces** in the owning task's files.
- [ ] **Step 4: Run, expect PASS.**
  - Run: `cd e2e && pnpm playwright test flows/export.spec.ts flows/mobile.spec.ts`
  - Expected: all pass.
- [ ] **Step 5: Commit.**
  - `git add e2e && git commit -m "test(export): e2e export to docx and odt"`

### Task 20: Whole-branch verification and acceptance files

**Files:** none new.

- [ ] **Step 1: Run every repo check** from Global Constraints. Expected: all green. The only advisory finding allowed is the pre-existing `ctutils` RUSTSEC-2026-0385.
- [ ] **Step 2: Build the acceptance files.**
  - Run `cargo run -p knot-export --example kitchen_sink -- target/export-samples`.
  - Also export a real seeded page through the UI as `.docx` and `.odt`.
  - Attach all four to the PR. The project owner runs the manual acceptance checklist from the spec, including a rotated JPEG phone photo: EXIF orientation is not honoured, so record what the photo looks like.
- [ ] **Step 3: Run the manual checks.**
  - Do one pass of the diagram step in Firefox and Safari, exporting a page with a Mermaid flowchart and a board (spec §8).
  - If a staging copy or restored backup exists, run `knot-server export-check` against it and paste the summary into the PR.
  - Add an SLO follow-up note to the PR, or an SLO line in `docs/SLO.md` if there's a measured target (spec §11).
- [ ] **Step 4: Check the binary size.** Run `cargo build --release --bin knot-server` and compare `ls -l target/release/knot-server` against main. Expected: growth ≤ 3 MB. Note the number in the PR.
