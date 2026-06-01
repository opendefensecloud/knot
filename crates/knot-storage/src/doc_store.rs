//! Document storage trait — placeholder in v0.1.
//!
//! The real implementation lands in Plan 5 (CRDT room actor + persistence).
//! For Plan 2 we just define the trait shape so consumers can be wired
//! against an interface from day 1.

use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DocStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("not found")]
    NotFound,
}

#[async_trait]
pub trait DocStore: Send + Sync + 'static {
    /// Returns true if a document with this id exists (and is not archived).
    async fn exists(&self, doc_id: Uuid) -> Result<bool, DocStoreError>;
}
