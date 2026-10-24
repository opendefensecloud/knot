//! doc_markdown_cache: lazy-fill on export, invalidated by seq drift. Each
//! row also records the version of the renderer that wrote it, so a newer
//! renderer can find and refresh the rows an older one wrote.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

/// The renderer version of a row that [`MarkdownCacheStore::put`], an old
/// release or [`MarkdownCacheStore::mark_stale`] wrote: what the row's
/// trigger gives a write that doesn't name a version. Every renderer since
/// is newer, so the refresh sweep renders such a row again.
pub const STALE_RENDERER_VERSION: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownCacheEntry {
    pub doc_id: Uuid,
    pub rendered_at_seq: i64,
    pub markdown_text: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum MarkdownCacheError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[async_trait]
pub trait MarkdownCacheStore: Send + Sync + 'static {
    /// Return the cached entry if its rendered_at_seq matches `current_seq`,
    /// else None. The room actor passes its `last_applied_seq`.
    async fn get_if_fresh(
        &self,
        doc_id: Uuid,
        current_seq: i64,
    ) -> Result<Option<MarkdownCacheEntry>, MarkdownCacheError>;

    /// Return the most recent cached entry for a doc regardless of seq.
    /// Used by the public share-render endpoint where any cached version is
    /// better than nothing.
    async fn get(&self, doc_id: Uuid) -> Result<Option<MarkdownCacheEntry>, MarkdownCacheError>;

    /// Write a render unconditionally. The row is marked as written by
    /// renderer version 1 ([`STALE_RENDERER_VERSION`]), so the refresh sweep
    /// renders it again: this is what releases before `renderer_version`
    /// wrote.
    async fn put(
        &self,
        doc_id: Uuid,
        rendered_at_seq: i64,
        markdown: &str,
    ) -> Result<(), MarkdownCacheError>;

    /// Write a render by `renderer_version`, unless the row already holds a
    /// render of a later seq. Returns whether a row was written; when it
    /// wasn't, this render is older than the cached one, and the caller
    /// skips the task reindex too.
    async fn put_rendered(
        &self,
        doc_id: Uuid,
        rendered_at_seq: i64,
        markdown: &str,
        renderer_version: i32,
    ) -> Result<bool, MarkdownCacheError>;

    /// Live pages whose cached Markdown is missing or was written by a
    /// renderer older than `current`, in id order after `after`, at most
    /// `limit` of them.
    async fn stale_ids(
        &self,
        current: i32,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Uuid>, MarkdownCacheError>;

    /// Mark the page's cached render as an old renderer's
    /// ([`STALE_RENDERER_VERSION`]), so the refresh sweep renders it and
    /// reindexes its tasks again: for a refresh whose task write failed
    /// after [`put_rendered`](Self::put_rendered) marked the row current.
    /// The render, its seq and `updated_at` stay. A page without a row is
    /// stale already and is left as it is.
    async fn mark_stale(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError>;

    /// Invalidate (delete) the cached row for a doc.
    async fn invalidate(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError>;
}

#[derive(Clone)]
pub struct PgMarkdownCache {
    pool: PgPool,
}

impl PgMarkdownCache {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MarkdownCacheStore for PgMarkdownCache {
    async fn get_if_fresh(
        &self,
        doc_id: Uuid,
        current_seq: i64,
    ) -> Result<Option<MarkdownCacheEntry>, MarkdownCacheError> {
        let row = sqlx::query_as::<_, (Uuid, i64, String, DateTime<Utc>)>(
            "SELECT doc_id, rendered_at_seq, markdown_text, updated_at
             FROM doc_markdown_cache
             WHERE doc_id = $1 AND rendered_at_seq = $2",
        )
        .bind(doc_id)
        .bind(current_seq)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| MarkdownCacheEntry {
            doc_id: r.0,
            rendered_at_seq: r.1,
            markdown_text: r.2,
            updated_at: r.3,
        }))
    }

    async fn get(&self, doc_id: Uuid) -> Result<Option<MarkdownCacheEntry>, MarkdownCacheError> {
        let row = sqlx::query_as::<_, (Uuid, i64, String, DateTime<Utc>)>(
            "SELECT doc_id, rendered_at_seq, markdown_text, updated_at
             FROM doc_markdown_cache
             WHERE doc_id = $1",
        )
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| MarkdownCacheEntry {
            doc_id: r.0,
            rendered_at_seq: r.1,
            markdown_text: r.2,
            updated_at: r.3,
        }))
    }

    async fn put(
        &self,
        doc_id: Uuid,
        rendered_at_seq: i64,
        markdown: &str,
    ) -> Result<(), MarkdownCacheError> {
        sqlx::query(
            "INSERT INTO doc_markdown_cache (doc_id, rendered_at_seq, markdown_text)
             VALUES ($1, $2, $3)
             ON CONFLICT (doc_id) DO UPDATE
             SET rendered_at_seq = EXCLUDED.rendered_at_seq,
                 markdown_text = EXCLUDED.markdown_text,
                 updated_at = now()",
        )
        .bind(doc_id)
        .bind(rendered_at_seq)
        .bind(markdown)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn put_rendered(
        &self,
        doc_id: Uuid,
        rendered_at_seq: i64,
        markdown: &str,
        renderer_version: i32,
    ) -> Result<bool, MarkdownCacheError> {
        let mut tx = crate::begin(&self.pool).await?;
        // The row's trigger stamps this version on the insert and on the
        // update alike; it lasts only until the transaction ends.
        sqlx::query("SELECT set_config('knot.renderer_version', $1::text, true)")
            .bind(renderer_version)
            .execute(&mut *tx)
            .await?;
        let written = sqlx::query(
            "INSERT INTO doc_markdown_cache (doc_id, rendered_at_seq, markdown_text)
             VALUES ($1, $2, $3)
             ON CONFLICT (doc_id) DO UPDATE
             SET rendered_at_seq = EXCLUDED.rendered_at_seq,
                 markdown_text = EXCLUDED.markdown_text,
                 updated_at = now()
             WHERE doc_markdown_cache.rendered_at_seq <= EXCLUDED.rendered_at_seq",
        )
        .bind(doc_id)
        .bind(rendered_at_seq)
        .bind(markdown)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(written)
    }

    async fn stale_ids(
        &self,
        current: i32,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Uuid>, MarkdownCacheError> {
        // Walks documents_pkey and probes the cache by its primary key, so
        // it needs no index of its own.
        //
        // Not a prepared statement: Postgres may switch a prepared one to a
        // generic plan after five runs, and in that plan `$2 IS NULL OR
        // d.id > $2` is no index condition, so every tick scans the pkey
        // from the lowest id (15.8 ms against 4.8 ms per tick at 50k
        // pages). An unnamed statement is planned with the cursor's value
        // each time, so the scan starts at the cursor.
        let ids = sqlx::query_scalar(
            "SELECT d.id
             FROM documents d
             LEFT JOIN doc_markdown_cache c ON c.doc_id = d.id
             WHERE d.archived_at IS NULL
               AND (c.doc_id IS NULL OR c.renderer_version < $1)
               AND ($2::uuid IS NULL OR d.id > $2)
             ORDER BY d.id
             LIMIT $3",
        )
        .persistent(false)
        .bind(current)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids)
    }

    async fn mark_stale(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError> {
        // The trigger gives the update this version anyway: nothing sets
        // one in its transaction.
        sqlx::query("UPDATE doc_markdown_cache SET renderer_version = $2 WHERE doc_id = $1")
            .bind(doc_id)
            .bind(STALE_RENDERER_VERSION)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn invalidate(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError> {
        sqlx::query("DELETE FROM doc_markdown_cache WHERE doc_id = $1")
            .bind(doc_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
