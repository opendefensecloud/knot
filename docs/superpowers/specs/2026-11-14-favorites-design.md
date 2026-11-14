# Favorite pages — design

**Date:** 2026-11-14
**Status:** Approved by the owner (2026-11-14)

## Context

People keep returning to the same handful of pages. Today the only way to
get to them is to find them in the tree (expanding parents) or search. The
owner wants people to be able to **star pages in the sidebar** and have a
**Favorites section at the top of the sidebar** that lists every starred
page, with the normal tree underneath. Goal: one click to the pages you use
most.

The draft mobile-app spec already plans this as K-D1
(`2026-10-22-mobile-app-design.md:844`): a server-side `user_favorites`
table, `/api/me/favorites` endpoints, ACL-filtered, hidden while archived,
cascade on purge, limit 200, shared between web and the future app. This
design builds the server half to that shape and the web sidebar half, and
leaves drag-reorder for later.

## Goals

- Every user, **Viewers included**, can star and unstar any page they can
  read, from the sidebar tree.
- A **Favorites** section sits between the workspace header (Search, Inbox,
  …) and the "Documents" header. It lists starred pages, oldest star first.
- Favorites are **per user, stored on the server**, so they follow the user
  across browsers and devices (and later the mobile app).
- A favorite the user can no longer read (restricted, grant removed,
  archived for a viewer) silently disappears from the list, and comes back
  when access returns. Archived pages are hidden for everyone while
  archived. Purging a page deletes its favorite rows.

## Non-goals (this change)

- **Drag to reorder** favorites. Storage carries a `sort_key` so it can be
  added without a migration (`PUT` with `after_id`/`before_id`, as K-D1).
- A star in the page header, recents, favorites in search or the command
  palette.
- Favorites that expand into their subpages. A favorite is a plain link;
  the tree below is for browsing.
- Live push of other users' changes. The sidebar has no push today; the
  favorites list is as fresh as the tree (refetch on own mutations, 30 s
  stale time).

## Anti-goals

- Leaking anything about pages the user can't read: no titles, no ids, no
  counts in responses, no difference between "hidden" and "doesn't exist".
- Telling other people who starred what. A star is a private preference:
  **no audit row**, nothing in the page audit log.
- Breaking the existing tree: no duplicate DOM ids or test ids, no second
  set of dnd-kit draggables inside the tree's `DndContext`.

## Approaches considered

1. **Server table + `/api/me/favorites` (chosen).** Follows the user across
   devices, matches the mobile plan, ACL-filtered on read like
   notifications.
2. **Browser localStorage.** Smallest, but per-browser only, invisible to
   the API and the coming app. Rejected.
3. **`favorite: bool` on `GET /api/docs`.** Saves one request, but gives
   no ordering, nothing for API/mobile consumers, and couples a private
   preference to the tree payload. Rejected as the primary API; the web
   instead joins the favorites ids against the already-loaded docs list
   (below).

Ordering: **order of starring** (chosen by the owner) now, drag-reorder
later on the same `sort_key`.

## Design

### Storage

Migration `YYYYMMDDHHMMSS_user_favorites.sql`:

```sql
CREATE TABLE user_favorites (
    user_id      uuid NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    doc_id       uuid NOT NULL REFERENCES documents(id)  ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sort_key     text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, doc_id)
);
CREATE INDEX user_favorites_user_ws_sort
    ON user_favorites (user_id, workspace_id, sort_key COLLATE "C");
-- The documents cascade deletes WHERE doc_id = ? once per purged page.
CREATE INDEX user_favorites_doc ON user_favorites (doc_id);
```

- FKs to `documents`/`users` take their locks through `knot_lock_tables`
  like `doc_short_links`.
- `workspace_id` is stored (notifications precedent) so reads filter by
  `ctx.workspace_id` without a join.
- `sort_key` uses `lexorank::sort_key_between` (append = between the
  current last key and nothing).
- No cleanup on member removal: an ex-member has no session in the
  workspace, and rows are filtered by readability anyway. Purge cascades.

Store: `crates/knot-storage/src/favorites.rs` — `FavoriteStore` trait +
`PgFavoriteStore`, wired into `AppState` as `Option<Arc<dyn FavoriteStore>>`.
Operations: `list(user, ws) -> Vec<(doc_id, sort_key, created_at)>` ordered
by `sort_key COLLATE "C"`; `add(user, ws, doc) -> Added | AlreadyThere |
LimitReached` (append, idempotent, cap enforced in the same transaction);
`remove(user, ws, doc)` idempotent.

### API (`crates/knot-server/src/routes/api/favorites.rs`)

| Method | Path | Behaviour | API-key scope |
|---|---|---|---|
| GET | `/api/me/favorites` | The caller's favorites in this workspace, oldest star first, filtered to pages the caller can read that are not archived and not templates. Each item: `{ doc_id, title, icon, favorited_at }`. | read |
| PUT | `/api/me/favorites/{doc_id}` | Star. Idempotent (already starred → 204, position unchanged). Needs read access: no access or unknown doc → `403 acl.no_grant`; readable-role-less (archived for viewer) → `404 doc.not_found`; archived → `409 doc.archived`; a template → `422 favorite.not_a_page`; at the cap → `409 favorite.limit_reached` (`details.limit = 200`). 204 on success. | update |
| DELETE | `/api/me/favorites/{doc_id}` | Unstar. Only touches the caller's own row, **no** ACL check (so a page that became hidden or archived can still be unstarred), idempotent, 204. | delete |

- The PUT access check mirrors `require_doc_role_mw` (same codes, same
  order) by calling `acl.access(ctx.workspace_id, doc_id, ctx.user_id)`;
  the routes are **not** under `/api/docs/{id}` so DELETE escapes the
  archived/hidden gate.
- GET filtering follows the notifications pattern: SQL deny-list from
  `visibility.hidden_ids()`, then `visibility_covering` + `readable()`
  re-check, joined to `documents` for `title`, `icon`, `archived_at IS
  NULL`, `NOT is_template`.
- The cap (200) counts all of the user's rows in the workspace, including
  currently hidden ones.
- No scope `OVERRIDES` (method-derived scopes are right), not
  `SESSION_ONLY`, default `Write` rate-limit bucket. No audit, no new
  metric.

### Web

- `web/src/features/favorites/favorites.api.ts` + valibot schema in
  `lib/validators.ts`; query key `["favorites"]`.
- `useFavorites()` hook: the favorites query plus `star(id)` / `unstar(id)`
  mutations with optimistic `setQueryData` and rollback on error; a
  `favoriteIds: Set<string>` for rows.
- **`FavoritesSection`** rendered in `DocTree` between `<WorkspaceHeader/>`
  and the "Documents" header, **outside** the `DndContext`, not reusing
  `TreeRow`.
  - Header styled like "Documents" (`FAVORITES`, 11px uppercase) with a
    collapse chevron; collapsed state in localStorage
    `knot.favoritesCollapsed` (try/catch, like `knot.sidebarWidth`).
  - Hidden entirely when the user has no visible favorites.
  - Rows: page icon + title link to `/doc/{id}`, selected style when open,
    and an unstar button (filled star) on hover/focus (always shown on
    devices without hover). Unstarring by keyboard moves focus to the
    next favorite, else the previous one, else the page's tree row.
  - Titles/icons come from joining favorite ids (in favorites order) with
    the `["docs"]` query data, so renames, archives and access changes
    show up whenever the tree refreshes; ids not in the docs list are
    skipped. Before `["docs"]` loads, the API's own titles are used.
- **TreeRow star**: a star `IconButton` (`lucide-react` `Star`) for every
  role. Outline, shown on hover/focus-within (and always on devices
  without hover) for unstarred rows; filled, accent, always visible for
  starred rows. Label "Add to favorites" / "Remove from favorites", no
  `aria-pressed` (the label already names the action).
- **Context menu**: "Add to favorites" / "Remove from favorites" item
  (`ctx-favorite`) for every role once favorites have loaded, which also
  gives Viewers a row menu; before that it is left out.
- Restoring a page or removing it from templates refetches
  `["favorites"]`.
- Test ids: `favorites` (section), `favorites-toggle`, `fav-row-{id}`,
  `fav-link-{id}`, `fav-unstar-{id}`, `tree-star-{id}`, `ctx-favorite`.
  No reuse of `doc-row-*`, `tree-row-*`, `tree-desc-*`.

### Tests

- Storage integration (`crates/knot-storage/tests/favorites.rs`): add
  appends in order, idempotent add/remove, cap, workspace scoping, purge
  cascades.
- Server integration (`crates/knot-server/tests/favorites_integration.rs`):
  star/list/unstar happy path; viewer can star; restricted page hidden
  from GET and PUT refused with `acl.no_grant`; archived hidden and comes
  back on restore; DELETE works on hidden/archived; template refused;
  limit; API-key scopes (read can GET, not PUT).
- Web vitest: section renders in order and hides when empty; star toggles
  optimistically and rolls back; viewer sees the star and the menu item;
  collapse persists.
- e2e `e2e/flows/favorites.spec.ts`: star from the tree, appears in
  Favorites, navigate via it, persists across reload, unstar; add
  `user_favorites` to `e2e/support/reset.ts` TABLES.

### Docs

- CHANGELOG `[Unreleased]`: Upgrading (migration, locks, rollback =
  drop table), Added (user-facing feature + one bullet per endpoint with
  scope).
- `.claude/skills/knot-api/SKILL.md`: endpoint table rows + error codes.
- Plan + outcome note + `docs/superpowers/README.md` row.

## Left to the builder

Exact migration timestamp, Rust type names, component file split, and
copy for tooltips, within the shapes above.
