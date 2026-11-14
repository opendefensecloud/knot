# Outcome: favorite pages in the sidebar

**Date:** 2026-11-14
**Issue:** none
**Spec:** [specs/2026-11-14-favorites-design.md](../specs/2026-11-14-favorites-design.md)
**Plan:** [plans/2026-11-14-favorites.md](../plans/2026-11-14-favorites.md)
**Goal:** The owner asked for people to be able to star pages in the sidebar, with a Favorites section at the top of the sidebar that lists every starred page and the normal tree underneath. Favorites are per user and stored on the server, in the shape the mobile-app spec planned as K-D1, so the coming app can share them.

## What landed

- **Migration** `20261114120000_user_favorites`. It creates `user_favorites (user_id, doc_id, workspace_id, sort_key, created_at)`, primary key `(user_id, doc_id)`, every FK `ON DELETE CASCADE`, and the index `user_favorites_user_ws_sort (user_id, workspace_id, sort_key COLLATE "C")`. The FKs need `SHARE ROW EXCLUSIVE` on `documents`, `users` and `workspaces`, taken through `knot_lock_tables` under `SET LOCAL lock_timeout = '5s'`. The table is in `migrations_apply`.
- **Storage** (`knot-storage/src/favorites.rs`): `FavoriteStore` and `PgFavoriteStore`, `Favorite`, `AddOutcome { Added, AlreadyThere, LimitReached }`, `FAVORITES_LIMIT = 200`.
  - `list(user, ws, hidden)` joins `documents` for `title` and `icon`, drops archived pages, templates and the `hidden` ids, and orders by `sort_key COLLATE "C"`, then `doc_id`.
  - `add` runs in one transaction under `pg_advisory_xact_lock` on `(user, workspace)`: already starred → `AlreadyThere`; 200 rows in the workspace (hidden ones included) → `LimitReached`; else append after the largest key with `lexorank::between(last, None)`. The lock is what keeps a double click or two tabs from writing two rows, two equal keys, or a 201st row.
  - `remove` deletes only the caller's row in that workspace and returns whether one went.
- **Server** (`routes/api/favorites.rs`), wired as `AppState.favorites: Option<Arc<dyn FavoriteStore>>`.
  - `GET /api/me/favorites` → `[{doc_id, title, icon, favorited_at}]`. It follows the notifications pattern: SQL deny-list from `visibility().hidden_ids()`, then a `visibility_covering` + `readable()` re-check of the rows returned.
  - `PUT /api/me/favorites/{doc_id}` → 204. It mirrors `require_doc_role_mw`: no access or unknown → `403 acl.no_grant`, `!can_read()` (a Viewer on an archived page) → `404 doc.not_found`, archived → `409 doc.archived` via `doc_archived()`. Then the doc is read for `is_template` → `422 favorite.not_a_page` (a page purged in between reads as `acl.no_grant`), then `add` → `409 favorite.limit_reached` with `details.limit = 200`.
  - `DELETE /api/me/favorites/{doc_id}` → 204, no access check.
  - API-key scopes are the method-derived ones (`read`, `update`, `delete`): no `OVERRIDES` entry, not `SESSION_ONLY`. PUT and DELETE fall in the default write rate-limit bucket. No audit row, no new metric.
- **Web.**
  - `favorites.api.ts`, a valibot `Favorite` schema in `lib/validators.ts`, query key `["favorites"]` with a 30 s stale time.
  - `useFavorites()` gives `favorites`, `favoriteIds`, `ready`, `isFavorite` and an optimistic `toggle`. Stars and unstars share one mutation scope, so they reach the server in order. A failure rolls the list back; `favorite.limit_reached` shows "You have 200 favorites, the most allowed. Remove one to add another." and other errors go through `notifyFailure`. `toggle` does nothing before the list has loaded.
  - `FavoritesSection` sits between `WorkspaceHeader` and the "Documents" header, outside the tree's `DndContext`, and doesn't reuse `TreeRow`. Its header is styled like "Documents", with a collapse chevron saved in `localStorage` `knot.favoritesCollapsed` (every access in try/catch). The section is hidden while no favorite is visible. Titles and icons come from the `["docs"]` list once it has loaded, so renames, archives and access changes show with the tree's next refresh; ids missing from that list, or shown there as archived or templates, are skipped. Before it loads, the API's own titles are used. Each row links to `/doc/{id}`, is highlighted while open, and has an unstar button shown on hover and keyboard focus; unstarring the open page leaves you on it.
  - `TreeRow` has a star `IconButton` (`tree-star-{id}`) for every role, `aria-pressed`, labelled "Add to favorites" / "Remove from favorites". Unstarred, it shows on hover and `group-focus-within` / `focus-visible`; starred, it is filled and always shown. It is disabled until the favorites list has loaded. The context menu's first item is `ctx-favorite` for every role, which gives Viewers a row menu. Templates get neither.
  - Test ids as in the spec: `favorites`, `favorites-toggle`, `fav-row-{id}`, `fav-link-{id}`, `fav-unstar-{id}`, `tree-star-{id}`, `ctx-favorite`.
- **Docs.** CHANGELOG `[Unreleased]` (Upgrading, Added), knot-api skill rows and a gotcha, this note and the README row.

## Tests

- Storage (`crates/knot-storage/tests/favorites.rs`): star order, idempotent add and remove, `remove_only_touches_the_callers_row`, per user and workspace, hidden / archived / template filtering, purge cascade, the cap, and the two concurrency tests from the plan's Review Focus (`concurrent_adds_of_same_doc_make_one_row`, `concurrent_adds_get_distinct_sort_keys`).
- Server (`crates/knot-server/tests/favorites_integration.rs`): round trip, idempotent PUT, Viewer can star, restricted page hidden and refused, back after unrestricting, archived hidden and back on restore (with the `404` for a Viewer), unknown and other-workspace ids, template, cap, API-key scopes, no audit rows.
- Web: `useFavorites.test.tsx` (order, `ready`, optimistic star and unstar, rollback both ways, toggles one at a time, nothing before load), `FavoritesSection.test.tsx` (order, API titles before the docs list, hidden when empty, titles follow the docs list, archived and templates skipped, collapse persists and survives a throwing `localStorage`, unstar keeps the route, selected style, hover and focus reveal), `DocTree.favorites.test.tsx` (Viewer star and menu item, menu order, star from the tree and unstar from the menu, keyboard reveal, disabled while loading, no star on templates, no duplicate ids, section above "Documents"). `DocTree.keyboard.test.tsx` was adjusted for the new button, and `DocTree.archive.test.tsx` now says Viewers get no More button and no archive item (they do get a row menu, for favorites).
- e2e (plan Task 5): `e2e/flows/favorites.spec.ts` (star from the tree, open it from Favorites, order and reload, unstar, the menu label) and `user_favorites` in `e2e/support/reset.ts` TABLES. It was not on the branch yet when this note was written.

## Deviations from the spec and plan

- **Migration created by hand.** The plan said `make migrate.create`; the file was written directly with the timestamp agreed with the page-history work running alongside, so the two branches' migrations don't collide.
- **Sort key helper.** The spec names `lexorank::sort_key_between`; the crate's function is `lexorank::between`.
- **Rollback.** The plan's rollback was `DROP TABLE user_favorites`. That alone isn't enough: a 0.9.13 binary refuses to start on a database whose `_sqlx_migrations` lists a migration it doesn't know. The CHANGELOG gives both statements, the drop and `DELETE FROM _sqlx_migrations WHERE version = 20261114120000`, in one transaction.
- **Extras the spec left to the builder.** The storage test `remove_only_touches_the_callers_row`; the web hook's limit toast; the section also skips favorites the docs list shows as archived or templates (the docs list can be fresher than the favorites query); no star or menu item on templates, since the server refuses them.

## Deferred

- **Drag to reorder** favorites. `sort_key` is already stored, so it needs no migration: a `PUT` with `after_id` / `before_id`, as K-D1 plans.
- **A star in the page header**, so a page can be starred without finding it in the tree.
- **Recents**, and favorites in search or the command palette.
- Favorites that expand into their subpages.
- Live push of other people's changes: the list is as fresh as the tree (own mutations, 30 s stale time).

## Carryforward

- The mobile app (K-D1) can use `/api/me/favorites` as it is; its reorder call is the one deferred above.
- The Helm chart README has no "Upgrading from 0.9.13" section yet; write it at release time from the CHANGELOG's Upgrading entry.
