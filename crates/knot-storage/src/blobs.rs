//! Blob storage abstraction.
//!
//! Two layers:
//!  - `BlobStore` (trait) — byte storage. Implementations: PgBytesStore
//!    (default, in `blobs/pg.rs`) and S3Store (feature `s3`, in `blobs/s3.rs`).
//!  - `BlobMeta` — Postgres-backed metadata operations shared by all backends.

use async_trait::async_trait;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum BlobStoreError {
    #[error("not found")]
    NotFound,
    #[error("backend: {0}")]
    Backend(String),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, BlobStoreError>;

#[derive(Debug, Clone)]
pub struct BlobMetadata {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub doc_id: Uuid,
    pub content_type: String,
    pub byte_size: i64,
    pub sha256: Vec<u8>,
    pub original_name: Option<String>,
    pub created_by: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, id: Uuid, bytes: &[u8], content_type: &str) -> Result<()>;
    async fn get(&self, id: Uuid) -> Result<Vec<u8>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
}

/// Metadata operations against the `blobs` table. Pool-only — backend-agnostic.
pub struct BlobMeta {
    pool: PgPool,
}

impl BlobMeta {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, m: &BlobMetadata) -> Result<()> {
        sqlx::query(
            "INSERT INTO blobs (id, workspace_id, doc_id, content_type, byte_size, sha256, original_name, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(m.id)
        .bind(m.workspace_id)
        .bind(m.doc_id)
        .bind(&m.content_type)
        .bind(m.byte_size)
        .bind(&m.sha256)
        .bind(&m.original_name)
        .bind(m.created_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find(&self, id: Uuid) -> Result<Option<BlobMetadata>> {
        let row: Option<BlobRow> = sqlx::query_as(
            "SELECT id, workspace_id, doc_id, content_type, byte_size, sha256, original_name, created_by, created_at \
             FROM blobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM blobs WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// All blob metadata records owned by `workspace_id`. Used by the
    /// workspace export to bundle attachment bytes alongside the docs.
    pub async fn list_for_workspace(&self, workspace_id: Uuid) -> Result<Vec<BlobMetadata>> {
        let rows: Vec<BlobRow> = sqlx::query_as(
            "SELECT id, workspace_id, doc_id, content_type, byte_size, sha256, original_name, created_by, created_at \
             FROM blobs WHERE workspace_id = $1",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

#[derive(sqlx::FromRow)]
struct BlobRow {
    id: Uuid,
    workspace_id: Uuid,
    doc_id: Uuid,
    content_type: String,
    byte_size: i64,
    sha256: Vec<u8>,
    original_name: Option<String>,
    created_by: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<BlobRow> for BlobMetadata {
    fn from(r: BlobRow) -> Self {
        Self {
            id: r.id,
            workspace_id: r.workspace_id,
            doc_id: r.doc_id,
            content_type: r.content_type,
            byte_size: r.byte_size,
            sha256: r.sha256,
            original_name: r.original_name,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}

pub mod pg;
pub use pg::PgBytesStore;

pub mod s3;
pub use s3::S3Store;
