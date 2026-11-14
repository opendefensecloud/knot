# Favorite pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Users star pages in the sidebar tree; a Favorites section at the top of the sidebar lists them (oldest star first), stored per user on the server.

**Architecture:** New `user_favorites` table + `FavoriteStore` in knot-storage; `/api/me/favorites` routes in knot-server, ACL-filtered on read like notifications; web `useFavorites` hook, a `FavoritesSection` above the "Documents" header (outside the tree's `DndContext`), and a star button + context-menu item on `TreeRow`.

**Tech Stack:** Rust (axum, sqlx, Postgres 18), React + TanStack Query + valibot + Tailwind + lucide-react, vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-14-favorites-design.md`

## Global Constraints

- Per-user cap: **200** favorites per (user, workspace), counting rows currently hidden. Over the cap: `409 favorite.limit_reached` with `details.limit = 200`.
- PUT access errors mirror `require_doc_role_mw` exactly: no access / unknown doc → `403 acl.no_grant`; `!can_read()` → `404 doc.not_found`; archived → `409 doc.archived` (use the same helper `doc_archived()` emits); template → `422 favorite.not_a_page`.
- DELETE never checks ACL; only touches the caller's own row; idempotent 204.
- GET never returns a doc the caller can't read, an archived doc, or a template. Order: `sort_key COLLATE "C"`, then `doc_id`.
- No audit rows, no new metric, no scope `OVERRIDES`, not `SESSION_ONLY`.
- Web test ids: `favorites`, `favorites-toggle`, `fav-row-{id}`, `fav-link-{id}`, `fav-unstar-{id}`, `tree-star-{id}`, `ctx-favorite`. Never reuse `doc-row-*`, `tree-row-*`, `tree-desc-*`.
- Copy: section header `Favorites`; button/menu labels `Add to favorites` / `Remove from favorites`.
- localStorage key `knot.favoritesCollapsed` (`"1"` collapsed), every access in try/catch.

## Review Focus

1. **Double-click / two tabs starring the same page at once** → exactly one row, both requests 204, no duplicate sort keys. Pinned in Task 1 (`concurrent_adds_of_same_doc_make_one_row`) and Task 1 (`concurrent_adds_get_distinct_sort_keys`).
2. **Keyboard users** must be able to reach and toggle the star: it must become visible on `focus-within`/`focus-visible`, not only on hover. Pinned in Task 4 (`star button is revealed on keyboard focus`).
3. **Renaming a starred page** must update its Favorites entry once the tree refreshes, without a separate refetch. Pinned in Task 4 (`favorite title follows the docs list`).
4. **Unstarring the page you're currently on from the Favorites section** removes it immediately and leaves you on the page (no navigation). Pinned in Task 4 (`unstar from favorites keeps the current route`).
5. **Star clicked before favorites have loaded, or the request fails** → the star must not lie: disabled until the favorites query has data; on error it rolls back. Pinned in Task 3 (`star rolls back on server error`) and Task 4 (`star is disabled while favorites load`).

---

### Task 1: Storage — migration and `FavoriteStore`

**Files:**
- Create: `migrations/<ts>_user_favorites.sql` (via `make migrate.create NAME=user_favorites`)
- Create: `crates/knot-storage/src/favorites.rs`
- Modify: `crates/knot-storage/src/lib.rs` (`pub mod favorites;` + `pub use`)
- Test: `crates/knot-storage/tests/favorites.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Favorite { pub doc_id: Uuid, pub title: String, pub icon: Option<String>, pub sort_key: String, pub created_at: DateTime<Utc> }
  pub enum AddOutcome { Added, AlreadyThere, LimitReached }
  pub const FAVORITES_LIMIT: i64 = 200;
  #[async_trait] pub trait FavoriteStore: Send + Sync + 'static {
      /// Live (not archived, not template) favorites of `user_id` in `workspace_id`, minus `hidden`,
      /// ordered by sort_key COLLATE "C", doc_id. Joins documents for title/icon.
      async fn list(&self, user_id: Uuid, workspace_id: Uuid, hidden: &[Uuid]) -> Result<Vec<Favorite>>;
      async fn add(&self, user_id: Uuid, workspace_id: Uuid, doc_id: Uuid) -> Result<AddOutcome>;
      /// true if a row was deleted.
      async fn remove(&self, user_id: Uuid, workspace_id: Uuid, doc_id: Uuid) -> Result<bool>;
  }
  pub struct PgFavoriteStore { .. } impl PgFavoriteStore { pub fn new(pool: PgPool) -> Self }
  pub enum FavoriteStoreError { Sqlx(#[from] sqlx::Error), Key(..) }  pub type Result<T> = ...
  ```
  `icon` type follows whatever `documents.icon` maps to in `DocStore` (check `Doc`).

- [ ] **Step 1: Migration.** Header comment in the `doc_short_links` style (purpose, cascade semantics, why locks). `SET LOCAL lock_timeout = '5s'; SELECT knot_lock_tables('user_favorites', ARRAY['documents','users','workspaces']::regclass[], 'SHARE ROW EXCLUSIVE');` then the table and index exactly as in the spec's Storage section. Run `make migrate.up` → applies cleanly.
- [ ] **Step 2: Write failing tests** in `crates/knot-storage/tests/favorites.rs` with a `setup()` modeled on `tests/notifications.rs:11-48` (workspace, alice owner, bob editor, docs created via `PgDocStore::create`):
  - `add_appends_in_star_order`: add d2 then d1 → `list` ids `[d2, d1]`, both `Added`.
  - `add_is_idempotent`: add d1 twice → second `AlreadyThere`, one row, sort_key unchanged.
  - `remove_is_idempotent`: remove → `true`, again → `false`; removing a never-starred doc → `false`.
  - `list_is_per_user_and_workspace`: bob's list doesn't contain alice's; a second workspace's list is empty.
  - `list_skips_hidden_archived_and_templates`: `hidden=[d1]` drops d1; archiving d2 (`DocStore` archive fn) drops it; restoring brings it back; a template doc never appears.
  - `purge_cascades`: after the store's purge/delete of d1, the row is gone (count via `sqlx::query_scalar`).
  - `limit_reached_at_200`: insert 200 → 201st returns `LimitReached`; re-adding an existing one still `AlreadyThere`.
  - `concurrent_adds_of_same_doc_make_one_row`: `tokio::join!` two `add(d1)` → one `Added` + one `AlreadyThere`, one row.
  - `concurrent_adds_get_distinct_sort_keys`: `join!` add(d1), add(d2) → two rows, distinct sort keys.
- [ ] **Step 3: Run** `cargo test -p knot-storage --test favorites` → FAIL (module missing).
- [ ] **Step 4: Implement `favorites.rs`** following `notifications.rs` conventions. `add` runs in one transaction: `SELECT pg_advisory_xact_lock(hashtextextended('user_favorites:' || $user || ':' || $ws, 0))`; if row exists → `AlreadyThere`; count rows for (user, ws) ≥ `FAVORITES_LIMIT` → `LimitReached`; else last key = `MAX(sort_key COLLATE "C")`, new key = `lexorank::between(last, None)`, insert. `list` filters `NOT (f.doc_id = ANY($hidden))`, `d.archived_at IS NULL AND NOT d.is_template`, `d.workspace_id = $ws`.
- [ ] **Step 5: Run** `cargo test -p knot-storage --test favorites` → PASS; `cargo clippy -p knot-storage --all-targets -- -D warnings` clean.
- [ ] **Step 6: Commit** `feat(favorites): user_favorites table and FavoriteStore`.

### Task 2: Server — `/api/me/favorites`

**Files:**
- Create: `crates/knot-server/src/routes/api/favorites.rs`
- Modify: `crates/knot-server/src/routes/api/mod.rs` (merge router), `crates/knot-server/src/lib.rs` (`favorites: Option<Arc<dyn FavoriteStore>>`: default `None` near :159, build + set near :217/:244)
- Test: `crates/knot-server/tests/favorites_integration.rs`

**Interfaces:**
- Consumes: Task 1 `FavoriteStore`, `AddOutcome`, `FAVORITES_LIMIT`; `AclCache::access`, `visibility`, `visibility_covering`, `Visibility::readable/hidden_ids` (`crates/knot-docs/src/cache.rs`).
- Produces (HTTP, consumed by Task 3):
  - `GET /api/me/favorites` → `200 [{ "doc_id": uuid, "title": string, "icon": string|null, "favorited_at": rfc3339 }]`
  - `PUT /api/me/favorites/{doc_id}` → `204` (see Global Constraints for errors)
  - `DELETE /api/me/favorites/{doc_id}` → `204`

- [ ] **Step 1: Write failing integration tests** modeled on `tests/notifications_integration.rs` / `tests/whats_new_integration.rs:24-80` (real `/auth/login`, cookies + csrf, `router_with_state(..).oneshot`). Reuse that file's helpers for creating a second member, restricting a page, archiving, restoring:
  - `star_list_unstar_round_trip`: PUT a, PUT b → GET returns `[a, b]` with titles; DELETE a → `[b]`.
  - `put_is_idempotent`: PUT a twice → both 204, GET length 1.
  - `viewer_can_star`.
  - `restricted_page_is_hidden_and_refused`: owner stars X, restricts X without granting viewer; viewer had starred X before restriction → viewer GET omits X; viewer PUT X → `403 acl.no_grant`; viewer DELETE X → 204.
  - `archived_hidden_then_restored`: editor stars X, X archived → GET omits X, PUT X → `409 doc.archived`, DELETE works; after restore of a still-starred X, X is back.
  - `unknown_doc_is_no_grant`: PUT random uuid → `403 acl.no_grant`.
  - `template_refused`: PUT a template → `422 favorite.not_a_page`.
  - `limit_reached`: seed 200 rows through `PgFavoriteStore` directly, PUT another → `409 favorite.limit_reached`, `details.limit == 200`.
  - `api_key_scopes`: a key with only `read` can GET, gets `403 api_key.scope_denied` on PUT; a key with `update` can PUT; `delete` needed for DELETE.
  - `no_audit_rows`: after star/unstar, `audit_events` has no row with `action LIKE 'favorite%'` and the page audit log is unchanged.
- [ ] **Step 2: Run** `cargo test -p knot-server --test favorites_integration` → FAIL (404s).
- [ ] **Step 3: Implement `favorites.rs`** with a `//!` header listing the three endpoints (style of `notifications.rs:1-11`). PUT: `acl.access(ws, doc, user)` → map `None`→403, `!can_read()`→404, `archived`→409 `doc.archived`; then look up the doc's `is_template` (via the doc store) → 422; then `add` → `LimitReached` → `json_err_with_details(CONFLICT, "favorite.limit_reached", "", json!({"limit": FAVORITES_LIMIT}))`. GET: `visibility(ws,user).hidden_ids()` → `list` → `visibility_covering(ws,user,&ids)` re-check with `readable()`. Errors logged `tracing::error!(error=?e, "favorites <op>")`.
- [ ] **Step 4: Run** the integration tests → PASS. Also `cargo test -p knot-server --lib auth::scope` still passes (no override added).
- [ ] **Step 5: Commit** `feat(favorites): /api/me/favorites endpoints`.

### Task 3: Web data layer — API client and `useFavorites`

**Files:**
- Modify: `web/src/lib/validators.ts` (add `Favorite` schema)
- Create: `web/src/features/favorites/favorites.api.ts`, `web/src/features/favorites/useFavorites.ts`
- Test: `web/src/features/favorites/useFavorites.test.tsx`

**Interfaces:**
- Consumes: Task 2 HTTP contract; existing `apiFetch` wrapper used by `features/docs/docs.api.ts`.
- Produces:
  ```ts
  export const Favorite = v.object({ doc_id: v.string(), title: v.string(), icon: v.nullable(v.string()), favorited_at: v.string() });
  export const favoritesApi: { list(): Promise<Favorite[]>; star(docId: string): Promise<void>; unstar(docId: string): Promise<void> };
  export const FAVORITES_KEY = ["favorites"] as const;
  export function useFavorites(): {
    favorites: Favorite[] | undefined;      // server order
    favoriteIds: ReadonlySet<string>;
    ready: boolean;                          // query has data
    isFavorite(id: string): boolean;
    toggle(doc: { id: string; title: string; icon: string | null }): void; // optimistic
  };
  ```

- [ ] **Step 1: Write failing tests** (render hook in `QueryClientProvider` with `retry:false`, `vi.stubGlobal("fetch", ...)` like `DocTree.collapse.test.tsx`):
  - `lists favorites in server order`.
  - `toggle stars optimistically and appends`: before the PUT resolves, `isFavorite(id)` is true and it is last.
  - `toggle unstars optimistically`.
  - `star rolls back on server error`: PUT → 409 → `isFavorite(id)` false again, list restored.
  - `ready is false until the first response`.
- [ ] **Step 2: Run** `cd web && npx vitest run src/features/favorites` → FAIL.
- [ ] **Step 3: Implement.** Mutations use `onMutate` (cancel + snapshot + `setQueryData`), `onError` (restore snapshot), `onSettled` (invalidate `FAVORITES_KEY`). Mutation `scope: { id: "favorites" }` so toggles serialize.
- [ ] **Step 4: Run** tests → PASS; `npx tsc --noEmit` clean.
- [ ] **Step 5: Commit** `feat(favorites): web API client and useFavorites hook`.

### Task 4: Web UI — Favorites section, tree star, context menu

**Files:**
- Create: `web/src/features/favorites/FavoritesSection.tsx`
- Modify: `web/src/features/docs/DocTree.tsx` (render `<FavoritesSection docs={docs} />` between `<WorkspaceHeader/>` and the "Documents" header, ~:450, outside `DndContext`), `web/src/features/docs/TreeRow.tsx` (star button + `ctx-favorite` item for every role)
- Test: `web/src/features/favorites/FavoritesSection.test.tsx`, `web/src/features/docs/DocTree.favorites.test.tsx`

**Interfaces:**
- Consumes: Task 3 `useFavorites`; the `["docs"]` list already in `DocTree`.
- Produces: `export function FavoritesSection(props: { docs: Doc[] | undefined }): JSX.Element | null`.

- [ ] **Step 1: Write failing tests.**
  `FavoritesSection.test.tsx`:
  - `renders favorites in order with fav-link testids`, links to `/doc/{id}`.
  - `hidden when there are no visible favorites` (`queryByTestId("favorites")` null), including when every favorite id is missing from `docs`.
  - `favorite title follows the docs list`: favorites API says "Old", docs list says "New" → shows "New"; a favorite absent from `docs` is skipped.
  - `collapse persists`: click `favorites-toggle` → rows hidden, `localStorage["knot.favoritesCollapsed"] === "1"`; remount → still collapsed; localStorage throwing doesn't crash.
  - `unstar from favorites keeps the current route`: in a `MemoryRouter` at `/doc/a`, click `fav-unstar-a` → row gone, location still `/doc/a`.
  - `open favorite has selected style` (`bg-muted` on `fav-row-{id}` when route is `/doc/{id}`).
  `DocTree.favorites.test.tsx` (fixtures from `DocTree.collapse.test.tsx`, plus a `/api/me/favorites` fake):
  - `viewer sees star and menu item`: as a Viewer, `tree-star-{id}` exists and the context menu shows `ctx-favorite` (and nothing editor-only).
  - `starring from the tree adds to favorites`: click `tree-star-{id}` → `fav-row-{id}` appears; star has `aria-pressed="true"` and label `Remove from favorites`.
  - `star button is revealed on keyboard focus`: its class list includes `focus-visible:opacity-100` and the row's reveal includes `group-focus-within:opacity-100`; a starred row's star has `opacity-100` always.
  - `star is disabled while favorites load`: favorites fetch pending → `tree-star-{id}` is `disabled`.
  - `no duplicate ids`: with a starred doc, `document.querySelectorAll('[data-testid="doc-row-ID"]').length === 1` and no element id appears twice.
- [ ] **Step 2: Run** `cd web && npx vitest run src/features/favorites src/features/docs` → new tests FAIL, existing pass.
- [ ] **Step 3: Implement.** Section header copies the "Documents" header style (`text-[11px] font-semibold uppercase tracking-wider text-fg-muted`), `<h3>` + chevron button `favorites-toggle` with `aria-expanded`. Rows: `<li data-testid="fav-row-{id}">` with `group`, `FileText` (or the doc icon if the tree shows one) + truncated title `Link` (`fav-link-{id}`), and an `IconButton` (`fav-unstar-{id}`, `Star` filled, label `Remove from favorites`, revealed on hover/focus-within). TreeRow: `IconButton` `tree-star-{id}` placed before the More button, rendered for all roles, `active={isFavorite}`, `disabled={!ready}`; context-menu item `ctx-favorite` first in the list for every role.
- [ ] **Step 4: Run** `cd web && npx vitest run && npx tsc --noEmit && npm run lint` → all PASS.
- [ ] **Step 5: Commit** `feat(favorites): sidebar Favorites section and tree star`.

### Task 5: e2e flow

**Files:**
- Create: `e2e/flows/favorites.spec.ts`
- Modify: `e2e/support/reset.ts` (add `user_favorites` to TABLES)

- [ ] **Step 1: Write the flow** using `setupOwner`, `seedPages` from `e2e/flows/tree-helpers.ts`: seed "Alpha", "Beta"; hover `tree-row-{beta}` → click `tree-star-{beta}` → `fav-link-{beta}` visible; click it → URL `/doc/{beta}`; star Alpha → favorites order Beta, Alpha; reload → still both in that order; unstar Beta via `fav-unstar-{beta}` → gone; right-click `doc-row-{alpha}` → `ctx-favorite` reads "Remove from favorites".
- [ ] **Step 2: Run** per memory's e2e recipe (`make e2e` or the Docker per-run-DB recipe) → PASS.
- [ ] **Step 3: Commit** `test(favorites): e2e flow`.

### Task 6: Docs and changelog

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]`), `.claude/skills/knot-api/SKILL.md` (endpoint table ~:89-120, gotchas ~:169-192), `docs/superpowers/README.md` (row)
- Create: `docs/superpowers/research/2026-11-14-favorites-outcome.md`

- [ ] **Step 1:** CHANGELOG — `### Upgrading`: migration name, takes SHARE ROW EXCLUSIVE on documents/users/workspaces briefly (5 s lock timeout), rollback = `DROP TABLE user_favorites`. `### Added`: user-facing bullet ("Star pages in the sidebar…"), one bullet per endpoint with scope.
- [ ] **Step 2:** knot-api skill — three rows; gotchas `favorite.limit_reached`, `favorite.not_a_page`, DELETE has no ACL check.
- [ ] **Step 3:** Outcome note (what landed, deferred: drag reorder, page-header star, recents) + README row in the existing table format.
- [ ] **Step 4: Run** `make lint && make test` → PASS.
- [ ] **Step 5: Commit** `docs(favorites): changelog, API reference, outcome`.
