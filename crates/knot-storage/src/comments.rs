//! Comment threads on documents.
//!
//! Schema:
//!   comments(id, doc_id, thread_id, parent_id NULL, author_id, body,
//!            position_y BYTEA NULL, anchor_text TEXT NULL,
//!            created_at, updated_at, resolved_at NULL, deleted_at NULL)
//!   comment_reactions(comment_id, user_id, emoji, created_at, PK(comment_id, user_id, emoji))
//!
//! Thread model: parent_id IS NULL → thread root; thread_id = root id.
//!               parent_id = thread_id  → reply.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

/// serde helper: serialize Option<Vec<u8>> as a base64 string (so JS clients
/// see `position_y: "AbCd..."` instead of `[1,2,3,...]`). Deserialize accepts
/// either a string (base64) or a missing field (None).
mod base64_opt {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => s.serialize_str(&STANDARD.encode(bytes)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let opt: Option<String> = Option::deserialize(d)?;
        match opt {
            Some(s) => STANDARD
                .decode(s.as_bytes())
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Comment {
    pub id: Uuid,
    pub doc_id: Uuid,
    pub thread_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub author_id: Uuid,
    pub body: String,
    /// Yjs RelativePosition for the START of the anchored range. Serialized as
    /// base64 string on the wire so JS clients can call atob() directly.
    #[serde(default, with = "base64_opt")]
    pub position_y: Option<Vec<u8>>,
    /// Yjs RelativePosition for the END of the anchored range.
    #[serde(default, with = "base64_opt")]
    pub position_y_end: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// Per-emoji lists of user_ids that reacted.
    pub reactions: HashMap<String, Vec<Uuid>>,
}

/// A reaction row as stored.
#[derive(Debug, Clone)]
pub struct Reaction {
    pub comment_id: Uuid,
    pub user_id: Uuid,
    pub emoji: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum CommentStoreError {
    #[error("not found")]
    NotFound,
    #[error("body too long")]
    BodyTooLong,
    #[error("forbidden")]
    Forbidden,
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, CommentStoreError>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait CommentStore: Send + Sync + 'static {
    /// Create a new thread root comment. Server sets thread_id = id, parent_id = NULL.
    async fn create_thread(
        &self,
        doc_id: Uuid,
        author_id: Uuid,
        body: &str,
        position_y: Option<Vec<u8>>,
        position_y_end: Option<Vec<u8>>,
        anchor_text: Option<String>,
    ) -> Result<Comment>;

    /// Create a reply in an existing thread. Server sets thread_id from URL,
    /// parent_id = thread_id.
    async fn create_reply(
        &self,
        doc_id: Uuid,
        thread_id: Uuid,
        author_id: Uuid,
        body: &str,
    ) -> Result<Comment>;

    /// List all non-deleted comments on a doc. When `include_resolved` is false,
    /// threads whose root has resolved_at set are excluded (along with their replies).
    async fn list(&self, doc_id: Uuid, include_resolved: bool) -> Result<Vec<Comment>>;

    /// Resolve a thread (only callable on a root; enforces parent_id IS NULL).
    async fn resolve(&self, thread_id: Uuid) -> Result<()>;

    /// Clear resolved_at on a thread root.
    async fn unresolve(&self, thread_id: Uuid) -> Result<()>;

    /// Update body of a comment. Returns the updated comment.
    async fn update_body(&self, comment_id: Uuid, body: &str) -> Result<Comment>;

    /// Soft-delete a comment.
    async fn delete(&self, comment_id: Uuid) -> Result<()>;

    /// Add a reaction. Idempotent (ON CONFLICT DO NOTHING).
    async fn add_reaction(&self, comment_id: Uuid, user_id: Uuid, emoji: &str) -> Result<()>;

    /// Remove a reaction. Idempotent — no error if absent.
    async fn remove_reaction(&self, comment_id: Uuid, user_id: Uuid, emoji: &str) -> Result<()>;

    /// Fetch a single comment by id (non-deleted). Used for author checks in the server.
    async fn get(&self, comment_id: Uuid) -> Result<Comment>;
}

// ---------------------------------------------------------------------------
// Postgres implementation
// ---------------------------------------------------------------------------

pub struct PgCommentStore {
    pool: PgPool,
}

impl PgCommentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// Internal row shapes for sqlx.

#[derive(sqlx::FromRow)]
struct CommentRow {
    id: Uuid,
    doc_id: Uuid,
    thread_id: Uuid,
    parent_id: Option<Uuid>,
    author_id: Uuid,
    body: String,
    position_y: Option<Vec<u8>>,
    position_y_end: Option<Vec<u8>>,
    anchor_text: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ReactionRow {
    comment_id: Uuid,
    user_id: Uuid,
    emoji: String,
}

/// Load reactions for a slice of comment ids and build the emoji→user_ids map.
async fn load_reactions(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, HashMap<String, Vec<Uuid>>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<ReactionRow> = sqlx::query_as(
        "SELECT comment_id, user_id, emoji
         FROM comment_reactions
         WHERE comment_id = ANY($1)
         ORDER BY created_at",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;

    let mut map: HashMap<Uuid, HashMap<String, Vec<Uuid>>> = HashMap::new();
    for r in rows {
        map.entry(r.comment_id)
            .or_default()
            .entry(r.emoji)
            .or_default()
            .push(r.user_id);
    }
    Ok(map)
}

fn row_to_comment(r: CommentRow, reactions: HashMap<String, Vec<Uuid>>) -> Comment {
    Comment {
        id: r.id,
        doc_id: r.doc_id,
        thread_id: r.thread_id,
        parent_id: r.parent_id,
        author_id: r.author_id,
        body: r.body,
        position_y: r.position_y,
        position_y_end: r.position_y_end,
        anchor_text: r.anchor_text,
        created_at: r.created_at,
        updated_at: r.updated_at,
        resolved_at: r.resolved_at,
        reactions,
    }
}

/// Fetch all reactions for a single comment.
async fn comment_reactions(pool: &PgPool, comment_id: Uuid) -> Result<HashMap<String, Vec<Uuid>>> {
    let ids = [comment_id];
    let mut map = load_reactions(pool, &ids).await?;
    Ok(map.remove(&comment_id).unwrap_or_default())
}

#[async_trait]
impl CommentStore for PgCommentStore {
    async fn create_thread(
        &self,
        doc_id: Uuid,
        author_id: Uuid,
        body: &str,
        position_y: Option<Vec<u8>>,
        position_y_end: Option<Vec<u8>>,
        anchor_text: Option<String>,
    ) -> Result<Comment> {
        if body.len() > 4096 {
            return Err(CommentStoreError::BodyTooLong);
        }
        let id = Uuid::new_v4();
        let thread_id = id; // root: thread_id = id
        let row: CommentRow = sqlx::query_as(
            "INSERT INTO comments
               (id, doc_id, thread_id, parent_id, author_id, body, position_y, position_y_end, anchor_text)
             VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
             RETURNING id, doc_id, thread_id, parent_id, author_id, body,
                       position_y, position_y_end, anchor_text, created_at, updated_at, resolved_at",
        )
        .bind(id)
        .bind(doc_id)
        .bind(thread_id)
        .bind(author_id)
        .bind(body)
        .bind(position_y)
        .bind(position_y_end)
        .bind(anchor_text)
        .fetch_one(&self.pool)
        .await?;

        Ok(row_to_comment(row, HashMap::new()))
    }

    async fn create_reply(
        &self,
        doc_id: Uuid,
        thread_id: Uuid,
        author_id: Uuid,
        body: &str,
    ) -> Result<Comment> {
        if body.len() > 4096 {
            return Err(CommentStoreError::BodyTooLong);
        }
        let id = Uuid::new_v4();
        let row: CommentRow = sqlx::query_as(
            "INSERT INTO comments
               (id, doc_id, thread_id, parent_id, author_id, body)
             VALUES ($1, $2, $3, $3, $4, $5)
             RETURNING id, doc_id, thread_id, parent_id, author_id, body,
                       position_y, position_y_end, anchor_text, created_at, updated_at, resolved_at",
        )
        .bind(id)
        .bind(doc_id)
        .bind(thread_id)
        .bind(author_id)
        .bind(body)
        .fetch_one(&self.pool)
        .await?;

        Ok(row_to_comment(row, HashMap::new()))
    }

    async fn list(&self, doc_id: Uuid, include_resolved: bool) -> Result<Vec<Comment>> {
        let rows: Vec<CommentRow> = if include_resolved {
            sqlx::query_as(
                "SELECT id, doc_id, thread_id, parent_id, author_id, body,
                        position_y, position_y_end, anchor_text, created_at, updated_at, resolved_at
                 FROM comments
                 WHERE doc_id = $1 AND deleted_at IS NULL
                 ORDER BY created_at",
            )
            .bind(doc_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            // Exclude threads that are resolved and their replies.
            sqlx::query_as(
                "SELECT c.id, c.doc_id, c.thread_id, c.parent_id, c.author_id, c.body,
                        c.position_y, c.position_y_end, c.anchor_text, c.created_at, c.updated_at, c.resolved_at
                 FROM comments c
                 JOIN comments root ON root.id = c.thread_id
                 WHERE c.doc_id = $1
                   AND c.deleted_at IS NULL
                   AND root.deleted_at IS NULL
                   AND root.resolved_at IS NULL
                 ORDER BY c.created_at",
            )
            .bind(doc_id)
            .fetch_all(&self.pool)
            .await?
        };

        let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let mut reactions = load_reactions(&self.pool, &ids).await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let rxn = reactions.remove(&r.id).unwrap_or_default();
                row_to_comment(r, rxn)
            })
            .collect())
    }

    async fn resolve(&self, thread_id: Uuid) -> Result<()> {
        // Only callable on a root (parent_id IS NULL). If the row does not
        // exist or is not a root, treat as NotFound.
        let affected = sqlx::query(
            "UPDATE comments
             SET resolved_at = NOW()
             WHERE id = $1
               AND parent_id IS NULL
               AND deleted_at IS NULL
               AND resolved_at IS NULL",
        )
        .bind(thread_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if affected == 0 {
            return Err(CommentStoreError::NotFound);
        }
        Ok(())
    }

    async fn unresolve(&self, thread_id: Uuid) -> Result<()> {
        let affected = sqlx::query(
            "UPDATE comments
             SET resolved_at = NULL
             WHERE id = $1
               AND parent_id IS NULL
               AND deleted_at IS NULL",
        )
        .bind(thread_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if affected == 0 {
            return Err(CommentStoreError::NotFound);
        }
        Ok(())
    }

    async fn update_body(&self, comment_id: Uuid, body: &str) -> Result<Comment> {
        if body.len() > 4096 {
            return Err(CommentStoreError::BodyTooLong);
        }
        let row: Option<CommentRow> = sqlx::query_as(
            "UPDATE comments
             SET body = $2, updated_at = NOW()
             WHERE id = $1 AND deleted_at IS NULL
             RETURNING id, doc_id, thread_id, parent_id, author_id, body,
                       position_y, position_y_end, anchor_text, created_at, updated_at, resolved_at",
        )
        .bind(comment_id)
        .bind(body)
        .fetch_optional(&self.pool)
        .await?;

        let row = row.ok_or(CommentStoreError::NotFound)?;
        let reactions = comment_reactions(&self.pool, comment_id).await?;
        Ok(row_to_comment(row, reactions))
    }

    async fn delete(&self, comment_id: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE comments SET deleted_at = NOW()
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(comment_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn add_reaction(&self, comment_id: Uuid, user_id: Uuid, emoji: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO comment_reactions (comment_id, user_id, emoji)
             VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(comment_id)
        .bind(user_id)
        .bind(emoji)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_reaction(&self, comment_id: Uuid, user_id: Uuid, emoji: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM comment_reactions
             WHERE comment_id = $1 AND user_id = $2 AND emoji = $3",
        )
        .bind(comment_id)
        .bind(user_id)
        .bind(emoji)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, comment_id: Uuid) -> Result<Comment> {
        let row: Option<CommentRow> = sqlx::query_as(
            "SELECT id, doc_id, thread_id, parent_id, author_id, body,
                    position_y, position_y_end, anchor_text, created_at, updated_at, resolved_at
             FROM comments
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(comment_id)
        .fetch_optional(&self.pool)
        .await?;

        let row = row.ok_or(CommentStoreError::NotFound)?;
        let reactions = comment_reactions(&self.pool, comment_id).await?;
        Ok(row_to_comment(row, reactions))
    }
}
