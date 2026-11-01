# Document archive with 30-day retention — design

**Date:** 2026-10-13
**Status:** Approved (brainstorm)
**Issue:** [#37](https://github.com/opendefensecloud/knot/issues/37)

## Context

Issue #37: "A folder in the sidebar cannot be deleted" (knot 0.7.0). knot has
no folder type; a "folder" is a doc with subpages. Deleting is a soft delete
(`DELETE /api/docs/{id}` sets `archived_at`). Investigating the delete path
turned up these problems:

1. **The role gates disagree.** The sidebar offers Delete to owners *and*
   editors (`DocTree.tsx`, `canEdit`), but the handler requires the effective
   doc role Owner (`routes/api/docs.rs`, `acl.owner_required`). An editor
   confirms, gets a 403, and sees a bare "Delete failed" toast. Owner-only
   came from the docs-and-ACL plan; the 2026-06-23 edit-mode spec then kept
   the sidebar's `canEdit` gate without noticing the mismatch. No test covers
   an editor deleting.
2. **Deleting a folder leaves its contents behind.** `archive` stamps only the
   one row. Its subpages stay live, `list_alive` still returns them, and
   `buildTree` promotes docs whose parent is missing to the top level. The
   folder vanishes and its pages pop up at the sidebar root, which to an owner
   looks like the folder did not delete. They also stay in search and in
   workspace export.
3. **Archived docs stay live everywhere else.** Only `list_alive` and search
   filter on `archived_at`. `GET /api/docs/{id}`, the collab WebSocket,
   markdown export, comments, public share links and My Tasks
   (`tasks.rs::list_for_assignee`) still serve an archived doc. The web client
   ignores the `archived` flag; deleting the page you are on leaves it open
   and editable.
4. **Undo isn't possible from the UI.** `POST /api/docs/{id}/restore` exists
   (owner-only) but the web client never calls it.
5. **Keyboard users can't use the row menu.** `ContextMenu` portals to
   `document.body`, but React events still bubble through the React tree to
   the sortable `<li>`, whose dnd-kit `KeyboardSensor` claims Enter/Space and
   calls `preventDefault()`. The menu item's click never fires.
6. **Errors are generic.** The toast says "Delete failed" whether the cause
   was a 403 or a 500.

The request on top of fixing these: deleted documents go to an **archive** and
are only **really deleted after a 30-day retention period**.

## Decisions

| Question | Decision |
| --- | --- |
| Why purge at all? | Safety net against accidental deletes, *and* a data-removal duty: content must sometimes be gone immediately. Owners get "Delete permanently now". |
| Who deletes (archives)? | Editor+ on the doc. |
| Who sees the archive and restores? | Workspace editor+ see the archive list; editor+ on the doc restore. Viewers don't see the archive. |
| Who purges early? | Owner only, and only docs already in the archive (two steps). |
| Can archived docs be opened? | Yes, as a read-only preview with a banner, for editor+. Viewers get 404. |
| Folder semantics | Archiving a doc archives its whole live subtree; restoring it brings the same set back. |
| Retention | `KNOT_ARCHIVE_RETENTION_DAYS`, default 30. |
| Docs archived before this release | Their 30-day clock starts at the upgrade, not at the original deletion. |
| Data model | Approach A: stamp every row of the archived subtree with the doc the user deleted (`archive_root_id`). Rejected: marking only the root and deriving descendants on every read (every read path needs an ancestor walk, and missing one is the bug class in finding 3); a separate archive table (all child tables reference `documents(id)`). |

## Design

### 1. Data model

New migration on `documents`:

- `archived_by uuid NULL REFERENCES users(id) ON DELETE SET NULL`: who
  archived it.
- `archive_root_id uuid NULL`: the doc the user archived. On that doc it
  equals `id`; on each descendant archived with it, it points at that doc.
  No foreign key: the root and its descendants are always deleted together
  by the parent cascade.
- `purge_requested_at timestamptz NULL`: set on an archive root (and on
  the archive roots inside it) once a purge has started deleting blob bytes
  outside the database. Restore is refused from then on, and the purge job
  retries the purge until it completes.
- `CHECK ((archived_at IS NULL) = (archive_root_id IS NULL))`.
- Partial index `documents_archive_roots_idx ON documents (workspace_id,
  archived_at) WHERE archive_root_id = id`, which serves the archive list and
  the purge scan.

Terms used below: an **archive root** is a row with `archive_root_id = id`.
Its **archive set** is every row with `archive_root_id` equal to that id.

Every table holding doc content already references `documents(id)` with
`ON DELETE CASCADE`: `doc_updates`, `doc_snapshots`, `doc_markdown_cache`,
`document_grants`, `doc_contributors`, `comments` (and `comment_reactions`
through it), `share_tokens`, `boards` (and `board_updates`/`board_snapshots`
through it), `doc_tasks`, `blobs` (and `blob_bytes` through it). Child docs
cascade through `documents.parent_id`. `audit_events` and `acl_invalidations`
keep ids only and are not touched by a purge.

### 2. Lifecycle (`DocStore`)

**Archive X:** one transaction.

1. `SELECT … FOR UPDATE` on X; `NotFound` if X is missing, `Archived` if it
   is already archived.
2. Recursive CTE over X's descendants that have `archived_at IS NULL` (a
   branch whose root is already archived is not descended; that subtree
   belongs to its own archive set).
3. `UPDATE` X plus those rows: `archived_at = now()`, `archived_by = actor`,
   `archive_root_id = X`. Repeat steps 2–3 until a pass stamps nothing: a
   create that held its parent `FOR SHARE` can commit a child after one
   UPDATE took its snapshot, and the next statement sees it.
4. Audit `doc.archive` on X. Write one ACL invalidation for X; the listener
   walks X's descendants and evicts (and revokes sockets on) each of them.

Descendants archived earlier keep their own root, `archived_by` and clock.
Such a nested root always has an earlier `archived_at` than any archived
ancestor, because an archived subtree cannot be archived again. So purging
the ancestor never cuts its retention short.

**Restore X:**

0. Lock X's parent `FOR SHARE` before locking X `FOR UPDATE`, the order
   archive uses. A concurrent archive of the parent then either commits
   first (and X goes to the top level, step 3) or waits for the restore (and
   stamps X along with the parent). Reading the parent without the lock
   could leave X live under an archived parent, where purging the parent
   would delete it.
1. X must be an archive root. A descendant archived with its folder gets
   `Conflict` (`doc.not_archive_root`); a live doc gets `NotFound`; a root
   whose purge has started gets `Purging` (`doc.purging`).
2. Clear `archived_at`, `archived_by`, `archive_root_id` on X's archive set.
3. If X's parent is archived, set X's `parent_id = NULL` and give it a
   `sort_key` after the last top-level sibling. Restoring a doc never
   resurrects an archived ancestor. Otherwise X returns to its old place
   with its old `sort_key`; the key is still reserved because `siblings()`
   includes archived rows.
4. Audit `doc.restore`; ACL invalidation for every restored row.

**Purge X:** shared by the retention job and the owner action.

1. X must be an archive root: a live doc gets `NotArchived`
   (`doc.not_archived`), a descendant archived with its folder gets
   `NotArchiveRoot` (`doc.not_archive_root`).
2. Only for a blob backend outside the database (S3,
   `BlobStore::bytes_deleted_with_metadata() == false`):
   1. `begin_purge`, committed on its own: set `purge_requested_at` on X and
      on every archive root in X's subtree. From here on restore refuses
      them, so nothing comes back with half its attachments gone.
   2. Collect the ids of all blobs whose `doc_id` is in what step 3 deletes.
   3. Delete those objects. Any failure aborts the purge before the
      database rows go; `due_for_purge` keeps returning X (whatever its age)
      until a run completes it.

   With the Postgres backend the bytes are rows that cascade from `blobs`
   inside step 3's transaction, so there is no separate delete and no
   window.
3. One transaction: delete X and every row stamped with `archive_root_id =
   X` (a move racing the archive can leave one outside X's subtree; on its
   own it could never be restored, listed or purged). The parent cascade
   takes their subtrees. Audit `doc.purge` on X: actor = the owner, or
   `NULL` for the retention job. ACL invalidation for every deleted id (once
   the rows are gone the listener can't find descendants itself).
4. Evict X's room and its descendants' rooms from the in-process registry,
   disconnecting any read-only viewers.

**Live-parent check on create and move:** `create`, `create_from_template`
and `move_to` lock the target parent with `SELECT … FOR SHARE WHERE
archived_at IS NULL` inside their transaction and fail with `Archived` if it
returns no row. Under READ COMMITTED this blocks behind a concurrent
archive's row lock and then re-checks the committed row, so a child can't
slip into a subtree being archived.

New `DocStoreError` variants: `Archived`, `NotArchived`, `NotArchiveRoot`,
`Purging`. New `DocStore` methods: `subtree_ids`, `begin_purge`, `purge`,
`list_archive_roots(workspace_id)`, `due_for_purge(cutoff, limit)`.
`Document` gains `archived_by` and `archive_root_id`.

### 3. Upgrade migration (pure SQL, same migration as §1)

1. `UPDATE documents SET archive_root_id = id, archived_at = now() WHERE
   archived_at IS NOT NULL`: every doc already archived becomes an archive root
   with a fresh retention clock. The original deletion time stays in
   `audit_events` (`doc.archive`).
2. `UPDATE documents c SET parent_id = NULL FROM documents p WHERE c.parent_id
   = p.id AND c.archived_at IS NULL AND p.archived_at IS NOT NULL`: live
   children of archived docs move to the top level, which is where the sidebar
   shows them today. Without this, purging their archived parent would
   cascade-delete pages people are still using. No `sort_key` conflicts: the
   `UNIQUE (workspace_id, parent_id, sort_key)` constraint treats NULL
   parents as distinct.

Order matters: step 2 runs after step 1. Both steps run before the `CHECK`
constraint is added.

### 4. Permissions and API

| Action | Endpoint | Gate | Errors |
| --- | --- | --- | --- |
| Archive | `DELETE /api/docs/{id}` (204) | editor+ on the doc (was owner) | 404 `doc.not_found`; 409 `doc.archived` if already archived |
| Restore | `POST /api/docs/{id}/restore` (204) | editor+ on the doc (was owner) | 404 if live; 409 `doc.not_archive_root`; 409 `doc.purging` |
| Purge | `POST /api/docs/{id}/purge` (204), **new** | owner on the doc | 409 `doc.not_archived`; 409 `doc.not_archive_root` |
| Archive list | `GET /api/workspace/archive`, **new** | workspace editor+ | 403 `acl.editor_required` |
| Doc metadata | `GET /api/docs/{id}` | as today; viewer on an archived doc → 404 `doc.not_found` | |
| Retention for the UI | `GET /api/workspace` gains `archive_retention_days` | as today | |

`GET /api/docs/{id}` adds `archived_at`, `archived_by` (`{id,
display_name}`), `purge_at`, `archive_root_id` and `archive_root_title`
(all `null` when live). `archived: bool` stays.

`GET /api/workspace/archive` returns archive roots in the caller's
workspace, newest first: `id`, `title`, `parent_id` and `parent_title`
(original parent, `null` at top level), `subpage_count` (size of the archive
set minus one), `archived_at`, `archived_by`, `purge_at`, `purging`. Like `GET
/api/docs`, the list is workspace-wide, not filtered by per-doc grants.

`purge_at = archived_at + retention` is computed server-side.

### 5. Archived means read-only

**One gate in `require_doc_role_mw`.** `acl::resolve` already loads the doc,
so it returns the doc's archived state alongside the role, and the
`AclCache` value becomes `(role, archived)`. The cache key and TTL don't
change. Archive, restore and purge write an ACL invalidation for every
affected row, which evicts the entry on every replica through the existing
NOTIFY listener. When the doc is archived:

- caller's effective role is Viewer → 404 `doc.not_found` for every route;
- any non-`GET` request → 409 `doc.archived`, except the restore and purge
  routes.

This covers rename, move, markdown import, grants, comments (create, edit,
resolve, react), template toggle, history restore and from-template (the
template is the `{id}`), without a check in each handler.

**Routes outside that middleware get the same rule explicitly:**

- **Collab WS** (`collab_upgrade`): viewer → refuse the upgrade; editor+ →
  `can_write = false`.
- **Board WS and board routes:** resolve the owning doc and apply the same
  rule.
- **Blob routes:** download follows the read rule; upload and delete are
  refused with 409 `doc.archived`.
- **Board WS:** refused for archived docs. It only exists for editing; the
  read-only preview shows boards through their cached SVG.
- **Public share** (`/p/{token}`, its board SVG and blob routes): a share
  link to an archived doc behaves like a revoked one (410 page, 404 for the
  board SVG and blob routes). `ShareTokenStore::find_alive` filters on the
  doc's `archived_at`, which covers all three routes.
- **Shares:** creating a share link on an archived doc → 409
  `doc.archived`. Listing and revoking existing links still work.
- **Task checkbox** (`PATCH /api/docs/{doc_id}/tasks/{item_index}`): 409
  `doc.archived`.
- **Create and move into an archived parent:** 409 `doc.archived` (§2).

**Open sockets.** No new close code. Archive writes an ACL invalidation, and
the existing listener on every replica already revokes every connection to
the affected docs (and their descendants), so clients get the existing 4403.
On 4403 the web client refetches the doc's metadata before deciding: if the
doc is archived, it shows the archived view and remounts the editor, which
reconnects read-only. Otherwise it keeps today's behaviour ("You no longer
have access", back to `/`).

**Local cache eviction.** The NOTIFY-driven eviction is asynchronous, so the
archive, restore and purge handlers also evict the affected docs from the
local `AclCache` before responding. A rename sent right after an archive on
the same replica then sees the archived state.

**Listings that must leave archived docs out:** `list_alive` and search
already do, and with §2 that covers subpages. Fix
`tasks.rs::list_for_assignee` (join `documents`, `archived_at IS NULL`) and
`BlobMetaStore::list_for_workspace` as used by workspace export (only blobs
of live docs).

### 6. Purge job

- **Config:** `KNOT_ARCHIVE_RETENTION_DAYS` in `knot-config`, default 30,
  minimum 1 (startup fails otherwise). Add it to the Helm `values.yaml`, the
  chart's env wiring and the config docs.
- **Task:** spawned from `main.rs` next to `knot_crdt::spawn_gc`, ticking
  hourly. The first run happens one interval after startup, so a deploy with
  a wrong retention value can be rolled back before anything is deleted.
- **One replica at a time:** each run takes
  `pg_try_advisory_lock(<fixed key>)` on a dedicated connection and skips
  the run if it isn't granted. It releases the lock when done.
- **Work loop:** `due_for_purge` returns archive roots with `archived_at <
  now() - retention`, or whose purge has started (`purge_requested_at` set,
  e.g. an owner's purge whose S3 deletes failed), oldest first, in batches
  of 100. Each root is purged
  independently (§2); a failure is logged with the doc id and counted, and
  the loop moves on.
- **Metrics:** `knot_docs_purged_total{trigger="retention"|"manual"}` and
  `knot_docs_purge_failures_total`, following `metrics.rs` conventions.
- **Ops note** in the deployment docs: Postgres backups and PITR keep purged
  content for as long as the operator keeps them. That is outside the app's
  control.

### 7. Web UI

**Sidebar (`DocTree.tsx`).**

- The row-menu item "Delete" becomes **"Move to archive"** (`ctx-delete`
  test id kept), gated on `canEdit`, which now matches the server.
- Confirm text: `Move "X" to the archive?`, plus `Its N subpages go with it.`
  when N > 0 (N from `descendantIds`), plus `It will be permanently deleted
  in D days unless restored.` (D from `archive_retention_days`).
- On success: toast "Moved to archive"; invalidate `["docs"]`, `["doc",
  id]` for the active doc, and `["archive"]`. If the active doc was in the
  subtree, the page stays open and switches to the archived view.
- On failure: the toast maps the error code to a sentence (`acl.*` → "You
  don't have permission to do that", `doc.archived` → "Already in the
  archive", otherwise "Couldn't move to archive").
- New **Archive** nav link in `WorkspaceHeader` after Templates, shown to
  workspace editor+ (`sidebar-archive`).

**Archive page (`/archive`, `features/docs/ArchivePage.tsx`).** One row per
archive root: title, original location ("in Parent" or "top level"),
"+N subpages", "Deleted by Anna · 12 Oct", "Deleted permanently in N days".
Actions:

- **Open** → `/doc/{id}`;
- **Restore** (editor+), then invalidate `["docs"]` and `["archive"]`;
- **Delete permanently** (owner), behind a confirm that names the doc and
  subpage count and says it can't be undone.

Empty state: "Nothing in the archive."

**Archived doc view (`DocPage.tsx`).**

- When `archived_at` is set, the editor is read-only whatever the role.
- Banner for an archive root: "In the archive. Deleted by Anna on 12 Oct.
  Permanently deleted on 11 Nov." with **Restore** and, for owners, **Delete
  permanently**.
- Banner for a descendant: "Part of 'X', which is in the archive", linking
  to X, with no actions.
- Hidden while archived: the edit toggle, share, comments, history,
  markdown import, the template toggle and the zip export (which reads live
  docs only; restore to export). The server rejects their writes anyway;
  hiding the whole panel is simpler than a read-only variant of each.
- Kept: the markdown view.
- On provider status `unauthorised` (close 4403), refetch `["doc", id]`. The
  server sends 4403 on any access change: archive, restore, a grant. If the
  doc is still readable, stay and remount the editor so it reconnects with
  what the server now grants (read-only once archived; writable again after
  a restore). Only a doc that can no longer be read sends the user to `/`
  with "You no longer have access". A purge started from the banner sets a
  flag so its own revoke doesn't race the navigation to `/archive`.
- The archive list shows "Being deleted permanently" and no Restore for an
  entry whose purge has started. An entry without a known deleter (deleted
  before this release, or by someone since removed) says "Deleted on
  <date>".

**Keyboard fix (finding 5).** The sidebar row passes its own `<li>` as the
dnd-kit activator node, so the keyboard sensor only starts a drag (and
swallows Enter/Space) when the row itself is focused, not on its link or
buttons. `ContextMenu` stops `keydown` and `pointerdown` propagation on its
root element (the portal still bubbles through the React tree), handles
Escape itself, focuses its first item when it opens, and moves focus with
the arrow keys.

### 8. Testing

**Storage (`crates/knot-storage/tests/documents.rs`):**

- archive stamps the live subtree with one root, leaves an earlier-archived
  nested root untouched, and sets `archived_by`;
- restore brings back exactly the archive set; restore under an archived
  parent goes to top level with a non-colliding key; restoring a non-root →
  `NotArchiveRoot`;
- purge deletes the subtree and rows in every child table (one row seeded
  per table);
- `due_for_purge` honours the retention window;
- create and move into an archived parent → `Archived`, including a
  concurrent archive-vs-create test;
- migration backfill: existing archived rows become roots with a fresh
  clock, and their live children move to top level.

**Server integration (`crates/knot-server/tests/docs_integration.rs` and a
new `archive_integration.rs`):**

- an editor can archive (regression for #37);
- a viewer can't archive, and gets 404 on an archived doc;
- one table-driven test asserts 409 `doc.archived` for every non-GET route
  under `/api/docs/{id}/…` except restore and purge;
- restore and purge role gates and error codes;
- archive list contents and gating;
- public share link to an archived doc → 410, like a revoked one;
- My Tasks and export leave archived docs out;
- purge job: removes S3 objects through a test blob store, keeps the DB rows
  when that store fails, and skips the run when the advisory lock is held;
- collab: the upgrade on an archived doc is read-only for an editor and
  refused for a viewer.

**Web unit (Vitest):**

- confirm text with and without subpages;
- "Move to archive" visible to editors, hidden from viewers;
- archive page actions by role;
- banner variants (root and descendant);
- on `unauthorised`, an archived doc stays open in the archived view;
- Enter on a context-menu item calls `onSelect`.

**E2E (`e2e/flows`):**

1. Create a folder with two subpages.
2. Move it to the archive; it and its subpages are gone from the sidebar and
   search.
3. Open the preview from `/archive`: read-only, with the banner.
4. Restore: the tree is back as before.
5. As owner: archive again, delete permanently, and `GET /api/docs/{id}`
   returns 404.

## Out of scope

- Restoring a single subpage out of an archived folder (open the folder's
  preview instead, or restore the folder).
- Filtering the archive list or the doc list by per-doc grants (both are
  workspace-wide today).
- Purging data from database backups.
- Per-workspace retention settings.
