//! Document storage — CRUD + tree ops.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgPool};
use thiserror::Error;
use uuid::Uuid;

use crate::audit;
use crate::invalidations;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub sort_key: String,
    pub icon: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
    /// When true, this doc is a template — excluded from the main tree
    /// listing and surfaced in the "New document" gallery instead.
    /// See Plan 36 (docs/superpowers/plans/2026-06-04-doc-templates.md).
    pub is_template: bool,
    /// Who moved this doc (or the folder it went with) to the archive.
    pub archived_by: Option<Uuid>,
    /// The doc the user archived; equals `id` on that doc and points at it
    /// from every descendant archived with it. Set iff `archived_at` is.
    pub archive_root_id: Option<Uuid>,
}

/// One entry of the archive list: a doc someone archived, with what the
/// archive page shows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedRoot {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub title: String,
    /// Where it lived. The parent may itself be archived.
    pub parent_id: Option<Uuid>,
    pub parent_title: Option<String>,
    pub archived_at: DateTime<Utc>,
    pub archived_by: Option<Uuid>,
    pub archived_by_name: Option<String>,
    /// Descendants archived together with it.
    pub subpage_count: i64,
    /// A purge has started and is not finished; it can't be restored.
    pub purging: bool,
}

#[derive(Debug, Error)]
pub enum DocStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("cycle")]
    Cycle,
    /// The doc, or the parent it would go under, is in the archive.
    #[error("archived")]
    Archived,
    /// Purge needs a doc that is in the archive.
    #[error("not archived")]
    NotArchived,
    /// Restore and purge act on the doc the user archived, not on a
    /// descendant that went into the archive with it.
    #[error("not an archive root")]
    NotArchiveRoot,
    /// A purge of this doc has started; it can no longer be restored.
    #[error("purging")]
    Purging,
}

#[async_trait]
pub trait DocStore: Send + Sync + 'static {
    async fn list_alive(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError>;
    async fn get(&self, doc_id: Uuid) -> Result<Option<Document>, DocStoreError>;
    async fn create(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
        title: &str,
        sort_key: &str,
        created_by: Uuid,
    ) -> Result<Document, DocStoreError>;
    async fn rename(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        title: &str,
        icon: Option<&str>,
    ) -> Result<Document, DocStoreError>;
    async fn move_to(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        parent_id: Option<Uuid>,
        sort_key: &str,
    ) -> Result<Document, DocStoreError>;
    /// Move the doc and its live subtree to the archive. Returns the ids
    /// stamped, the doc first. Descendants already archived on their own
    /// keep their own archive entry and clock.
    async fn archive(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<Vec<Uuid>, DocStoreError>;
    /// Bring an archive root and everything archived with it back. Returns
    /// the ids restored. If the root's parent is still archived, the root
    /// goes to the top level instead.
    async fn restore(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<Vec<Uuid>, DocStoreError>;
    /// Returns siblings under `parent_id` in sort order, archived ones
    /// included -- they still hold their sort_key, and the UNIQUE index over
    /// (workspace_id, parent_id, sort_key) still counts them.
    async fn siblings(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
    ) -> Result<Vec<Document>, DocStoreError>;
    /// Returns the IDs of all descendants (children, grandchildren, ...) of
    /// `doc_id` within the given workspace. Excludes the doc itself.
    async fn descendant_ids(&self, doc_id: Uuid) -> Result<Vec<Uuid>, DocStoreError>;
    /// Everything a purge of `doc_id` deletes: the doc, every row archived
    /// with it, and all their descendants, archived or not.
    async fn subtree_ids(&self, doc_id: Uuid) -> Result<Vec<Uuid>, DocStoreError>;
    /// Fence a purge before any blob bytes are deleted outside the
    /// database: marks the archive root and every archive root inside it so
    /// restore refuses them and `due_for_purge` returns them until the purge
    /// completes. Committed on its own, ahead of the deletes.
    async fn begin_purge(&self, workspace_id: Uuid, doc_id: Uuid) -> Result<(), DocStoreError>;
    /// Permanently delete an archive root and its subtree; the foreign-key
    /// cascades take every row that hangs off them. Returns the ids deleted.
    /// `actor` is `None` when the retention job purges.
    async fn purge(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<Vec<Uuid>, DocStoreError>;
    /// The workspace's archive roots, newest first.
    async fn list_archive_roots(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<ArchivedRoot>, DocStoreError>;
    /// Archive roots archived before `cutoff`, or whose purge has started,
    /// oldest first, across all workspaces, as `(workspace_id, doc_id)`.
    async fn due_for_purge(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<(Uuid, Uuid)>, DocStoreError>;
    /// Documents in the workspace flagged as templates. Returned in
    /// title order. Used to populate the "New document" gallery.
    async fn list_templates(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError>;
    /// Flip the `is_template` flag on a doc. Auditable; owners only at
    /// the route layer.
    async fn set_template(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        is_template: bool,
    ) -> Result<Document, DocStoreError>;
}

#[derive(Clone)]
pub struct PgDocStore {
    pool: PgPool,
}

impl PgDocStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

type DocRow = (
    Uuid,
    Uuid,
    Option<Uuid>,
    String,
    String,
    Option<String>,
    Uuid,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    bool,
    Option<Uuid>,
    Option<Uuid>,
);
fn doc_from_row(r: DocRow) -> Document {
    Document {
        id: r.0,
        workspace_id: r.1,
        parent_id: r.2,
        title: r.3,
        sort_key: r.4,
        icon: r.5,
        created_by: r.6,
        created_at: r.7,
        updated_at: r.8,
        archived_at: r.9,
        is_template: r.10,
        archived_by: r.11,
        archive_root_id: r.12,
    }
}
/// Shared SELECT column list, spliced into query strings with `format!`.
///
/// sqlx 0.9 only accepts `&'static str` directly, so every query below wraps
/// its `format!` in `AssertSqlSafe`. That assertion holds because the only
/// values interpolated into those strings are compile-time constants (this
/// and `SUBTREE_IDS_SQL`), never caller-supplied, and every runtime value
/// remains a `$N` bind. Keep it that way: interpolating anything else here
/// would defeat the check.
const COLS: &str = "id, workspace_id, parent_id, title, sort_key, icon, created_by, created_at, updated_at, archived_at, is_template, archived_by, archive_root_id";

#[async_trait]
impl DocStore for PgDocStore {
    async fn list_alive(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError> {
        // Templates live in their own gallery (see list_templates) and
        // are intentionally excluded from the main doc-tree listing. Each
        // level comes in the sidebar's order (by code unit, whatever the
        // database's collation), with `id` breaking ties.
        let rows = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents
             WHERE workspace_id = $1 AND archived_at IS NULL AND NOT is_template
             ORDER BY parent_id NULLS FIRST, sort_key COLLATE \"C\", id"
        )))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(doc_from_row).collect())
    }

    async fn list_templates(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError> {
        let rows = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents
             WHERE workspace_id = $1 AND archived_at IS NULL AND is_template
             ORDER BY title"
        )))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(doc_from_row).collect())
    }

    async fn set_template(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        is_template: bool,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "UPDATE documents SET is_template = $3, updated_at = now()
             WHERE workspace_id = $1 AND id = $2
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(is_template)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DocStoreError::NotFound)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            if is_template {
                "doc.template.mark"
            } else {
                "doc.template.unmark"
            },
            "doc",
            doc.id,
        )
        .await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn get(&self, doc_id: Uuid) -> Result<Option<Document>, DocStoreError> {
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents WHERE id = $1"
        )))
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(doc_from_row))
    }

    async fn create(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
        title: &str,
        sort_key: &str,
        created_by: Uuid,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        lock_live_parent(&mut tx, workspace_id, parent_id).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(parent_id)
        .bind(title)
        .bind(sort_key)
        .bind(created_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_unique)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(created_by),
            "doc.create",
            "doc",
            doc.id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc.id, "create").await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn rename(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        title: &str,
        icon: Option<&str>,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "UPDATE documents SET title = $3, icon = COALESCE($4, icon), updated_at = now()
             WHERE workspace_id = $1 AND id = $2
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(title)
        .bind(icon)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DocStoreError::NotFound)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.rename",
            "doc",
            doc.id,
        )
        .await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn move_to(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        parent_id: Option<Uuid>,
        sort_key: &str,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        // Reject moves that would create a parent cycle: the destination
        // parent must not be the doc itself or one of its descendants. Done
        // inside the tx so it's correct under concurrency.
        if let Some(p) = parent_id {
            let creates_cycle: bool = sqlx::query_scalar(
                "WITH RECURSIVE sub AS (
                     SELECT id FROM documents WHERE id = $1
                     UNION ALL
                     SELECT d.id FROM documents d JOIN sub s ON d.parent_id = s.id
                 )
                 SELECT EXISTS(SELECT 1 FROM sub WHERE id = $2)",
            )
            .bind(doc_id)
            .bind(p)
            .fetch_one(&mut *tx)
            .await?;
            if creates_cycle {
                return Err(DocStoreError::Cycle);
            }
        }
        // Archived docs stay where they are until restored.
        let archived: Option<bool> = sqlx::query_scalar(
            "SELECT archived_at IS NOT NULL FROM documents
             WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .fetch_optional(&mut *tx)
        .await?;
        match archived {
            None => return Err(DocStoreError::NotFound),
            Some(true) => return Err(DocStoreError::Archived),
            Some(false) => {}
        }
        lock_live_parent(&mut tx, workspace_id, parent_id).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "UPDATE documents SET parent_id = $3, sort_key = $4, updated_at = now()
             WHERE workspace_id = $1 AND id = $2
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(parent_id)
        .bind(sort_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_unique)?
        .ok_or(DocStoreError::NotFound)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.move",
            "doc",
            doc.id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc.id, "tree-move").await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn archive(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<Vec<Uuid>, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let archived: Option<bool> = sqlx::query_scalar(
            "SELECT archived_at IS NOT NULL FROM documents
             WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .fetch_optional(&mut *tx)
        .await?;
        match archived {
            None => return Err(DocStoreError::NotFound),
            Some(true) => return Err(DocStoreError::Archived),
            Some(false) => {}
        }
        // Stamp the live subtree, descending through rows this archive has
        // already stamped but not into subtrees archived on their own.
        // Repeat until a pass stamps nothing: a create that held its parent
        // `FOR SHARE` can commit a child after one UPDATE took its snapshot;
        // the next statement sees it. Creates that start later block on the
        // stamped parent's row lock and then find it archived.
        let mut stamped: Vec<Uuid> = Vec::new();
        loop {
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "WITH RECURSIVE sub AS (
                     SELECT id FROM documents WHERE id = $2
                     UNION ALL
                     SELECT d.id FROM documents d JOIN sub s ON d.parent_id = s.id
                      WHERE d.archived_at IS NULL OR d.archive_root_id = $2
                 )
                 UPDATE documents
                    SET archived_at = now(), archived_by = $3,
                        archive_root_id = $2, updated_at = now()
                  WHERE workspace_id = $1 AND archived_at IS NULL
                    AND id IN (SELECT id FROM sub)
                 RETURNING id",
            )
            .bind(workspace_id)
            .bind(doc_id)
            .bind(actor)
            .fetch_all(&mut *tx)
            .await?;
            if ids.is_empty() {
                break;
            }
            stamped.extend(ids);
        }
        // The archived doc first, for callers that report it.
        stamped.sort_by_key(|id| *id != doc_id);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.archive",
            "doc",
            doc_id,
        )
        .await?;
        // One invalidation for the root: the ACL listener walks descendants
        // and evicts (and revokes live sockets on) each of them.
        invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "archive").await?;
        tx.commit().await?;
        Ok(stamped)
    }

    async fn restore(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<Vec<Uuid>, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        // Lock the parent before the doc, the order archive uses. A
        // concurrent archive of the parent then either commits first (and
        // the doc goes to the top level below) or waits for this restore
        // (and stamps the restored doc along with the parent). Reading the
        // parent without the lock could leave the doc live under an
        // archived parent, where purging the parent would delete it.
        let expected_parent: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT parent_id FROM documents WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(expected_parent) = expected_parent else {
            return Err(DocStoreError::NotFound);
        };
        let parent_archived = match expected_parent {
            Some(parent_id) => sqlx::query_scalar::<_, bool>(
                "SELECT archived_at IS NOT NULL FROM documents WHERE id = $1 FOR SHARE",
            )
            .bind(parent_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false),
            None => false,
        };
        let row: Option<(Option<Uuid>, Option<Uuid>, bool)> = sqlx::query_as(
            "SELECT archive_root_id, parent_id, purge_requested_at IS NOT NULL FROM documents
             WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            None | Some((None, _, _)) => return Err(DocStoreError::NotFound),
            Some((Some(root), _, _)) if root != doc_id => {
                return Err(DocStoreError::NotArchiveRoot);
            }
            // Its attachments may already be partly gone.
            Some((_, _, true)) => return Err(DocStoreError::Purging),
            // Archived docs can't move, so this only trips if the doc was
            // restored, moved and archived again between the two reads.
            Some((Some(_), parent_id, _)) if parent_id != expected_parent => {
                return Err(DocStoreError::Conflict);
            }
            Some(_) => {}
        }
        let restored: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE documents
                SET archived_at = NULL, archived_by = NULL,
                    archive_root_id = NULL, updated_at = now()
              WHERE workspace_id = $1 AND archive_root_id = $2
             RETURNING id",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .fetch_all(&mut *tx)
        .await?;
        // Restoring never resurrects an archived ancestor: under one, the
        // doc would be invisible, so it moves to the end of the top level.
        if parent_archived {
            let last_top_level_key: Option<String> = sqlx::query_scalar(
                "SELECT max(sort_key) FROM documents
                 WHERE workspace_id = $1 AND parent_id IS NULL",
            )
            .bind(workspace_id)
            .fetch_one(&mut *tx)
            .await?;
            let sort_key = crate::sort_key_between(last_top_level_key.as_deref(), None);
            sqlx::query("UPDATE documents SET parent_id = NULL, sort_key = $2 WHERE id = $1")
                .bind(doc_id)
                .bind(&sort_key)
                .execute(&mut *tx)
                .await?;
        }
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.restore",
            "doc",
            doc_id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "restore").await?;
        tx.commit().await?;
        Ok(restored)
    }

    async fn siblings(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
    ) -> Result<Vec<Document>, DocStoreError> {
        // Archived rows are included on purpose: they keep their sort_key and
        // still count towards UNIQUE (workspace_id, parent_id, sort_key), so a
        // caller generating the next key must see them or it can regenerate a
        // key an archived sibling still holds. In the sidebar's order, as
        // list_alive.
        let rows = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents
             WHERE workspace_id = $1 AND parent_id IS NOT DISTINCT FROM $2
             ORDER BY sort_key COLLATE \"C\", id"
        )))
        .bind(workspace_id)
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(doc_from_row).collect())
    }

    async fn descendant_ids(&self, doc_id: Uuid) -> Result<Vec<Uuid>, DocStoreError> {
        // Recursive CTE descending from doc_id.
        let rows = sqlx::query_scalar::<_, Uuid>(
            "WITH RECURSIVE chain AS (
                 SELECT id FROM documents WHERE parent_id = $1
                 UNION ALL
                 SELECT d.id FROM documents d JOIN chain c ON d.parent_id = c.id
             )
             SELECT id FROM chain",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn subtree_ids(&self, doc_id: Uuid) -> Result<Vec<Uuid>, DocStoreError> {
        Ok(sqlx::query_scalar(SUBTREE_IDS_SQL)
            .bind(doc_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn begin_purge(&self, workspace_id: Uuid, doc_id: Uuid) -> Result<(), DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        lock_archive_root(&mut tx, workspace_id, doc_id).await?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE documents SET purge_requested_at = COALESCE(purge_requested_at, now())
              WHERE archive_root_id = id AND id IN ({SUBTREE_IDS_SQL})"
        )))
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn purge(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<Vec<Uuid>, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        lock_archive_root(&mut tx, workspace_id, doc_id).await?;
        let deleted: Vec<Uuid> = sqlx::query_scalar(SUBTREE_IDS_SQL)
            .bind(doc_id)
            .fetch_all(&mut *tx)
            .await?;
        // The parent_id cascade takes the subtrees.
        sqlx::query("DELETE FROM documents WHERE id = $1 OR archive_root_id = $1")
            .bind(doc_id)
            .execute(&mut *tx)
            .await?;
        audit::record_in_tx(&mut tx, workspace_id, actor, "doc.purge", "doc", doc_id).await?;
        // One per deleted doc: once the rows are gone the ACL listener can no
        // longer find descendants to evict from the root's id alone.
        for id in &deleted {
            invalidations::record_in_tx(&mut tx, workspace_id, *id, "purge").await?;
        }
        tx.commit().await?;
        Ok(deleted)
    }

    async fn list_archive_roots(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<ArchivedRoot>, DocStoreError> {
        type Row = (
            Uuid,
            Uuid,
            String,
            Option<Uuid>,
            Option<String>,
            DateTime<Utc>,
            Option<Uuid>,
            Option<String>,
            i64,
            bool,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT d.id, d.workspace_id, d.title, d.parent_id, p.title, d.archived_at,
                    d.archived_by, u.display_name,
                    (SELECT count(*) FROM documents s
                      WHERE s.archive_root_id = d.id AND s.id <> d.id),
                    d.purge_requested_at IS NOT NULL
               FROM documents d
               LEFT JOIN documents p ON p.id = d.parent_id
               LEFT JOIN users u ON u.id = d.archived_by
              WHERE d.workspace_id = $1 AND d.archive_root_id = d.id
              ORDER BY d.archived_at DESC",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ArchivedRoot {
                id: r.0,
                workspace_id: r.1,
                title: r.2,
                parent_id: r.3,
                parent_title: r.4,
                archived_at: r.5,
                archived_by: r.6,
                archived_by_name: r.7,
                subpage_count: r.8,
                purging: r.9,
            })
            .collect())
    }

    async fn due_for_purge(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<(Uuid, Uuid)>, DocStoreError> {
        Ok(sqlx::query_as(
            "SELECT workspace_id, id FROM documents
              WHERE archive_root_id = id
                AND (archived_at < $1 OR purge_requested_at IS NOT NULL)
              ORDER BY archived_at
              LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// Lock `doc_id` for a purge step; fails unless it is an archive root.
async fn lock_archive_root(
    tx: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    doc_id: Uuid,
) -> Result<(), DocStoreError> {
    let archive_root_id: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT archive_root_id FROM documents
         WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(doc_id)
    .fetch_optional(&mut *tx)
    .await?;
    match archive_root_id {
        None => Err(DocStoreError::NotFound),
        Some(None) => Err(DocStoreError::NotArchived),
        Some(Some(root)) if root != doc_id => Err(DocStoreError::NotArchiveRoot),
        Some(Some(_)) => Ok(()),
    }
}

/// Fails with `Archived` when the parent a doc is about to go under is in
/// the archive. `FOR SHARE` blocks behind an archive's row lock on that
/// parent and then re-reads the committed row, so a child cannot slip into
/// a subtree while it is being archived. A missing parent is left to the
/// foreign key, as before.
async fn lock_live_parent(
    tx: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
) -> Result<(), DocStoreError> {
    let Some(parent_id) = parent_id else {
        return Ok(());
    };
    let archived: Option<bool> = sqlx::query_scalar(
        "SELECT archived_at IS NOT NULL FROM documents
         WHERE workspace_id = $1 AND id = $2 FOR SHARE",
    )
    .bind(workspace_id)
    .bind(parent_id)
    .fetch_optional(&mut *tx)
    .await?;
    if archived == Some(true) {
        return Err(DocStoreError::Archived);
    }
    Ok(())
}

/// Everything a purge of `$1` deletes: `$1`, every row archived with it
/// (normally all inside its subtree, but a move racing the archive can
/// leave one outside), and all their descendants, whatever their state.
const SUBTREE_IDS_SQL: &str = "WITH RECURSIVE sub AS (
         SELECT id FROM documents WHERE id = $1 OR archive_root_id = $1
         UNION
         SELECT d.id FROM documents d JOIN sub s ON d.parent_id = s.id
     )
     SELECT id FROM sub";

fn map_unique(e: sqlx::Error) -> DocStoreError {
    match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => DocStoreError::Conflict,
        e => DocStoreError::Sqlx(e),
    }
}
