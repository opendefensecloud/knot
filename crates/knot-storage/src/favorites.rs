//! Favorite pages: the pages a user starred, per workspace, oldest star
//! first.
//!
//! A star is a private preference, so nothing here audits. Rows are never
//! removed when a page becomes hidden or archived; [`FavoriteStore::list`]
//! filters them instead, so a favorite comes back when access does. Purging
//! the page removes its rows through the foreign-key cascade.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::lexorank;

/// Most favorites one user may have in one workspace, counting rows that
/// are currently hidden.
pub const FAVORITES_LIMIT: i64 = 200;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Favorite {
    pub doc_id: Uuid,
    pub title: String,
    pub icon: Option<String>,
    pub sort_key: String,
    /// When the user starred the page.
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    /// Already starred; its position is unchanged.
    AlreadyThere,
    /// The user already has [`FAVORITES_LIMIT`] favorites in the workspace.
    LimitReached,
}

#[derive(Debug, Error)]
pub enum FavoriteStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("sort key: {0}")]
    Key(#[from] lexorank::KeyError),
}

pub type Result<T> = std::result::Result<T, FavoriteStoreError>;

#[async_trait]
pub trait FavoriteStore: Send + Sync + 'static {
    /// Live (not archived, not template) favorites of `user_id` in
    /// `workspace_id`, minus `hidden` (the pages the user can't read),
    /// ordered by `sort_key` (code unit), then `doc_id`.
    async fn list(
        &self,
        user_id: Uuid,
        workspace_id: Uuid,
        hidden: &[Uuid],
    ) -> Result<Vec<Favorite>>;

    /// Star `doc_id` at the end of the user's list. No access check:
    /// callers check read access first.
    async fn add(&self, user_id: Uuid, workspace_id: Uuid, doc_id: Uuid) -> Result<AddOutcome>;

    /// Unstar. `true` if a row was deleted.
    async fn remove(&self, user_id: Uuid, workspace_id: Uuid, doc_id: Uuid) -> Result<bool>;
}

pub struct PgFavoriteStore {
    pool: PgPool,
}

impl PgFavoriteStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FavoriteStore for PgFavoriteStore {
    async fn list(
        &self,
        user_id: Uuid,
        workspace_id: Uuid,
        hidden: &[Uuid],
    ) -> Result<Vec<Favorite>> {
        let rows = sqlx::query_as::<_, Favorite>(
            "SELECT f.doc_id, d.title, d.icon, f.sort_key, f.created_at \
             FROM user_favorites f \
             JOIN documents d ON d.id = f.doc_id \
             WHERE f.user_id = $1 AND f.workspace_id = $2 \
               AND d.workspace_id = $2 \
               AND d.archived_at IS NULL AND NOT d.is_template \
               AND NOT (f.doc_id = ANY($3)) \
             ORDER BY f.sort_key COLLATE \"C\", f.doc_id",
        )
        .bind(user_id)
        .bind(workspace_id)
        .bind(hidden)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn add(&self, user_id: Uuid, workspace_id: Uuid, doc_id: Uuid) -> Result<AddOutcome> {
        let mut tx = crate::begin(&self.pool).await?;
        // One writer per (user, workspace) list at a time: two concurrent
        // stars would otherwise read the same last key (and append the same
        // one), or both pass the cap check.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended( \
                 'user_favorites:' || $1::text || ':' || $2::text, 0))",
        )
        .bind(user_id)
        .bind(workspace_id)
        .execute(&mut *tx)
        .await?;

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM user_favorites WHERE user_id = $1 AND doc_id = $2)",
        )
        .bind(user_id)
        .bind(doc_id)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            return Ok(AddOutcome::AlreadyThere);
        }

        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM user_favorites WHERE user_id = $1 AND workspace_id = $2",
        )
        .bind(user_id)
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await?;
        if count >= FAVORITES_LIMIT {
            return Ok(AddOutcome::LimitReached);
        }

        let last: Option<String> = sqlx::query_scalar(
            "SELECT MAX(sort_key COLLATE \"C\") FROM user_favorites \
             WHERE user_id = $1 AND workspace_id = $2",
        )
        .bind(user_id)
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await?;
        let key = lexorank::between(last.as_deref(), None)?;

        sqlx::query(
            "INSERT INTO user_favorites (user_id, doc_id, workspace_id, sort_key) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(user_id)
        .bind(doc_id)
        .bind(workspace_id)
        .bind(&key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AddOutcome::Added)
    }

    async fn remove(&self, user_id: Uuid, workspace_id: Uuid, doc_id: Uuid) -> Result<bool> {
        let res = sqlx::query(
            "DELETE FROM user_favorites \
             WHERE user_id = $1 AND workspace_id = $2 AND doc_id = $3",
        )
        .bind(user_id)
        .bind(workspace_id)
        .bind(doc_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }
}
