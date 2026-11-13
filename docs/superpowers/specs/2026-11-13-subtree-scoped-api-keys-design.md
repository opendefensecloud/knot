# Subtree-scoped API keys (design)

**Date:** 2026-11-13
**Builds on:** [2026-11-06-api-keys-and-bots-design.md](2026-11-06-api-keys-and-bots-design.md)
(threats S1–S9, scope model, credential lifecycle; all unchanged here)

## Goal

Let a user (or an owner, for a bot) mint an API key that reaches only one
page and its subpages, present and future. A script or AI agent working on
one project then can't read or change anything else in the workspace, even
when its user can.

A key's reach is always **user ∩ HTTP scopes ∩ subtree**: the subtree only
narrows, it never grants. A key without a root behaves exactly as today.

**Not in scope:** several roots per key (mint several keys instead); a root
that is readable but not writable; opting a subtree key back into
workspace-wide endpoints (notifications, templates, export, …) — a key that
needs those is a key without a root; if real demand shows up, it becomes an
additive scope, not a switch on the subtree; a page-menu entry point
("API key for this page…") and a searchable page picker.

## Threat model

Adds one threat to the S-table of the API-keys design. Code comments cite it
as `S10`.

| # | Threat | Enforced by |
|---|---|---|
| **S10** | A subtree key reads or changes a page outside its subtree, or workspace-wide state (members, templates, export/import/archive, the user's notifications), or widens its own root. | The root is part of the ACL rule itself (`knot-docs`): a page whose ancestor chain doesn't contain the root resolves to no access, in `acl::resolve` and in `rule::compute_access`, so every doc-scoped route, listing, search and task query narrows without its own check. Workspace-wide routes are refused by `auth/scope.rs::SUBTREE_DENIED`; a test forces every `/api/*` route into `SUBTREE_DENIED` or `SUBTREE_OK`. Top-level destinations are refused in `require_editable_destination` and `duplicate.rs`. In the database, trigger `KB005` refuses a root from another workspace and any change of `root_doc_id` after insert; the foreign key is `ON DELETE CASCADE`, never `SET NULL`, which would silently widen the key to the whole workspace. |

## Data model

Migration `api_keys_root_doc`:

```sql
ALTER TABLE api_keys
  ADD COLUMN root_doc_id uuid NULL REFERENCES documents(id) ON DELETE CASCADE;
CREATE INDEX api_keys_root_doc_idx ON api_keys (root_doc_id) WHERE root_doc_id IS NOT NULL;
```

- `NULL` = no subtree, today's behaviour. Existing keys stay `NULL`.
- `ON DELETE CASCADE`: purging the root deletes the key. Archiving the root
  does not; the key keeps working within the archive rules of its user
  (a viewer can't read archived pages, an editor can restore them).
- Trigger `api_keys_root_doc_guard` (`KB005`), `BEFORE INSERT OR UPDATE OF
  root_doc_id`: on insert, the root's `workspace_id` must equal the key's;
  on update, `root_doc_id` must not change. `documents` has no
  `UNIQUE (workspace_id, id)`, so a composite foreign key isn't available;
  the trigger follows the `KB001`–`KB004` pattern.
- Takes the same lock discipline as `20261112120002` (`knot_lock_tables`
  on `api_keys`, `documents`).

## Minting

`POST /api/auth/api-keys` and the bot-key variant accept an optional
`root_doc_id`.

- The key's principal (the user, or the bot) must be able to read the root
  at mint time (`AclCache::access(..).can_read()` for that principal, without
  a root). Otherwise `404 doc.not_found`, the same answer as an id that
  doesn't exist, so the endpoint can't probe page ids.
- The check runs before the mint transaction, not inside it. A race with a
  grant change is harmless: if the principal loses read access later, the
  key simply reaches nothing, since the rule is evaluated per request.
- The listing returns `root_doc_id` and `root_title`. `root_title` is `null`
  when the viewer of the listing can't read the root.

## Authentication and the ACL rule

1. `find_live_by_hash` returns `root_doc_id`; `api_key_loader.rs` puts it
   into `AuthContext.root_doc_id: Option<Uuid>`. A cookie session always has
   `None`.
2. `AclCache::access`, `effective_role`, `visibility` and
   `visibility_covering` take the root alongside `user_id`. The cache keys
   become `(workspace_id, doc_id, user_id, root)` and
   `(workspace_id, user_id, root)`, so the same user with and without a
   root never shares an entry. Invalidation is unchanged (evictions already
   clear by doc and sweep visibility).
3. `acl::resolve`: when a root is given and it isn't among the ids of the
   loaded ancestor chain (the doc itself included), the result is `None`.
4. `rule::compute_access`: when a root is given, every node whose ancestor
   walk doesn't reach the root gets no entry. Grants, restrictions and the
   workspace role are folded as before, inside the subtree only; a
   restricted page inside the subtree stays out of reach for a user without
   a grant on it.
5. Every call site passes `ctx.root_doc_id` (or `None` where the principal
   is not a request, e.g. background sweeps). The signature change makes
   the compiler list all of them; none may default silently.

## Top-level destinations

A page without a parent is outside every subtree. One check, for a context
with a root and a destination `parent_id == None`, answers
`403 api_key.subtree_denied`:

- `routes/api/docs.rs::require_editable_destination` — `POST /api/docs`,
  `POST /api/docs/from-template/{id}`, `POST /api/docs/{id}/move`.
- `duplicate.rs`, the `None` arm of `source.parent_id` — duplicating a
  top-level page.

Everything else follows from the rule without its own code:

- Creating, moving or duplicating under a parent outside the subtree:
  `require_editable_parent` sees no access, `404 doc.not_found`.
- Moving or duplicating the root itself: the destination is the root's
  parent, outside, so refused the same way.
- Deleting, archiving or restoring the root: allowed under the key's HTTP
  scopes; the root belongs to its subtree.
- `GET /api/docs/{id}` of the root: the parent is already blanked when
  unreadable (`parent_hidden`), so nothing outside leaks.

## Workspace-wide routes

`auth/scope.rs` gets `SUBTREE_DENIED: &[(Method, &str)]`, checked in
`scope_mw` after `SESSION_ONLY`, refusing a key with a root with
`403 api_key.subtree_denied`:

| Route | Why |
|---|---|
| `GET /api/workspace/members`, `PATCH/DELETE /api/workspace/members/{id}` | who is in the workspace is not page content (writes are also `SESSION_ONLY`) |
| `GET /api/workspace/templates` | templates live anywhere in the workspace |
| `/api/workspace/export`, `/api/workspace/import`, `/api/workspace/archive` (all methods) | workspace-wide by definition |
| `GET /api/notifications`, `GET /api/notifications/unread_count`, `POST /api/notifications/read` | the user's inbox; doc-less notifications pass the visibility filter, and `read` checks no ACL at all |
| `POST /api/whats-new/seen` | changes the user's state |

`SUBTREE_OK` lists the rest, each with a one-line reason: every
`/api/docs/{id}/…`, `/api/blobs/{id}`, `/api/boards/{id}…` and
`/api/short-links/{code}` route (resolved through the doc ACL), `GET
/api/docs` (tree, filtered by `Visibility`), `GET /api/search` and `GET
/api/workspace/tasks` (filtered by `Visibility`; every task and hit has a
doc), `GET /api/workspace` (id, slug, name, role, retention — needed by
clients, no page content), `/api/version`, `/api/release-notes`, `GET
/api/whats-new`, `/api/healthz`, `/api/readyz`.

A test walks the built router and fails when an `/api/*` route is in neither
table, so a new route must be classified before it ships. Both tables also
get the row-count tests the existing tables have.

Errors: a page outside the subtree is `404`, indistinguishable from a page
the user can't see. A workspace-wide route or a top-level destination is
`403 api_key.subtree_denied`. `scope_mw` logs subtree refusals like scope
refusals (`key_id`, method, route template).

## Web UI

`features/apiKeys/ApiKeysSection.tsx`:

- The create form gets a native `<select>` "Pages": "All pages" (default)
  followed by every non-archived page of the already loaded tree, indented
  by depth (`tree.ts`). Hint below: "The key sees only this page and its
  subpages."
- The key list shows a chip with the root's title, linking to the page, or
  "a page you can't see" when `root_title` is `null`.
- Bot keys use the same form; `apiKeyErrorMessage` maps `doc.not_found` on
  mint to "The bot can't read that page." (for one's own key: "That page
  no longer exists.").
- `apiKeys.api.ts`: `root_doc_id?: string` on create, `root_doc_id` and
  `root_title` on the listed key.

## Testing

- **knot-docs:** table tests for `access_along_path` and `compute_access`
  with a root: root, child, grandchild, sibling, parent, restricted page
  inside the subtree (with and without a grant), cycle. Cache test: the
  same user with and without a root gets separate, correct answers.
- **knot-storage:** migration applies; purging the root deletes the key;
  `KB005` refuses a foreign-workspace root and an update of `root_doc_id`.
- **knot-server integration (`api_keys_integration.rs`):** with a subtree
  key — read, write, create inside work; outside is `404`; top-level
  create/move/duplicate, moving or duplicating the root and every
  `SUBTREE_DENIED` route are `403 api_key.subtree_denied`; tree, search and
  tasks list only the subtree; minting with an unreadable root is `404`; a
  bot key with a root behaves the same. Plus the router-walk test.
- **web:** Vitest — the create body carries `root_doc_id`; the chip renders
  both states. e2e `api-keys.spec.ts` — mint a subtree key, a page outside
  answers `404`.

## Documentation

`ARCHITECTURE.md` (request lifecycle: subtree), `CHANGELOG.md`, the
`knot-api` skill, and in the API-keys design the "Not in scope" line points
here.
