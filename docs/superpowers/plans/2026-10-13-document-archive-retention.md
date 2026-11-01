# Document archive with 30-day retention — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deleting a doc moves it and its subtree into an archive; editors+
can preview and restore for a retention period (default 30 days); owners can
purge early; a background job purges what's due, including S3 attachments.
Fixes issue #37 (editors offered Delete but refused; folders left their
subpages behind).

**Architecture:** Stamp every row of an archived subtree with
`archive_root_id` (spec approach A). The ACL lookup returns the archived flag
with the role, so the doc middleware, the WS upgrades and the few routes
outside the middleware can treat archived docs as read-only (editor+) or
invisible (viewers). Purge = delete S3 objects, then delete the root row and
let the existing `ON DELETE CASCADE` foreign keys remove everything else. An
hourly task purges due roots under a Postgres advisory lock.

**Tech stack:** Rust (axum 0.8, sqlx 0.9, tokio, moka), Postgres 18, React +
TanStack Query + valibot (web), Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-10-13-document-archive-retention-design.md`

## Global constraints

- Retention: `KNOT_ARCHIVE_RETENTION_DAYS`, default `30`, minimum `1`.
- Error codes: `doc.archived` (409), `doc.not_archived` (409),
  `doc.not_archive_root` (409), `doc.not_found` (404).
- Archive and restore: editor+ on the doc. Purge: owner on the doc. Archive
  list: workspace editor+.
- Viewers never see archived docs (404 on doc routes, refused on WS).
- Docs archived before this release get `archived_at = now()` at migration.
- Live children of already-archived docs move to the top level at migration.
- Audit actions: `doc.archive`, `doc.restore`, `doc.purge`.
- Metrics: `knot_docs_purged_total{trigger}` (`retention` | `manual`),
  `knot_docs_purge_failures_total`.
- Purge job: hourly, first run one interval after start,
  `pg_try_advisory_lock`, batches of 100.
- Never commit or push from an agent session in this repo without the user's
  explicit go-ahead (user instruction); the commit steps below are for the
  human.

## Review focus

1. **Concurrent create under a folder being archived:** the new child must end
   up archived or the create must fail; never a live child under an archived
   parent. Pinned in Task 3 (`create_racing_archive_never_leaves_live_child`).
2. **A rename or comment right after archive on the same replica:** must get
   409, not succeed against a stale ACL cache entry. Pinned in Task 6 (local
   eviction, `writes_to_archived_doc_are_rejected`).
3. **Restore while the original parent is still archived:** the restored doc
   must appear at the top level, not vanish under an archived parent. Pinned
   in Task 1 (`restore_under_archived_parent_goes_to_top_level`).
4. **S3 delete fails during purge:** the DB rows must stay so the next run
   retries; nothing half-deleted. Pinned in Task 7
   (`purge_keeps_rows_when_blob_delete_fails`).
5. **Upgrade with old soft-deleted folders:** their live children must not be
   cascade-deleted by the first purge. Pinned in Task 1
   (`migration_backfill_detaches_live_children`).

---

### Task 1: Schema, archive and restore in `DocStore`

**Files:**
- Create: `migrations/20261013120000_doc_archive.sql`
- Modify: `crates/knot-storage/src/doc_store.rs`
- Test: `crates/knot-storage/tests/documents.rs`

**Interfaces:**
- Produces: `Document { …, archived_by: Option<Uuid>, archive_root_id: Option<Uuid> }`;
  `DocStoreError::{Archived, NotArchived, NotArchiveRoot}`;
  `DocStore::archive(ws, doc, actor) -> Result<Vec<Uuid>, DocStoreError>`
  (ids stamped, root first); `DocStore::restore(ws, doc, actor) ->
  Result<Vec<Uuid>, DocStoreError>` (ids restored).

- [ ] **Step 1: Migration**

```sql
-- Document archive with retention (#37). See
-- docs/superpowers/specs/2026-10-13-document-archive-retention-design.md.
ALTER TABLE documents
    ADD COLUMN archived_by     uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN archive_root_id uuid NULL;

-- Docs soft-deleted before this release become archive roots with a fresh
-- retention clock (the original time stays in audit_events).
UPDATE documents
   SET archive_root_id = id, archived_at = now()
 WHERE archived_at IS NOT NULL;

-- Their live children are what the sidebar shows at the top level today.
-- Detach them for real, or purging the archived parent would cascade-delete
-- pages people still use. UNIQUE (workspace_id, parent_id, sort_key) treats
-- NULL parents as distinct, so keeping their sort_key cannot conflict.
UPDATE documents c
   SET parent_id = NULL
  FROM documents p
 WHERE c.parent_id = p.id
   AND c.archived_at IS NULL
   AND p.archived_at IS NOT NULL;

ALTER TABLE documents
    ADD CONSTRAINT documents_archive_consistent
    CHECK ((archived_at IS NULL) = (archive_root_id IS NULL));

CREATE INDEX documents_archive_roots_idx
    ON documents (workspace_id, archived_at)
 WHERE archive_root_id = id;
```

- [ ] **Step 2: Failing tests** in `crates/knot-storage/tests/documents.rs`:
  `archive_stamps_live_subtree_with_one_root`,
  `archive_leaves_earlier_archived_subtree_alone`,
  `archive_twice_is_archived_error`,
  `restore_brings_back_exactly_the_archive_set`,
  `restore_under_archived_parent_goes_to_top_level`,
  `restore_of_non_root_is_rejected`,
  `migration_backfill_detaches_live_children`: create an empty database with
  `knot_test_support::fresh_db_url()`, apply every migration older than
  `20261013120000` by iterating `sqlx::migrate!("../../migrations")` and
  executing each `m.sql`, insert the pre-feature state (a folder with
  `archived_at` 90 days ago and a live child), execute the new migration's
  SQL, then assert the folder is a root with `archived_at` within the last
  minute and the child has `parent_id IS NULL` and is still live.

- [ ] **Step 3: Run** `KNOT_TEST_DATABASE_URL=… cargo test -p knot-storage --test documents` → new tests fail to compile.

- [ ] **Step 4: Implement** in `doc_store.rs`:
  - `COLS` gains `archived_by, archive_root_id`; `DocRow` and `doc_from_row` grow.
  - `archive`:

```rust
let mut tx = crate::begin(&self.pool).await?;
let state: Option<bool> = sqlx::query_scalar(
    "SELECT archived_at IS NOT NULL FROM documents
      WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
)
.bind(workspace_id).bind(doc_id)
.fetch_optional(&mut *tx).await?;
match state {
    None => return Err(DocStoreError::NotFound),
    Some(true) => return Err(DocStoreError::Archived),
    Some(false) => {}
}
// Repeat until no live descendant is left: a create racing this one may
// commit a child after an UPDATE's snapshot (see Task 3).
let mut stamped: Vec<Uuid> = Vec::new();
loop {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "WITH RECURSIVE sub AS (
             SELECT id FROM documents WHERE id = $2
             UNION ALL
             SELECT d.id FROM documents d JOIN sub s ON d.parent_id = s.id
              WHERE d.archived_at IS NULL OR d.archive_root_id = $2
         )
         UPDATE documents SET archived_at = now(), archived_by = $3,
                              archive_root_id = $2, updated_at = now()
          WHERE workspace_id = $1 AND id IN (SELECT id FROM sub)
            AND archived_at IS NULL
         RETURNING id",
    )
    .bind(workspace_id).bind(doc_id).bind(actor)
    .fetch_all(&mut *tx).await?;
    if ids.is_empty() { break; }
    stamped.extend(ids);
}
// Root first, for callers that report it.
stamped.sort_by_key(|id| *id != doc_id);
audit::record_in_tx(&mut tx, workspace_id, Some(actor), "doc.archive", "doc", doc_id).await?;
invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "archive").await?;
tx.commit().await?;
Ok(stamped)
```

  - `restore`:

```rust
let mut tx = crate::begin(&self.pool).await?;
let row: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
    "SELECT archive_root_id, parent_id FROM documents
      WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
)
.bind(workspace_id).bind(doc_id)
.fetch_optional(&mut *tx).await?;
let parent_id = match row {
    None | Some((None, _)) => return Err(DocStoreError::NotFound),
    Some((Some(root), _)) if root != doc_id => return Err(DocStoreError::NotArchiveRoot),
    Some((Some(_), parent)) => parent,
};
let restored: Vec<Uuid> = sqlx::query_scalar(
    "UPDATE documents SET archived_at = NULL, archived_by = NULL,
                          archive_root_id = NULL, updated_at = now()
      WHERE workspace_id = $1 AND archive_root_id = $2
     RETURNING id",
)
.bind(workspace_id).bind(doc_id)
.fetch_all(&mut *tx).await?;
if let Some(parent) = parent_id {
    let parent_archived: bool = sqlx::query_scalar(
        "SELECT archived_at IS NOT NULL FROM documents WHERE id = $1",
    )
    .bind(parent).fetch_one(&mut *tx).await?;
    if parent_archived {
        let last_top: Option<String> = sqlx::query_scalar(
            "SELECT max(sort_key) FROM documents
              WHERE workspace_id = $1 AND parent_id IS NULL",
        )
        .bind(workspace_id).fetch_one(&mut *tx).await?;
        let key = crate::lexorank::between(last_top.as_deref(), None);
        sqlx::query("UPDATE documents SET parent_id = NULL, sort_key = $2 WHERE id = $1")
            .bind(doc_id).bind(&key).execute(&mut *tx).await?;
    }
}
audit::record_in_tx(&mut tx, workspace_id, Some(actor), "doc.restore", "doc", doc_id).await?;
invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "restore").await?;
tx.commit().await?;
Ok(restored)
```

- [ ] **Step 5: Fix the existing callers** of `archive`/`restore` (server
  handlers ignore the returned ids for now: `Ok(_)`), and the existing test
  `archive_hides_and_restore_brings_back`.
- [ ] **Step 6: Run** the storage tests → all pass.
- [ ] **Step 7: Commit** `feat(storage): archive whole subtrees and restore them as a set (#37)`.

### Task 2: Purge, archive listing and due scan in `DocStore`; blob ids

**Files:**
- Modify: `crates/knot-storage/src/doc_store.rs`, `crates/knot-storage/src/blobs.rs`, `crates/knot-storage/src/lib.rs`
- Test: `crates/knot-storage/tests/documents.rs`

**Interfaces:**
- Produces:
  - `pub struct ArchivedRoot { id, workspace_id, title, parent_id: Option<Uuid>, parent_title: Option<String>, archived_at: DateTime<Utc>, archived_by: Option<Uuid>, archived_by_name: Option<String>, subpage_count: i64 }` (re-exported from `knot_storage`).
  - `DocStore::subtree_ids(doc_id) -> Result<Vec<Uuid>>` — doc plus all descendants, any state.
  - `DocStore::purge(ws, doc, actor: Option<Uuid>) -> Result<Vec<Uuid>>` — deleted ids.
  - `DocStore::list_archive_roots(ws) -> Result<Vec<ArchivedRoot>>` — newest first.
  - `DocStore::due_for_purge(cutoff: DateTime<Utc>, limit: i64) -> Result<Vec<(Uuid /*ws*/, Uuid /*doc*/)>>` — oldest first.
  - `BlobMeta::ids_for_docs(&[Uuid]) -> Result<Vec<Uuid>>`; `BlobMeta::list_for_workspace` returns blobs of live docs only.

- [ ] **Step 1: Failing tests:** `purge_deletes_subtree_and_child_rows` (seed one
  row in `doc_updates`, `document_grants`, `comments`, `share_tokens`,
  `boards`, `doc_tasks`, `blobs`+`blob_bytes`, `doc_markdown_cache`,
  `doc_contributors` for a child; after purge every table has 0 rows for the
  subtree and `audit_events` has `doc.purge`), `purge_live_doc_is_not_archived`,
  `purge_non_root_is_rejected`, `list_archive_roots_reports_roots_only`,
  `due_for_purge_honours_cutoff` (backdate `archived_at` with SQL),
  `blob_ids_for_docs_and_live_listing`.
- [ ] **Step 2: Run** → fail.
- [ ] **Step 3: Implement.** `purge`:

```rust
let mut tx = crate::begin(&self.pool).await?;
let root: Option<Option<Uuid>> = sqlx::query_scalar(
    "SELECT archive_root_id FROM documents WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
)
.bind(workspace_id).bind(doc_id).fetch_optional(&mut *tx).await?;
match root {
    None => return Err(DocStoreError::NotFound),
    Some(None) => return Err(DocStoreError::NotArchived),
    Some(Some(r)) if r != doc_id => return Err(DocStoreError::NotArchiveRoot),
    Some(Some(_)) => {}
}
let ids: Vec<Uuid> = sqlx::query_scalar(SUBTREE_SQL).bind(doc_id).fetch_all(&mut *tx).await?;
sqlx::query("DELETE FROM documents WHERE id = $1").bind(doc_id).execute(&mut *tx).await?;
audit::record_in_tx(&mut tx, workspace_id, actor, "doc.purge", "doc", doc_id).await?;
for id in &ids {
    invalidations::record_in_tx(&mut tx, workspace_id, *id, "purge").await?;
}
tx.commit().await?;
Ok(ids)
```

  with `const SUBTREE_SQL` = recursive CTE from `$1` over `parent_id`
  (no archived filter). `list_archive_roots`:

```sql
SELECT d.id, d.workspace_id, d.title, d.parent_id, p.title, d.archived_at,
       d.archived_by, u.display_name,
       (SELECT count(*) FROM documents s
         WHERE s.archive_root_id = d.id AND s.id <> d.id) AS subpage_count
  FROM documents d
  LEFT JOIN documents p ON p.id = d.parent_id
  LEFT JOIN users u ON u.id = d.archived_by
 WHERE d.workspace_id = $1 AND d.archive_root_id = d.id
 ORDER BY d.archived_at DESC
```

  `due_for_purge`: `SELECT workspace_id, id FROM documents WHERE
  archive_root_id = id AND archived_at < $1 ORDER BY archived_at LIMIT $2`.
  `ids_for_docs`: `SELECT id FROM blobs WHERE doc_id = ANY($1)`.
  `list_for_workspace`: add `AND EXISTS (SELECT 1 FROM documents d WHERE
  d.id = blobs.doc_id AND d.archived_at IS NULL)`.
  `audit::record_in_tx` takes `Option<Uuid>` for the actor already (check
  its signature; it does — `Some(actor)` is passed elsewhere).
- [ ] **Step 4: Run** → pass.
- [ ] **Step 5: Commit** `feat(storage): purge archived subtrees, list archive roots, scan due roots`.

### Task 3: No children under archived parents

**Files:**
- Modify: `crates/knot-storage/src/doc_store.rs` (`create`, `move_to`)
- Test: `crates/knot-storage/tests/documents.rs`

- [ ] **Step 1: Failing tests:** `create_under_archived_parent_is_rejected`,
  `move_into_archived_parent_is_rejected`,
  `create_racing_archive_never_leaves_live_child` (spawn 20 creates under a
  folder's child concurrently with one archive of the folder; afterwards no
  row has `archived_at IS NULL` with an archived parent).
- [ ] **Step 2: Implement** a helper used by both, inside their transaction,
  before the INSERT/UPDATE:

```rust
async fn lock_live_parent(
    tx: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
) -> Result<(), DocStoreError> {
    let Some(parent) = parent_id else { return Ok(()) };
    // FOR SHARE blocks behind an archive's row lock and then re-reads the
    // committed row, so a child cannot slip into a subtree being archived.
    let archived: Option<bool> = sqlx::query_scalar(
        "SELECT archived_at IS NOT NULL FROM documents
          WHERE workspace_id = $1 AND id = $2 FOR SHARE",
    )
    .bind(workspace_id).bind(parent)
    .fetch_optional(&mut *tx).await?;
    if archived == Some(true) {
        return Err(DocStoreError::Archived);
    }
    Ok(())
}
```

  A missing parent keeps today's behaviour (the FK fails on insert).
  `move_to` also refuses to move an archived doc (`Archived`), checked with
  the same `FOR UPDATE` row read it needs anyway.
- [ ] **Step 3: Run** → pass. **Step 4: Commit** `fix(storage): refuse children under archived parents`.

### Task 4: ACL returns the archived flag

**Files:**
- Modify: `crates/knot-docs/src/acl.rs`, `crates/knot-docs/src/cache.rs`, `crates/knot-docs/src/lib.rs`

**Interfaces:**
- Produces in `knot_docs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocAccess {
    pub role: WorkspaceRole,
    pub archived: bool,
}
impl DocAccess {
    /// Archived docs are hidden from viewers.
    pub fn can_read(self) -> bool { !(self.archived && self.role == WorkspaceRole::Viewer) }
    /// Archived docs are read-only for everyone.
    pub fn can_write(self) -> bool {
        !self.archived && matches!(self.role, WorkspaceRole::Owner | WorkspaceRole::Editor)
    }
}
```

  `resolve(...) -> Result<Option<DocAccess>, ResolveError>`;
  `AclCache::access(ws, doc, user) -> Result<Option<DocAccess>, ResolveError>`
  (raw, includes viewers on archived docs);
  `AclCache::effective_role(...)` keeps its signature and returns `None` when
  `!access.can_read()`.

- [ ] **Step 1: Failing tests** in `acl.rs`: `archived_doc_reports_archived`,
  and in `cache.rs`: `effective_role_hides_archived_doc_from_viewers`.
- [ ] **Step 2: Implement**; update existing tests' expectations (`.map(|a| a.role)`).
- [ ] **Step 3: Run** `cargo test -p knot-docs` → pass. **Step 4: Commit** `feat(acl): report whether a doc is archived with the role`.

### Task 5: Read-only enforcement in the server

**Files:**
- Create: `crates/knot-server/src/auth/doc_access.rs` (re-exported from `auth/mod.rs`)
- Modify: `crates/knot-server/src/auth/require_doc_role.rs`, `crates/knot-server/src/lib.rs` (both WS upgrades), `crates/knot-server/src/routes/api/{blobs,boards,shares,tasks}.rs`, `crates/knot-server/src/http_error.rs`, `crates/knot-storage/src/{share_tokens,tasks}.rs`
- Test: `crates/knot-server/tests/archive_integration.rs` (new)

**Interfaces:**
- Produces: `crate::http_error::doc_archived() -> Response` (409
  `doc.archived`); `crate::auth::DocArchived` request extension (unit struct,
  inserted by the middleware when the doc is archived).

- [ ] **Step 1: Failing integration tests** (helpers: `login_state`,
  `invite_user`, `login_as`, `create_doc`, plus a `send(app, method, uri,
  cookie, csrf, body) -> (StatusCode, serde_json::Value)`):
  `editor_can_archive` (regression #37), `viewer_cannot_archive`,
  `viewer_gets_404_on_archived_doc`, `writes_to_archived_doc_are_rejected`
  (table: PATCH `/api/docs/{id}`, POST `/move`, POST `/markdown`, PUT
  `/grants/user:<id>`, POST `/comments`, POST `/template`, POST
  `/history/1/restore`, POST `/api/docs/from-template/{id}`, POST
  `/api/docs/{id}/shares`, PATCH `/api/docs/{id}/tasks/0`, POST
  `/api/docs/{id}/boards` → each 409 `doc.archived`),
  `share_link_to_archived_doc_is_gone`, `my_tasks_skip_archived_docs`.
- [ ] **Step 2: Implement** the middleware rule:

```rust
let access = match acl.access(ctx.workspace_id, doc_id, ctx.user_id).await {
    Ok(Some(a)) => a,
    Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
    Err(e) => { tracing::error!(error=?e, "acl resolve"); return internal() }
};
if !access.can_read() {
    return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
}
if access.archived {
    let route = req.extensions().get::<MatchedPath>().map(|m| m.as_str());
    let exempt = matches!(route, Some(ARCHIVE_RESTORE_ROUTE | ARCHIVE_PURGE_ROUTE));
    if !matches!(*req.method(), Method::GET | Method::HEAD) && !exempt {
        return doc_archived();
    }
    req.extensions_mut().insert(DocArchived);
}
req.extensions_mut().insert(EffectiveDocRole(access.role));
```

  with `pub const ARCHIVE_RESTORE_ROUTE: &str = "/api/docs/{id}/restore";`
  and `ARCHIVE_PURGE_ROUTE = "/api/docs/{id}/purge"`.
  - `collab_upgrade`: `access()`; `!can_read` → 403 `acl.no_grant`;
    `can_write = access.can_write()`.
  - `collab_board_upgrade`: archived → 409 `doc.archived`.
  - `boards::require_doc_role`: `access()`; `!can_read` → 403; editor
    required and archived → 409.
  - `blobs::upload` / `delete_blob`: archived → 409; `download` unchanged
    (uses `effective_role`, which now hides archived docs from viewers).
  - `shares::create`: archived → 409 (list/revoke unchanged).
  - `tasks::patch_checked`: archived → 409.
  - `PgShareTokenStore::find_alive`: add `AND EXISTS (SELECT 1 FROM
    documents d WHERE d.id = share_tokens.doc_id AND d.archived_at IS NULL)`.
  - `PgTaskStore::list_for_assignee`: both queries add `AND EXISTS (SELECT 1
    FROM documents d WHERE d.id = doc_tasks.doc_id AND d.archived_at IS NULL)`.
- [ ] **Step 3: Run** `cargo test -p knot-server --test archive_integration` → pass.
- [ ] **Step 4: Commit** `feat(server): archived docs are read-only for editors and hidden from viewers`.

### Task 6: Archive API, purge endpoint, config

**Files:**
- Create: `crates/knot-server/src/archive.rs` (purge orchestration, shared with Task 7)
- Modify: `crates/knot-config/src/lib.rs`, `crates/knot-server/src/routes/api/docs.rs`, `crates/knot-server/src/routes/api/workspace.rs`, `crates/knot-server/src/lib.rs`
- Test: `crates/knot-server/tests/archive_integration.rs`, `crates/knot-config/src/lib.rs` (unit)

**Interfaces:**
- Produces: `Config::archive_retention_days: u32` (default 30, `>= 1`);
  `Config::archive_retention() -> chrono::Duration`;
  `knot_server::archive::purge_doc(state: &AppState, workspace_id, doc_id,
  actor: Option<Uuid>, trigger: PurgeTrigger) -> Result<(), PurgeError>`;
  `knot_server::archive::evict_local(state, &[Uuid])`.

- [ ] **Step 1: Failing tests:** config `zero_retention_is_rejected`,
  `retention_defaults_to_30`; integration `restore_by_editor`,
  `restore_of_live_doc_is_404`, `restore_of_subpage_is_not_archive_root`,
  `purge_requires_owner`, `purge_of_live_doc_is_not_archived`,
  `purge_removes_doc`, `archive_list_contents_and_gating` (viewer 403),
  `get_archived_doc_reports_archive_fields`, `workspace_reports_retention`.
- [ ] **Step 2: Implement.**
  - `archive` handler: editor+ (`role.0 == Viewer` → 403
    `acl.editor_required`); `DocStoreError::Archived` → 409; on success
    `archive::evict_local(&state, &ids)` then 204.
  - `restore` handler: editor+; `NotArchiveRoot` → 409; evict; 204.
  - `purge` handler (new route `POST /api/docs/{id}/purge`): owner →
    `archive::purge_doc(.., Some(ctx.user_id), PurgeTrigger::Manual)`;
    map `NotArchived`/`NotArchiveRoot` → 409; 204.
  - `list_archive` (new `GET /api/workspace/archive` in `list_routes`):
    `ctx.role == Viewer` → 403 `acl.editor_required`; JSON array of
    `{id, title, parent_id, parent_title, subpage_count, archived_at,
    archived_by: {id, display_name} | null, purge_at}`.
  - `get_one`: when archived, add `archived_at`, `archived_by`
    (`{id, display_name}` via `UserStore::get`), `purge_at`,
    `archive_root_id`, `archive_root_title`; all `null` otherwise.
  - `get_workspace`: add `archive_retention_days`.
  - `archive.rs`:

```rust
pub enum PurgeTrigger { Retention, Manual }
pub enum PurgeError { NotFound, NotArchived, NotArchiveRoot, Blob(String), Internal(String) }

pub async fn purge_doc(state: &AppState, workspace_id: Uuid, doc_id: Uuid,
                       actor: Option<Uuid>, trigger: PurgeTrigger) -> Result<(), PurgeError> {
    let docs = state.docs.clone().ok_or_else(|| PurgeError::Internal("docs".into()))?;
    let blob_meta = state.blob_meta.clone().ok_or_else(|| PurgeError::Internal("blob_meta".into()))?;
    let blob_store = state.blob_store.clone().ok_or_else(|| PurgeError::Internal("blob_store".into()))?;
    // Pre-check so a live doc never loses its attachments.
    match docs.get(doc_id).await { /* NotFound / NotArchived / NotArchiveRoot */ }
    let subtree = docs.subtree_ids(doc_id).await?;
    for blob_id in blob_meta.ids_for_docs(&subtree).await? {
        match blob_store.delete(blob_id).await {
            Ok(()) | Err(BlobStoreError::NotFound) => {}
            Err(e) => return Err(PurgeError::Blob(e.to_string())), // DB untouched; retried next run
        }
    }
    let deleted = docs.purge(workspace_id, doc_id, actor).await?;
    evict_local(state, &deleted).await;
    if let Some(rooms) = state.rooms_v2.clone() {
        for id in &deleted { rooms.revoke_all_for_doc(*id).await; rooms.evict(*id).await; }
    }
    metrics::counter!("knot_docs_purged_total", "trigger" => trigger.label()).increment(1);
    Ok(())
}
```

- [ ] **Step 3: Run** → pass. **Step 4: Commit** `feat(api): archive list, editor restore, owner purge, retention setting`.

### Task 7: Purge job

**Files:**
- Modify: `crates/knot-server/src/archive.rs`, `crates/knot-server/src/main.rs`, `deploy/helm/knot/{values.yaml,values.schema.json,templates/configmap.yaml,README.md}`, `README.md` (config table) or the existing config docs
- Test: `crates/knot-server/tests/archive_integration.rs`

**Interfaces:**
- Produces: `archive::run_purge_once(state: &AppState, retention: chrono::Duration) -> Result<PurgeRun, sqlx::Error>`
  with `pub enum PurgeRun { Skipped, Done { purged: usize, failed: usize } }`;
  `archive::spawn_purge_job(state: AppState, retention: chrono::Duration)`.

- [ ] **Step 1: Failing tests:** `purge_job_removes_due_roots_only`,
  `purge_keeps_rows_when_blob_delete_fails` (a `FailingBlobStore` whose
  `delete` returns `Backend`), `purge_job_skips_when_lock_is_held` (hold
  `pg_advisory_lock(PURGE_LOCK_KEY)` on another connection).
- [ ] **Step 2: Implement:**

```rust
pub const PURGE_LOCK_KEY: i64 = 0x6b6e_6f74_7075_7267; // "knotpurg"
const PURGE_BATCH: i64 = 100;
const PURGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

pub async fn run_purge_once(state: &AppState, retention: chrono::Duration) -> Result<PurgeRun, sqlx::Error> {
    let pool = state.pool.clone().expect("pool");
    let mut lock_conn = pool.acquire().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PURGE_LOCK_KEY).fetch_one(&mut *lock_conn).await?;
    if !got { return Ok(PurgeRun::Skipped); }
    let result = purge_due(state, retention).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(PURGE_LOCK_KEY)
        .execute(&mut *lock_conn).await;
    result
}
```

  `purge_due` loops `due_for_purge(now - retention, PURGE_BATCH)`, calls
  `purge_doc(.., None, PurgeTrigger::Retention)` per root, counts failures
  (`knot_docs_purge_failures_total`), and stops when a batch returns nothing
  new or every remaining root in the batch failed (so a stuck root can't spin
  the loop). `spawn_purge_job` sleeps `PURGE_INTERVAL` first, then runs.
  `main.rs`: spawn after the state is built when `state.pool` is set.
  Helm: `archive.retentionDays: 30` → `KNOT_ARCHIVE_RETENTION_DAYS`; schema
  `integer`, `minimum: 1`; README row and backup note.
- [ ] **Step 3: Run** → pass. **Step 4: Commit** `feat(server): hourly purge of archive roots past retention`.

### Task 8: Web — API, sidebar, context-menu keyboard fix

**Files:**
- Modify: `web/src/lib/validators.ts`, `web/src/features/docs/docs.api.ts`, `web/src/features/docs/DocTree.tsx`, `web/src/components/ContextMenu.tsx`, `web/src/features/workspace/WorkspaceHeader.tsx`, `web/src/routes.tsx`
- Create: `web/src/features/docs/archive.ts` (pure helpers: `archiveConfirmText`, `archiveErrorText`, `daysUntil`)
- Test: `web/src/features/docs/archive.test.ts`, `web/src/components/ContextMenu.test.tsx`, `web/src/features/docs/DocTree.archive.test.tsx`

**Interfaces:**
- Produces: `Doc` gains `archived_at: string | null`, `archive_root_id: string | null` (both `v.fallback(v.nullable(v.string()), null)`);
  `DocWithRole` additionally `archived_by: {id, display_name} | null`, `purge_at: string | null`, `archive_root_title: string | null`;
  `Workspace.archive_retention_days: number` (fallback 30);
  `ArchiveEntry` schema; `docsApi.purge(id)`, `docsApi.listArchive()`.

- [ ] **Step 1: Failing tests:** `archiveConfirmText("Folder", 2, 30)` →
  `Move "Folder" to the archive?\n\nIts 2 subpages go with it. It will be permanently deleted in 30 days unless restored.`;
  singular "subpage" for 1; no subpage sentence for 0. `archiveErrorText`
  maps `acl.*`, `doc.archived`, default. ContextMenu: `fireEvent.keyDown(item, {key: "Enter", code: "Enter"})`
  inside a parent with an `onKeyDown` spy → parent spy not called. DocTree:
  editor sees `ctx-delete` labelled "Move to archive"; viewer sees no menu.
- [ ] **Step 2: Implement.** ContextMenu root gets
  `onKeyDown={(e) => e.stopPropagation()}` and
  `onPointerDown={(e) => e.stopPropagation()}`. DocTree `onArchive` uses
  `archiveConfirmText(node.title, descendantIds(docs, node.id).size, retentionDays)`;
  on success `notify("info", "Moved to archive")`, invalidate `["docs"]`,
  `["archive"]`, `["doc", activeId]`. WorkspaceHeader link `sidebar-archive`
  (icon `Archive`) for editor+. Route `archive` → lazy `ArchivePage`.
- [ ] **Step 3: Run** `cd web && NODE_OPTIONS=--no-experimental-webstorage ./node_modules/.bin/vitest run` → pass; `npx tsc -b` (or the repo's typecheck script) → clean.
- [ ] **Step 4: Commit** `feat(web): move to archive from the sidebar; menu works by keyboard`.

### Task 9: Web — Archive page

**Files:**
- Create: `web/src/features/docs/ArchivePage.tsx`, `web/src/features/docs/ArchivePage.test.tsx`

- [ ] **Step 1: Failing tests:** renders a row with title, "+2 subpages",
  "Deleted by Anna", "Deleted permanently in N days"; editor sees Restore
  but not Delete permanently; owner sees both; empty state text
  "Nothing in the archive."; Restore calls `docsApi.restore` and invalidates.
- [ ] **Step 2: Implement** following `TemplatesPage` layout (`mx-auto
  max-w-[760px] px-6 py-8`), `Button` for actions, `window.confirm` for
  purge: `Permanently delete "X" and its N subpages? This can't be undone.`
- [ ] **Step 3: Run** → pass. **Step 4: Commit** `feat(web): archive page`.

### Task 10: Web — archived doc view

**Files:**
- Create: `web/src/features/docs/ArchivedBanner.tsx`, `web/src/features/docs/DocPage.archived.test.tsx`
- Modify: `web/src/features/docs/DocPage.tsx`

- [ ] **Step 1: Failing tests:** archived root → banner with Restore (editor),
  plus Delete permanently (owner); title read-only; no edit toggle, share,
  comments, history, import, template buttons. Descendant → "Part of …"
  banner with link, no actions. Status `unauthorised` + refetch returns an
  archived doc → no navigation.
- [ ] **Step 2: Implement.** `archived = Boolean(meta.archived_at)`;
  `KnotEditor key={archived ? "archived" : "live"} role={archived ? "viewer" : meta.effective_role} editMode={archived ? false : editMode}`;
  hide the listed buttons when archived; `useEffect` on `status ===
  "unauthorised"` → `const fresh = await doc.refetch()`; archived → return;
  else notify + navigate as today.
- [ ] **Step 3: Run** → pass. **Step 4: Commit** `feat(web): read-only archived view with restore`.

### Task 11: E2E flow

**Files:**
- Create: `e2e/flows/archive.spec.ts`

- [ ] **Step 1:** Write the flow from spec §8 (folder + two subpages via API;
  archive via sidebar menu; sidebar and search no longer show them; `/archive`
  lists the folder with "+2 subpages"; open → banner, title read-only;
  Restore → tree back; archive again; Delete permanently → `GET
  /api/docs/{id}` 404).
- [ ] **Step 2: Run** against a throwaway Postgres + local server + Vite
  (see the repo's e2e README) → pass.
- [ ] **Step 3: Commit** `test(e2e): archive, preview, restore and purge`.

### Task 12: Whole-branch verification

- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `KNOT_TEST_DATABASE_URL=… cargo test --workspace`
- [ ] `cd web && npx --yes pnpm@11.22.0 lint && npx --yes pnpm@11.22.0 typecheck` (or the scripts `package.json` defines) and the full Vitest run
- [ ] `helm lint deploy/helm/knot` and `helm template` with defaults
- [ ] CHANGELOG entry under Unreleased.
