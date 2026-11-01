//! Doc byline: who created a page and who has changed its content since.
//!
//! `doc_contributors` is written by `PgUpdatesStore::insert_batch`, in the
//! same statement that persists each content update — this module only
//! reads it. Names are resolved by joining `users` rather than
//! `workspace_members`, so someone removed from the workspace keeps their
//! credit. Only ids and display names leave this module, never emails.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersonRef {
    pub id: Uuid,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contributor {
    pub user_id: Uuid,
    pub display_name: String,
    pub first_edited_at: DateTime<Utc>,
    pub last_edited_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocByline {
    pub created_by: PersonRef,
    pub created_at: DateTime<Utc>,
    /// From when on `contributors` is complete. Docs that predate
    /// contributor tracking carry the migration time here; their earlier
    /// edits were never attributed.
    pub contributors_since: DateTime<Utc>,
    /// Most recent editor first; ties broken by display name.
    pub contributors: Vec<Contributor>,
}

#[derive(Debug, Error)]
pub enum ContributorStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[async_trait]
pub trait ContributorStore: Send + Sync + 'static {
    /// The byline for `doc_id`, or `None` if no such doc exists.
    async fn byline(&self, doc_id: Uuid) -> Result<Option<DocByline>, ContributorStoreError>;
}

#[derive(Clone)]
pub struct PgContributorStore {
    pool: PgPool,
}

impl PgContributorStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ContributorStore for PgContributorStore {
    async fn byline(&self, doc_id: Uuid) -> Result<Option<DocByline>, ContributorStoreError> {
        let Some((creator_id, creator_name, created_at, contributors_since)) =
            sqlx::query_as::<_, (Uuid, String, DateTime<Utc>, DateTime<Utc>)>(
                "SELECT d.created_by, u.display_name, d.created_at, d.contributors_since
                 FROM documents d
                 JOIN users u ON u.id = d.created_by
                 WHERE d.id = $1",
            )
            .bind(doc_id)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        // user_id is a last tie-break only so equal (time, name) pairs come
        // back in a stable order across requests.
        let rows = sqlx::query_as::<_, (Uuid, String, DateTime<Utc>, DateTime<Utc>)>(
            "SELECT c.user_id, u.display_name, c.first_edited_at, c.last_edited_at
             FROM doc_contributors c
             JOIN users u ON u.id = c.user_id
             WHERE c.doc_id = $1
             ORDER BY c.last_edited_at DESC, u.display_name ASC, c.user_id",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(DocByline {
            created_by: PersonRef {
                id: creator_id,
                display_name: creator_name,
            },
            created_at,
            contributors_since,
            contributors: rows
                .into_iter()
                .map(|r| Contributor {
                    user_id: r.0,
                    display_name: r.1,
                    first_edited_at: r.2,
                    last_edited_at: r.3,
                })
                .collect(),
        }))
    }
}
