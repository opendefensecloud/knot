# `[[` Page-Link Autocomplete Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Typing `[[` in the editor opens a page picker that inserts an internal link, or creates the page as a subpage of the current one and links it.

**Architecture:** A pure function ranks the client's doc list into candidates with their paths. A new Tiptap extension (`@tiptap/suggestion`, trigger `[[`) shows them in a DOM popup modelled on `MentionExtension` and inserts the existing `knot://doc/<id>` link mark. `KnotEditor` supplies the page list (React Query cache `["docs"]`) and, for editors of the page, a `createSubpage` callback over `docsApi.create`.

**Tech Stack:** React 18, Tiptap 3, `@tiptap/suggestion`, TanStack Query, vitest + jsdom, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-15-page-link-autocomplete-design.md`

## Global Constraints

- No schema change: `tools/schema.json`, `crates/*` stay untouched.
- Links are stored exactly like today's: link mark, `href = "knot://doc/<uuid>"`, text = page title.
- Only pages from `GET /api/docs` (readable, not archived, not templates) are offered.
- UI copy is English like the rest of the app: create row `Create subpage "<query>"`, empty title shown as `Untitled`.
- Test first (Marco's rule): each behaviour's failing test is run, shown to Marco and discussed **before** its implementation. Steps marked **⏸ Marco** are that checkpoint.
- Never commit on `main`; branch `feat/page-link-autocomplete` exists. Conventional commits. PR only, Marco has read access upstream and does not self-merge.

## Review Focus

1. **Titles with spaces** (`[[Urlaub Planung`): the query must keep spaces (`allowSpaces: true`), else the popup closes after the first word. → Task 2 test `keeps spaces in the query`.
2. **`[[` inside code** (code block, inline code): must not open the picker; people paste `a[[0]]` in code. → Task 2 test `does not open inside code`.
3. **Double Enter / double click on the create row** while the request runs must create one page, not two. → Task 2 test `creates only once on repeated picks`.
4. **Create fails** (403, network): `[[query` stays as typed, nothing inserted, error toast shown. → Task 2 test `leaves the text when creation fails`; toast in Task 3.
5. **Popup inside a table cell**: arrows/Enter/Tab must drive the popup, not the table (same issue `MentionExtension` fixed with `priority: 1000`). → Task 2 test `owns arrow keys in a table cell`.

---

## File Structure

| File | Responsibility |
|---|---|
| Create `web/src/features/editor/pageLinkCandidates.ts` | Pure: filter, rank, path, limit |
| Create `web/src/features/editor/pageLinkCandidates.test.ts` | Unit tests for the above |
| Create `web/src/features/editor/PageLinkExtension.ts` | Suggestion plugin, popup, insert/create commands |
| Create `web/src/features/editor/PageLinkExtension.test.ts` | Editor-level tests (jsdom) |
| Modify `web/src/features/editor/InternalLinkExtension.ts:29` | `export` the existing `DOC_HREF_PREFIX` |
| Modify `web/src/features/editor/extensions.ts:331-418` | `pageLinks` option, register extension after `DateTimeExtension` |
| Modify `web/src/features/editor/KnotEditor.tsx` (~`:330-355`) | Pass `listPages`, `currentDocId`, `createSubpage` |
| Create `e2e/flows/page-links.spec.ts` | Browser flow |
| Modify `CHANGELOG.md` (`## [Unreleased]`) | Added entry |

The popup is a second class next to `MentionPopup`, not a shared one: its rows have two lines and a create row; extracting a shared base for two users is not worth the churn (accepted duplication of the ~30 lines of key handling).

---

### Task 1: Candidate ranking

**Files:**
- Create: `web/src/features/editor/pageLinkCandidates.ts`
- Test: `web/src/features/editor/pageLinkCandidates.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export type PageLinkPage = { id: string; title: string; parent_id: string | null };
  export type PageLinkCandidate = { id: string; title: string; path: string[] };
  export const PAGE_LINK_LIMIT = 8;
  export function pageLinkCandidates(
    pages: readonly PageLinkPage[], query: string, currentDocId: string | null, limit?: number,
  ): PageLinkCandidate[];
  ```

Rules: match is case-insensitive substring of the title (query trimmed); titles starting with the query rank before other matches, then `localeCompare` on title; empty query returns all pages in title order; the current page is excluded; `title === ""` becomes `"Untitled"` (also for matching); `path` is the ancestor titles root-first, excluding the page itself, stopping at a missing parent or a cycle.

- [ ] **Step 1: Write the failing tests**

```ts
const P = (id: string, title: string, parent_id: string | null = null) => ({ id, title, parent_id });
const pages = [P("t", "Team"), P("u", "Urlaub", "t"), P("p", "Urlaubsplanung", "u"), P("x", "Mein Urlaub"), P("e", "")];

it("filters case-insensitively and puts prefix matches first", () => {
  expect(pageLinkCandidates(pages, "urlaub", null).map((c) => c.id)).toEqual(["u", "p", "x"]);
});
it("returns all pages in title order for an empty query", () => {
  expect(pageLinkCandidates(pages, "  ", null).map((c) => c.title))
    .toEqual(["Mein Urlaub", "Team", "Untitled", "Urlaub", "Urlaubsplanung"]);
});
it("gives each candidate its ancestor path, root first", () => {
  expect(pageLinkCandidates(pages, "planung", null)[0]?.path).toEqual(["Team", "Urlaub"]);
});
it("leaves out the current page", () => {
  expect(pageLinkCandidates(pages, "urlaub", "u").map((c) => c.id)).toEqual(["p", "x"]);
});
it("caps the list", () => {
  const many = Array.from({ length: 20 }, (_, i) => P(`d${i}`, `Doc ${i}`));
  expect(pageLinkCandidates(many, "doc", null)).toHaveLength(PAGE_LINK_LIMIT);
});
it("survives a parent cycle", () => {
  expect(pageLinkCandidates([P("a", "A", "b"), P("b", "B", "a")], "a", null)[0]?.path).toEqual(["B"]);
});
```

- [ ] **Step 2: Run and confirm they fail** — `cd web && pnpm vitest run src/features/editor/pageLinkCandidates.test.ts` → FAIL, module not found.
- [ ] **Step 3: ⏸ Marco** — show the failing output, explain in plain words what must hold afterwards, wait for OK.
- [ ] **Step 4: Implement `pageLinkCandidates`** per the rules above (build an `id → page` map once; walk parents with a `Set` of visited ids).
- [ ] **Step 5: Run** the same command → PASS.
- [ ] **Step 6: Commit** — `git add web/src/features/editor/pageLinkCandidates*.ts && git commit -m "feat: rank pages for [[ page links"`

---

### Task 2: `PageLinkExtension`

**Files:**
- Create: `web/src/features/editor/PageLinkExtension.ts`
- Test: `web/src/features/editor/PageLinkExtension.test.ts`
- Modify: `web/src/features/editor/InternalLinkExtension.ts:29` (`export const DOC_HREF_PREFIX`)
- Modify: `web/src/features/editor/extensions.ts` (option + registration)

**Interfaces:**
- Consumes: `pageLinkCandidates`, `PageLinkPage`, `PageLinkCandidate` (Task 1); `placeFixedPopup` (`popupPosition.ts`); `DOC_HREF_PREFIX`.
- Produces:
  ```ts
  export type PageLinkItem =
    | { kind: "page"; page: PageLinkCandidate }
    | { kind: "create"; title: string };
  export type PageLinkOptions = {
    listPages: () => Promise<PageLinkPage[]>;
    currentDocId: string | null;
    /** null: the user may not add subpages here; no create row. Resolves null on failure. */
    createSubpage: ((title: string) => Promise<{ id: string; title: string } | null>) | null;
  };
  export const PageLinkExtension: Extension<PageLinkOptions>; // name "knotPageLink", priority 1000
  ```
  `createSchemaExtensions` and `createExtensions` gain `pageLinks?: PageLinkOptions`; default `{ listPages: () => Promise.resolve([]), currentDocId: null, createSubpage: null }`.

Behaviour: `Suggestion({ char: "[[", allowSpaces: true, allow: not in a `code_block` and no `code` mark at the range start })`. Items = candidates for `query`, plus `{kind:"create", title: query.trim()}` last when `createSubpage` is set and the trimmed query is non-empty. Picking a page replaces the range with `[{text: title, marks:[link knot://doc/<id>]}, {text:" "}]` (same shape as `insertMention`). Picking create: ignore if a create is already in flight; await `createSubpage(title)`; on `null` leave the text untouched; on success replace the range if `doc.textBetween(from, to)` still equals `"[[" + query`, else insert at the current selection. Popup: `div.knot-page-link-popup`, `role="listbox"`, rows `data-testid="page-link-item"` (title + small path joined with ` › `) and `data-testid="page-link-create"`; keys as `MentionPopup`.

- [ ] **Step 0: Verify the trigger** — after `make install`, read `findSuggestionMatch` in `web/node_modules/@tiptap/suggestion` and confirm a two-character `char` (`"[["`) is regex-escaped as a whole. If it is not, use `char: "["` with a `findSuggestionMatch` override that requires the preceding `[`, and note it in the PR.
- [ ] **Step 1: Write the failing tests** (mount like `MentionExtension.test.ts`: `createExtensions({ …, pageLinks: { listPages, currentDocId: "cur", createSubpage } })`, `typeText`, one `await` tick, `press`).

```ts
it("opens on [[ and filters by title", …)              // typeText "[[urla" → items text ["Urlaub", "Urlaubsplanung"], create row 'Create subpage "urla"'
it("inserts a doc link with the page title on Enter", …) // getJSON paragraph: text "Urlaub" with link href "knot://doc/u", then " "; no "[[" left
it("keeps spaces in the query", …)                     // "[[Mein Ur" → first item "Mein Urlaub"
it("does not open inside code", …)                     // in a code block and in inline code: popup absent or display none
it("creates a subpage and links it", …)                // createSubpage resolves {id:"n", title:"Neu"}; pick create row for "[[Neu" → called once with "Neu"; link knot://doc/n text "Neu"
it("creates only once on repeated picks", …)           // Enter twice before the promise resolves → createSubpage called 1×
it("leaves the text when creation fails", …)           // createSubpage resolves null → text "[[Neu" still there, no link
it("shows no create row without permission", …)        // createSubpage: null → no [data-testid=page-link-create]
it("owns arrow keys in a table cell", …)               // as the mention table-cell test: ArrowDown moves highlight, caret stays in cell
```

- [ ] **Step 2: Run and confirm they fail** — `cd web && pnpm vitest run src/features/editor/PageLinkExtension.test.ts` → FAIL (module not found / option unknown).
- [ ] **Step 3: ⏸ Marco** — show the failures, talk through each behaviour, wait for OK.
- [ ] **Step 4: Implement** the extension, export `DOC_HREF_PREFIX`, add the option to both factory functions and register `PageLinkExtension.configure(opts.pageLinks ?? default)` right after `DateTimeExtension` in `createSchemaExtensions`.
- [ ] **Step 5: Run** the file → PASS; then `cd web && pnpm vitest run src/features/editor` → PASS (mention, datetime, schema tests unchanged).
- [ ] **Step 6: Commit** — `git commit -m "feat: open a page picker on [[ in the editor"`

---

### Task 3: Wire into `KnotEditor`

**Files:**
- Modify: `web/src/features/editor/KnotEditor.tsx` (the `createExtensions({...})` call, ~`:330-355`)

**Interfaces:**
- Consumes: `PageLinkOptions` (Task 2), `docsApi.list`, `docsApi.create` (`features/docs/docs.api.ts`), `notifyFailure`, `qc`.

Pass:
- `listPages: async () => { const r = await qc.fetchQuery({ queryKey: ["docs"], queryFn: () => docsApi.list(), staleTime: 30_000 }); return "ok" in r ? r.ok.map(({ id, title, parent_id }) => ({ id, title, parent_id })) : []; }`
- `currentDocId: docId`
- `createSubpage: role === "viewer" ? null : async (title) => { const r = await docsApi.create({ title, parent_id: docId }); if ("error" in r) { notifyFailure(notify, r.error, "Couldn't create page"); return null; } await qc.invalidateQueries({ queryKey: ["docs"] }); const d = r.ok as { id: string; title: string }; return { id: d.id, title: d.title }; }`

(The server enforces `require_editable_destination`; the `role` check only hides the row.)

- [ ] **Step 1: Implement the wiring.** No separate unit test: Task 2 covers behaviour, Task 4 covers the wiring end to end.
- [ ] **Step 2: Typecheck and lint** — `set -o pipefail; make lint 2>&1 | tail -20` → exit 0.
- [ ] **Step 3: Commit** — `git commit -m "feat: offer and create pages from [[ in the page editor"`

---

### Task 4: End-to-end flow and changelog

**Files:**
- Create: `e2e/flows/page-links.spec.ts`
- Modify: `CHANGELOG.md` under `## [Unreleased]` → `### Added`

**Interfaces:**
- Consumes: `reset` (`e2e/support/reset.ts`), `setupOwner`, `seedPages` (`e2e/flows/tree-helpers.ts`).

- [ ] **Step 1: Write the failing spec**: seed "Team" › "Urlaub"; open another page in edit mode; type `[[urla`, Enter → editor contains a link to `/doc/<Urlaub id>` with text "Urlaub"; type `[[Reiseplan`, pick `Create subpage "Reiseplan"` → `GET /api/docs` has a "Reiseplan" whose `parent_id` is the current page, the editor links to it, the URL is still the current page.
- [ ] **Step 2: Run it** (Tasks 2–3 have landed, so this guards the wiring rather than driving it): `make compose.up`, then `cd e2e && pnpm playwright test flows/page-links.spec.ts` → PASS.
- [ ] **Step 3: Changelog** entry: "`[[` in the editor opens a page picker: pick a page to link it, or create it as a subpage of the current page."
- [ ] **Step 4: Full suites** — `set -o pipefail; make test 2>&1 | tail -20 && make lint 2>&1 | tail -20` → both exit 0.
- [ ] **Step 5: Commit** — `git commit -m "test: e2e flow for [[ page links"` (changelog in the same commit or `docs: changelog for [[ page links`).

---

### Task 5: Review and PR

- [ ] **Step 1:** Run the `code-reviewer` agent on the branch diff; fix confirmed findings (with tests where behaviour changes).
- [ ] **Step 2:** Run the `sicherheits-review` skill (focus: only readable pages offered, server-side permission on create, no HTML injection — popup rows use `textContent`, never `innerHTML` with titles).
- [ ] **Step 3:** Push and `gh pr create` with description, test results and review results. Do not merge.
