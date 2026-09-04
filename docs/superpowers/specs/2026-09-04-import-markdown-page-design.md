# Import a Markdown file as a page — design

**Date:** 2026-09-04
**Status:** Approved (brainstorm)
**Issue:** [#8](https://github.com/opendefensecloud/knot/issues/8)

## Context

There is no way to get an existing Markdown document into knot. The two things a
user naturally tries are both dead ends:

1. Upload a `.md` file as a page — no such control exists in the UI.
2. Create a page and paste the Markdown source — it lands as literal text
   (`## Heading` stays `## Heading`), because the editor's `handlePaste` only
   intercepts clipboard *files* for attachment upload.

Settings → Backup has "Import zip…", but that restores a whole workspace from an
export; it is not a single-page import.

The backend capability already exists. `POST /api/docs/{id}/markdown`
(`crates/knot-server/src/routes/api/markdown.rs:161`, `import_inline`) is
implemented, registered (`routes/api/docs.rs:58`), role-gated, and has an SLO
entry (`docs/SLO.md:36`). Nothing in `web/src` calls it — the web app only ever
GETs that path, for the read-only markdown panel. So this is a UI gap on top of
a working capability, with one backend semantics fix.

## The merge-vs-replace problem

`import_inline` parses the Markdown into a **fresh** Y.Doc via
`knot_markdown::from_markdown::parse`, then sends the resulting initial-state
bytes to the room as `Event::ApplyUpdate`. Yjs merges that update into the live
doc's `"default"` XmlFragment rather than replacing it, so importing into a
non-empty page appends the imported content to what is already there.

The only current caller (`from_template`, `routes/api/docs.rs:534`) always
targets a freshly-created, empty doc, so this path has never been exercised
against content. Shipping an import button without addressing it would make
"import into a page you already used" silently duplicate content.

`Event::ReplaceWithMarkdown` (`crates/knot-crdt/src/room.rs:67`) already does
the right thing: in a single transaction it clears the fragment and applies the
update, captures what the transaction *actually* produced, and persists and fans
*that* out — so peers replace rather than merge. History restore
(`routes/api/history.rs:173`) and workspace import
(`routes/api/export_import.rs:680`) both use it.

**Decision:** expose both, default to today's behaviour on the wire, and have the
new UI ask for `replace`.

## Design

### 1. Backend — `?mode=` on `POST /api/docs/{id}/markdown`

`import_inline` gains a query parameter:

| `mode` | Event | Notes |
| --- | --- | --- |
| absent or `append` | `ApplyUpdate` | today's behaviour, unchanged |
| `replace` | `ReplaceWithMarkdown` | clear + apply in one transaction |
| anything else | — | `400` with code `markdown.bad_mode` |

The shared prologue is untouched: auth check → `EffectiveDocRole` check →
viewer rejection (`acl.editor_required`) → 1 MB body cap → UTF-8 validation
(`markdown.not_utf8`) → `from_markdown::parse` (`markdown.parse`). Both branches
end by calling `refresh_markdown_and_index`, so the markdown cache and the
`/tasks` index reflect the import either way. Both return `204 No Content`.

Defaulting the *wire* to `append` keeps `from_template` and any existing API
client behaviourally identical; only the new UI opts into `replace`.

### 2. Attribution fix on `ReplaceWithMarkdown`

`ReplaceWithMarkdown` persists with `by_user_id: None`
(`crates/knot-crdt/src/room.rs:355`), so a replace lands in `doc_updates`
unattributed. An import that wipes and rewrites a page is exactly the kind of
change that should name its author.

Add `by_user: Option<Uuid>` to the variant and thread it into the `PersistJob`,
mirroring the `ApplyUpdate` arm. The import handler passes
`Some(ctx.user_id)`. The two existing call sites (history restore, workspace
import) pass `None`, so their behaviour is unchanged — this is a capability
added for the new path, not a change to old ones.

### 3. Web API client

`docsApi.importMarkdown(id, markdown, mode)` in
`web/src/features/docs/docs.api.ts`, over the existing `apiFetch`:

```ts
importMarkdown(id: string, markdown: string, mode: "replace" | "append" = "replace") {
  return apiFetch<void>(
    `/api/docs/${encodeURIComponent(id)}/markdown?mode=${mode}`,
    { method: "POST", body: markdown, contentType: "text/markdown; charset=utf-8" },
  );
}
```

`apiFetch` already attaches `X-CSRF-Token` for unsafe methods, sends
`credentials: "include"`, and unwraps the `{ error: { code, message, … } }`
envelope into `ApiResult`. A `204` yields an empty body, which `apiFetch`
handles (`text.length > 0` guard) and returns as `{ ok: undefined }`.

### 4. The "Import Markdown…" control

A new component `web/src/features/docs/ImportMarkdownButton.tsx`, rendered from
the DocPage header (`web/src/features/docs/DocPage.tsx`) immediately **before**
the existing export button, so import and export sit together. It owns the file
input, the size guard, the emptiness check, the confirm and the POST.

Its own component keeps that logic testable without mounting `DocPage` — which
needs a router, a `QueryClientProvider`, a session and a lazy editor — and keeps
a 250-line page from growing another 60-line inline handler. Props:
`{ docId: string; docTitle: string }`.

The button itself:

- icon `FileUp` (lucide-react), `data-testid="doc-import-md"`,
  label `"Import Markdown…"`
- rendered only for `effRole === "owner" || effRole === "editor"`, matching the
  server's viewer rejection and the existing gating on History / Edit
- a hidden `<input type="file" accept=".md,.markdown,text/markdown,text/plain">`
  with `data-testid="doc-import-md-input"`; its `value` is reset after each pick
  so the same file can be re-imported (same pattern as the toolbar's attachment
  input, `EditorToolbar.tsx:146`)

Flow on file pick:

1. **Size guard.** Reject `file.size > 1 MB` client-side with
   "That file is larger than the 1 MB import limit." The server's cap surfaces
   as a bare `400 bad_request` from `to_bytes`, which is not a usable message.
2. **Read** `await file.text()`.
3. **Emptiness check.** `historyApi.exportMarkdown(id)`; treat a blank/whitespace
   body as empty. On failure to determine, fall through to the confirm (fail
   safe — ask rather than silently overwrite).
4. **Confirm when non-empty.** `window.confirm` — the established idiom here
   (`HistoryDrawer.tsx:105`, `DocTree.tsx:385`, `MembersPage.tsx:164`):
   `Replace the contents of "<title>" with <file name>? The current version stays in History.`
5. **POST** with `mode=replace`.
6. **Feedback.** Success → `notify("info", 'Imported "<file name>"')`. Failure →
   a code-mapped message (`acl.editor_required` → permission, `markdown.parse` /
   `markdown.not_utf8` → unreadable file, otherwise generic).

The open editor needs no explicit refresh: the room actor fans the replace out
over the same WebSocket the editor is already on, which is exactly how history
restore updates a live editor today.

### 5. Paste support

New module `web/src/features/editor/markdownPaste.ts` so the risky logic is a
pure function, unit-testable without an editor:

**`looksLikeMarkdown(text): boolean`** — line-anchored cue detection.

Cue kinds: ATX heading, bullet, ordered item, blockquote, thematic break,
fenced code, GFM table, link, image, emphasis.

Converts when:

- a fenced code block is present, **or**
- a GFM table is present (a pipe row followed by a delimiter row), **or**
- two or more *distinct* cue kinds appear, **or**
- the same block-level cue appears on two or more lines.

The asymmetry is deliberate. A false negative pastes plain text — exactly
today's behaviour, so nothing is lost. A false positive mangles the user's
paste. The heuristic therefore errs toward not firing, and single-line prose
never converts.

**`markdownToHtml(text): string`** — three steps:

1. `marked` (18.x, MIT, GFM enabled) → HTML.
2. Promote task-list items. `marked` emits
   `<li><input type="checkbox" checked disabled> text</li>`, but knot's
   `list_item` reads its state from `data-checked`
   (`TaskListExtension.ts:30`) and has no `input` in its schema — so without
   this step `- [x] done` degrades to a plain bullet and disappears from
   `/tasks`. Rewrite to `<li data-checked="true">text</li>` and drop the input.
3. Sanitize. Markdown passes raw HTML straight through, so this is required,
   not decorative. Add `sanitizeEditorHtml` alongside the existing
   `sanitizeSvg` in `web/src/lib/sanitize.ts`, using DOMPurify restricted to
   the tags and attributes the knot schema can represent, with `data-checked`
   explicitly allowed.

**`handlePaste` order** in `KnotEditor.tsx`, first match wins:

1. clipboard contains files → existing `uploadAndInsert` path (unchanged)
2. clipboard contains `text/html` → return `false`; the source was rich text
   and ProseMirror's own handler is better than round-tripping through Markdown
3. selection is inside a `code_block` or carries the `code` mark → return
   `false` (plain text). Pasting a snippet into a code block must stay verbatim.
4. Shift was held (⌘⇧V / Ctrl+Shift+V, tracked via a `handleDOMEvents.keydown`
   ref) → return `false` (plain text). This is the conventional
   paste-without-formatting escape hatch.
5. `looksLikeMarkdown(text)` → insert `markdownToHtml(text)` via
   `insertContent`, `notify("info", "Pasted as Markdown")`, return `true`
6. otherwise → return `false`, unchanged from today

## Testing

**Rust** — new `crates/knot-server/tests/markdown_import_integration.rs`:

- append into a non-empty doc duplicates content (pins the behaviour the issue
  asked us to confirm, and locks in that `mode=append` stays the default)
- `?mode=replace` into a non-empty doc replaces rather than merges
- `?mode=replace` into an empty doc works (the `len > 0` guard)
- unknown `?mode=` → `400 markdown.bad_mode`
- viewer → `403 acl.editor_required`; unauthenticated → `401`
- invalid UTF-8 body → `422 markdown.not_utf8`
- the replace path records `by_user_id` on the persisted update

**Vitest** — `web/src/features/editor/markdownPaste.test.ts`:

- positive: headings + lists, fenced code alone, GFM table alone, task lists
- negative: one line of prose, `"C# is fine"`, a Python snippet whose comment
  lines start with `#`, a single bare hyphen line
- `markdownToHtml`: task-list promotion to `data-checked`, `<script>` and
  `onerror=` stripped by the sanitizer

`web/src/features/docs/ImportMarkdownButton.test.tsx`: an empty page posts
without prompting; a non-empty page prompts and posts only when the confirm is
accepted; a declined confirm posts nothing; an oversized file is rejected before
any network call.

**Playwright** — `e2e/flows/import-markdown.spec.ts`:

- import a `.md` into an empty page → headings/lists render as real nodes
- import a second file into the now-non-empty page, accept the confirm →
  content is replaced, not duplicated
- paste Markdown source into the editor → real headings and lists appear

## Non-goals

- **Title adoption.** Importing does not change the page title, even when the
  file opens with a single `#` heading. The heading becomes an H1 in the body,
  matching the existing round-trip.
- **Workspace zip import** (Settings → Backup) is untouched.
- **New-page-from-file.** The control imports into the page you are on; it does
  not create a page. Creating a blank page first is one click away.
- **Attachment/image rewriting.** Relative image paths in an imported file are
  imported as-is and will not resolve; out of scope for this change.
