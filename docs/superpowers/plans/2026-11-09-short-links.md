# Short Links Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every page gets a stable short link `https://<host>/<code>` that any reader can copy from the page header and that opens the page after sign-in, with the page's normal ACL.

**Architecture:** A new table `doc_short_links` (code ↔ doc) behind a `ShortLinkStore` in `knot-storage`. Three routes in `knot-server`: get-or-create and read-only get under the existing doc-role middleware, plus a resolve route that does its own ACL check and answers one uniform 404. The SPA gets a "Copy link" button in the doc header and a `/:code` route inside `RequireAuth` that resolves the code and replaces the location with `/doc/<uuid>`.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9, Postgres 18), React 18 + React Router + TanStack Query, Vitest, Playwright with Dex.

**Spec:** `docs/superpowers/specs/2026-11-09-short-links-design.md`

**Base branch:** `feat/short-links`, on top of `fix/oidc-return-to-page` (PR #84). Merges after #84.

## Global Constraints

- Code: exactly 7 characters from `23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ` (56 symbols), at least one digit, random (OS CSPRNG as `share_tokens.rs::fresh_token`), case-sensitive.
- Code pattern (server and client): `^[2-9a-kmnp-zA-HJ-NP-Z]{7}$` **and** contains `[2-9]`.
- One code per page, never changes, never reused; created only on the first "Copy link" of a live (non-archived) page.
- URL is `base_url` with trailing `/` trimmed + `/` + code (as `shares.rs::url_for`).
- Resolve answers **404 `short_link.not_found`** with the same body for: unknown code, malformed code, no read access (restricted page, no grant), and an archived page the caller can't read. Never 403 there.
- Opening requires a session; nothing is resolved for a signed-out request.
- Pages only; no board codes.
- Migrations are forward-only; bound lock waits with `knot_lock_tables` (`migrations/20261107120000_history_log_columns.sql:64`).
- Tests use `knot_test_support::fresh_db()`, never testcontainers.
- Local toolchain differs from CI (Rust 1.99 / Node 26 vs 1.97.1 / 24): run vitest with `NODE_OPTIONS=--no-experimental-webstorage`, clippy with `-A clippy::result_large_err -A deprecated`; `cargo test` instead of nextest if it is missing. e2e needs port 3000 free and `make compose.up`.

## Review Focus

1. **Clipboard unavailable or denied** (insecure context, permission refused): the user still gets the link — a small popover with the URL selected. → Task 3, `CopyLinkButton.test.tsx: falls back to showing the link when the clipboard refuses`.
2. **Two people click "Copy link" on a fresh page at the same moment**: both get the same code, no error. → Task 1, `concurrent_get_or_create_returns_one_code`.
3. **A link pasted with a trailing slash or into a chat that appends punctuation** (`/k3x9Fa7/`): the trailing slash still resolves; anything else that doesn't match the pattern falls through to `/` without an API call. → Task 3, `ShortLinkResolver.test.tsx: resolves with a trailing slash` and `ignores a path that is not a code`.
4. **A page moved into a restricted subtree after its link went out**: a colleague without a grant sees the same "doesn't exist" page as for an unknown code. → Task 2, `resolve_hides_restricted_page_like_unknown_code`.
5. **Top-level paths the server or SPA already owns** (`/login`, `/tasks`, `/favicon.ico`, `/assets/...`): never treated as a code. → Task 1, `generated_codes_never_match_an_app_route`, and Task 3, `ignores a path that is not a code`.

---

### Task 1: Storage — table, code generator, `ShortLinkStore`

**Files:**
- Create: `migrations/20261109120000_doc_short_links.sql`
- Create: `crates/knot-storage/src/short_links.rs`
- Modify: `crates/knot-storage/src/lib.rs` (module + re-exports)
- Test: `crates/knot-storage/tests/short_links.rs` (new), unit tests inside `short_links.rs`

**Interfaces:**
- Produces:
  - `pub const SHORT_CODE_LEN: usize = 7;`
  - `pub fn new_short_code() -> String` (always valid per pattern)
  - `pub fn is_short_code(s: &str) -> bool`
  - `pub struct ShortLink { pub code: String, pub doc_id: Uuid, pub created_by: Option<Uuid>, pub created_at: DateTime<Utc> }`
  - `#[async_trait] pub trait ShortLinkStore: Send + Sync`:
    - `async fn get_or_create(&self, doc_id: Uuid, created_by: Uuid) -> Result<ShortLink, ShortLinkStoreError>`
    - `async fn find_for_doc(&self, doc_id: Uuid) -> Result<Option<ShortLink>, ShortLinkStoreError>`
    - `async fn resolve(&self, code: &str) -> Result<Option<ShortLink>, ShortLinkStoreError>`
  - `pub struct PgShortLinkStore` with `new(pool: PgPool)`; `ShortLinkStoreError { Sqlx(#[from] sqlx::Error) }`
  - Re-exported from `knot_storage`.

- [ ] **Step 1: Write the failing unit tests** in `short_links.rs`:
  - `generated_codes_are_valid`: 10 000 × `new_short_code()` → len 7, every char in the alphabet, contains a digit, `is_short_code` true.
  - `is_short_code_rejects`: `"abcdefg"` (no digit), `"k3x9Fa"` (6), `"k3x9Fa77"` (8), `"k3x9Fl7"` (`l`), `"k3x0Fa7"` (`0`), `"k3x9Fa/"`, `""` → false.
  - `generated_codes_never_match_an_app_route`: for `["login","setup","tasks","members","settings","templates","notifications","archive","about","library-return","api","auth","collab","p","doc","assets","favicon.ico"]`, `is_short_code(name)` is false.
- [ ] **Step 2: Run** `cargo test -p knot-storage --lib short_links` — Expected: FAIL (module missing).
- [ ] **Step 3: Implement** `new_short_code` (rejection-sample bytes from `SysRng` into the 56-symbol alphabet, re-draw the whole code until it contains a digit) and `is_short_code`.
- [ ] **Step 4: Run** the same command — Expected: PASS.
- [ ] **Step 5: Write the failing integration tests** in `crates/knot-storage/tests/short_links.rs` (setup like `tests/boards.rs::setup`):
  - `get_or_create_is_idempotent`: two calls → same `code`; `find_for_doc` and `resolve(code)` return it.
  - `concurrent_get_or_create_returns_one_code`: 8 parallel `get_or_create` on one doc → one distinct code, one row.
  - `code_survives_rename_and_move`: after `DocStore::rename` and `move_to`, `resolve(code)` still gives the doc.
  - `purge_removes_code`: `archive` then `purge` the doc → `resolve(code)` is `None`.
  - `unknown_code_resolves_none`.
- [ ] **Step 6: Run** `cargo test -p knot-storage --test short_links` — Expected: FAIL (table/store missing).
- [ ] **Step 7: Write the migration** `20261109120000_doc_short_links.sql`: `SET LOCAL lock_timeout = '5s';` then `SELECT knot_lock_tables('doc_short_links', ARRAY['documents']::regclass[], 'SHARE ROW EXCLUSIVE');` then `CREATE TABLE doc_short_links` exactly as the spec's Storage block (`code text PRIMARY KEY`, `doc_id uuid NOT NULL UNIQUE REFERENCES documents(id) ON DELETE CASCADE`, `created_by uuid NULL REFERENCES users(id) ON DELETE SET NULL`, `created_at timestamptz NOT NULL DEFAULT now()`), plus `CHECK (code ~ '^[2-9a-kmnp-zA-HJ-NP-Z]{7}$' AND code ~ '[2-9]')`.
- [ ] **Step 8: Implement `PgShortLinkStore`.** `get_or_create`: loop up to 5 times — `INSERT … VALUES (new_short_code(), $doc, $user) ON CONFLICT DO NOTHING`, then `SELECT … WHERE doc_id = $doc`; return the row when present; a row-less result means the code collided, so draw again. After 5 attempts return the sqlx error from a final plain insert (no silent success).
- [ ] **Step 9: Run** Steps 2 and 6 commands plus `cargo test -p knot-storage --test migrations_apply` — Expected: PASS.
- [ ] **Step 10: Commit** `feat(short-links): store short page codes`.

### Task 2: Server — routes, ACL, uniform 404

**Files:**
- Create: `crates/knot-server/src/routes/api/short_links.rs`
- Modify: `crates/knot-server/src/routes/api/mod.rs` (module + `.merge(short_links::router())`), `crates/knot-server/src/routes/api/docs.rs` (add the doc-scoped route inside `doc_id_routes`), `crates/knot-server/src/lib.rs` (`pub short_links: Option<Arc<dyn ShortLinkStore>>` in `AppState`, `None` in the in-memory state, `PgShortLinkStore` in `with_pool` next to `shares`)
- Test: `crates/knot-server/tests/short_links_integration.rs` (scaffolding copied from `tests/shares_integration.rs`: `state_with_seeded`, login helper)

**Interfaces:**
- Consumes: Task 1's `ShortLinkStore`, `is_short_code`.
- Produces (JSON):
  - `POST /api/docs/{id}/short-link` → `200 { "code": String, "url": String }` (live pages; archived → 409 `doc.archived` from `require_doc_role_mw`).
  - `GET  /api/docs/{id}/short-link` → `200 { code, url }` or `404 { error: { code: "short_link.none" } }`; never creates.
  - `GET  /api/short-links/{code}` → `200 { "doc_id": String }` or `404 short_link.not_found`.
  - Handlers `short_links::get_or_create_inline` and `short_links::get_inline` (doc-scoped, mounted in `docs.rs` so they sit behind `require_doc_role_mw`); `short_links::router()` holds only the resolve route.

- [ ] **Step 1: Write the failing integration tests:**
  - `viewer_can_create_and_gets_same_link_twice`: viewer POSTs twice → 200, same `code`, `url == format!("{}/{}", state.base_url.trim_end_matches('/'), code)`.
  - `get_does_not_create`: GET before any POST → 404 `short_link.none`; then POST; GET → same code.
  - `archived_page_keeps_existing_link_and_gets_no_new_one`: editor POSTs, archives the doc; GET → existing code; on a second archived doc without a code, POST → 409 `doc.archived` and GET → 404 `short_link.none`.
  - `resolve_returns_doc_for_a_reader`: resolve → 200 `{doc_id}`.
  - `resolve_hides_restricted_page_like_unknown_code`: owner makes the doc restricted, a second member without a grant resolves its code → 404; body byte-identical to resolving `"Zz9Zz9Z"` (unknown) and `"nope"` (malformed).
  - `resolve_hides_archived_page_from_viewer`: archived doc, viewer → 404 identical body; editor → 200.
  - `resolve_requires_session`: no cookie → 401 `auth.session_required` (from `require_session_mw`).
- [ ] **Step 2: Run** `cargo test -p knot-server --test short_links_integration` — Expected: FAIL (routes 404 / don't compile).
- [ ] **Step 3: Implement the handlers.** `get_or_create_inline` and `get_inline` read `AuthContext` and use `url_for`-style building from `state.base_url`. `resolve(Path(code))`: if `!is_short_code(&code)` → not_found; `store.resolve` → None → not_found; then `acl.access(ctx.workspace_id, link.doc_id, ctx.user_id)` and return not_found unless `Ok(Some(a)) if a.can_read()` (covers restricted, no grant, viewer on archived — `crates/knot-docs/src/acl.rs:41`). Store/ACL errors → 500 `internal` with `tracing::error!`. One `fn not_found() -> Response` builds the single 404.
- [ ] **Step 4: Run** Step 2 command, then `cargo test -p knot-server` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(short-links): create, read and resolve short links`.

### Task 3: Web — "Copy link" button and `/:code` route

**Files:**
- Create: `web/src/lib/shortLinks.api.ts`, `web/src/lib/shortCode.ts`
- Create: `web/src/features/docs/CopyLinkButton.tsx`, `web/src/features/docs/CopyLinkButton.test.tsx`
- Create: `web/src/features/docs/ShortLinkResolver.tsx`, `web/src/features/docs/ShortLinkResolver.test.tsx`
- Modify: `web/src/features/docs/DocPage.tsx` (render `<CopyLinkButton docId={id} archived={archived} />` in the `doc-actions` row before the Share link, for every reader), `web/src/routes.tsx` (route `{ path: ":code", element: <ShortLinkResolver /> }` inside the `RequireAuth` → `AppShell` children, after the fixed routes)

**Interfaces:**
- Consumes: Task 2's JSON contracts.
- Produces:
  - `shortLinksApi.getOrCreate(docId): Promise<ApiResult<{ code: string; url: string }>>` (POST), `shortLinksApi.get(docId)` (GET), `shortLinksApi.resolve(code): Promise<ApiResult<{ doc_id: string }>>` — built on `apiFetch` like `web/src/lib/shares.api.ts`.
  - `isShortCode(s: string): boolean` in `shortCode.ts`, same rule as the server.
  - `CopyLinkButton({ docId, archived }: { docId: string; archived: boolean })`, `data-testid="copy-link"`, label "Copy link", lucide `Link` icon, styled like the Share link in `DocPage.tsx:525-534`.

- [ ] **Step 1: Write the failing vitest tests** (mock `shortLinksApi` and `navigator.clipboard`; render pattern from `web/src/auth/RequireAuth.test.tsx`):
  - `CopyLinkButton.test.tsx`:
    - `copies the short link of a live page`: click → `getOrCreate` called, `clipboard.writeText` called with `url`, toast "Copied!".
    - `copies the existing short link of an archived page`: `archived` → `get` called (not `getOrCreate`), writes its `url`.
    - `copies the long link of an archived page without one`: `get` → 404 → writes `${location.origin}/doc/${docId}`.
    - `falls back to showing the link when the clipboard refuses`: `writeText` rejects → a textbox `data-testid="copy-link-fallback"` with the URL is visible.
  - `ShortLinkResolver.test.tsx` (MemoryRouter with the real route table subset):
    - `replaces the location with the page`: `/k3x9Fa7` → resolve → navigates to `/doc/<id>` with `replace`.
    - `resolves with a trailing slash`: `/k3x9Fa7/` → same.
    - `shows the not-found text on 404`: text "This link doesn't exist or you don't have access to it."
    - `ignores a path that is not a code`: `/abcdefg` and `/favicon.ico` → no `resolve` call, lands on `/`.
- [ ] **Step 2: Run** `cd web && NODE_OPTIONS=--no-experimental-webstorage pnpm vitest run src/features/docs/CopyLinkButton.test.tsx src/features/docs/ShortLinkResolver.test.tsx` — Expected: FAIL (modules missing).
- [ ] **Step 3: Implement** the API client, `isShortCode`, both components, the DocPage and routes wiring. Toast via `useUi((s) => s.notify)("info", "Copied!")` as in `PermissionsDialog.tsx:75,340`.
- [ ] **Step 4: Run** Step 2 command, then the full vitest suite, `pnpm tsc --noEmit`, `pnpm eslint src` — Expected: PASS / clean.
- [ ] **Step 5: Commit** `feat(short-links): copy link button and short link route`.

### Task 4: End-to-end, docs, changelog

**Files:**
- Create: `e2e/flows/short-links.spec.ts`
- Modify: `CHANGELOG.md` (`[Unreleased]` → `### Added` entry plus one `### Upgrading` line: one migration adds `doc_short_links`; it waits at most a few seconds for a lock on `documents` and gives up rather than stalling page writes), `docs/superpowers/README.md` (table row linking spec, plan, outcome)
- Create after merge-ready: `docs/superpowers/research/2026-11-09-short-links-outcome.md`

**Interfaces:**
- Consumes: everything above; `data-testid="copy-link"`.

- [ ] **Step 1: Write the failing e2e spec** (`test.beforeAll(reset)`, `mode: "serial"`, owner setup and Dex login as in `e2e/flows/oidc.spec.ts`; `context.grantPermissions(["clipboard-read", "clipboard-write"])` and read with `navigator.clipboard.readText()`):
  - `a reader copies a short link and gets the same one twice`: owner creates a page; alice (auto-provisioned Viewer via SSO) opens it, clicks `copy-link` twice; both clipboard reads equal and match `^http://localhost:5173/[2-9a-kmnp-zA-HJ-NP-Z]{7}$`.
  - `a signed-out colleague opens a short link and lands on the page after SSO`: fresh context → `goto(<code path>)` → login → Dex → `waitForURL(/doc/<id>)`, `doc-title` visible.
  - `an archived page copies its existing link, and one without a code copies the long link`: owner archives the page from test 1 and a second page that never had a code; on each, `copy-link` → clipboard equals the short link / `http://localhost:5173/doc/<id2>`.
- [ ] **Step 2: Run** `cd e2e && NODE_OPTIONS=--no-experimental-webstorage npx playwright test flows/short-links.spec.ts` — Expected: FAIL, timing out on `getByTestId("copy-link")`. **Write and run Steps 1–2 before Task 1** and show the red run; the spec turns green after Task 3. Run it again here — Expected: PASS.
- [ ] **Step 3: Write** the CHANGELOG entry, the README row, and the outcome doc (status, gates, what was non-obvious, deferred: boards, readable slugs, revocation).
- [ ] **Step 4: Run** the full gate: `make lint` (with the local clippy allowances), `cargo test --workspace --all-features`, full vitest, full Playwright (the two `ws-reconnect` specs need toxiproxy and fail on macOS; say so in the PR).
- [ ] **Step 5: Commit** `test(short-links): e2e for copying and opening short links` and `docs(short-links): changelog, plan index, outcome`.
- [ ] **Step 6: Review gates** before the PR: `code-reviewer` agent over the branch, then `sicherheits-review` (new endpoint, ACL, enumeration). Then push to the `fork` remote and open the PR against `opendefensecloud/knot` `main`, stacked on #84; do not merge.
