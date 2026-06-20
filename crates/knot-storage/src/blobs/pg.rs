//! Postgres bytea blob backend. Default. Caps at 10 MB enforced by the
//! migration's CHECK constraint on `blobs.byte_size`.

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use super::{BlobStore, BlobStoreError, Result};

pub struct PgBytesStore {
    pool: PgPool,
}

impl PgBytesStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BlobStore for PgBytesStore {
    async fn put(&self, id: Uuid, bytes: &[u8], _content_type: &str) -> Result<()> {
        sqlx::query("INSERT INTO blob_bytes (blob_id, bytes) VALUES ($1, $2)")
            .bind(id)
            .bind(bytes)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Vec<u8>> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT bytes FROM blob_bytes WHERE blob_id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(b,)| b).ok_or(BlobStoreError::NotFound)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM blob_bytes WHERE blob_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
