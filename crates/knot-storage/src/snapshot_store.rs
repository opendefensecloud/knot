//! doc_snapshots persistence. One row per snapshot. Per spec §5.4:
//! `(doc_id, snapshot_seq)` is the PK; `state_bytes` is the Y.Doc encoded
//! state at that seq; `state_vector` lets us compute diff fetches cheaply.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSnapshot {
    pub doc_id: Uuid,
    pub snapshot_seq: i64,
    pub state_bytes: Vec<u8>,
    pub state_vector: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

/// Lightweight metadata row returned by `SnapshotStore::list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotMeta {
    pub snapshot_seq: i64,
    pub byte_size: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum SnapshotStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[async_trait]
pub trait SnapshotStore: Send + Sync + 'static {
    async fn insert(
        &self,
        doc_id: Uuid,
        snapshot_seq: i64,
        state_bytes: &[u8],
        state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError>;

    async fn latest(&self, doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError>;

    async fn gc(
        &self,
        doc_id: Uuid,
        keep_recent: i64,
        retain_days: i32,
    ) -> Result<u64, SnapshotStoreError>;

    /// Most recent `limit` snapshots for a doc, newest-first, metadata only.
    async fn list(&self, doc_id: Uuid, limit: i64)
    -> Result<Vec<SnapshotMeta>, SnapshotStoreError>;

    /// Full snapshot row by seq.
    async fn by_seq(
        &self,
        doc_id: Uuid,
        snapshot_seq: i64,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError>;
}

#[derive(Clone)]
pub struct PgSnapshotStore {
    pool: PgPool,
}

impl PgSnapshotStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SnapshotStore for PgSnapshotStore {
    async fn insert(
        &self,
        doc_id: Uuid,
        snapshot_seq: i64,
        state_bytes: &[u8],
        state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError> {
        sqlx::query(
            "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now()",
        )
        .bind(doc_id)
        .bind(snapshot_seq)
        .bind(state_bytes)
        .bind(state_vector)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn latest(&self, doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        let row = sqlx::query_as::<_, (Uuid, i64, Vec<u8>, Vec<u8>, DateTime<Utc>)>(
            "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at
             FROM doc_snapshots WHERE doc_id = $1 ORDER BY snapshot_seq DESC LIMIT 1",
        )
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| DocSnapshot {
            doc_id: r.0,
            snapshot_seq: r.1,
            state_bytes: r.2,
            state_vector: r.3,
            created_at: r.4,
        }))
    }

    async fn list(
        &self,
        doc_id: Uuid,
        limit: i64,
    ) -> Result<Vec<SnapshotMeta>, SnapshotStoreError> {
        let rows = sqlx::query_as::<_, (i64, i64, DateTime<Utc>)>(
            "SELECT snapshot_seq, octet_length(state_bytes)::bigint, created_at
             FROM doc_snapshots
             WHERE doc_id = $1
             ORDER BY snapshot_seq DESC
             LIMIT $2",
        )
        .bind(doc_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(snapshot_seq, byte_size, created_at)| SnapshotMeta {
                snapshot_seq,
                byte_size,
                created_at,
            })
            .collect())
    }

    async fn by_seq(
        &self,
        doc_id: Uuid,
        snapshot_seq: i64,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        let row = sqlx::query_as::<_, (Uuid, i64, Vec<u8>, Vec<u8>, DateTime<Utc>)>(
            "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at
             FROM doc_snapshots
             WHERE doc_id = $1 AND snapshot_seq = $2",
        )
        .bind(doc_id)
        .bind(snapshot_seq)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| DocSnapshot {
            doc_id: r.0,
            snapshot_seq: r.1,
            state_bytes: r.2,
            state_vector: r.3,
            created_at: r.4,
        }))
    }

    async fn gc(
        &self,
        doc_id: Uuid,
        keep_recent: i64,
        retain_days: i32,
    ) -> Result<u64, SnapshotStoreError> {
        let r = sqlx::query(
            "WITH recent AS (
                 SELECT snapshot_seq FROM doc_snapshots
                 WHERE doc_id = $1 ORDER BY snapshot_seq DESC LIMIT $2
             ),
             per_day AS (
                 SELECT DISTINCT ON (date_trunc('day', created_at)) snapshot_seq
                 FROM doc_snapshots
                 WHERE doc_id = $1 AND created_at >= now() - ($3 || ' days')::interval
                 ORDER BY date_trunc('day', created_at), created_at DESC
             )
             DELETE FROM doc_snapshots
             WHERE doc_id = $1
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM recent)
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM per_day)",
        )
        .bind(doc_id)
        .bind(keep_recent)
        .bind(retain_days)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}
