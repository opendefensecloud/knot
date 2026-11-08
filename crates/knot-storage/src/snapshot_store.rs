//! doc_snapshots persistence. One row per snapshot. Per spec §5.4:
//! `(doc_id, snapshot_seq)` is the PK; `state_bytes` is the Y.Doc encoded
//! state at that seq; `state_vector` lets us compute diff fetches cheaply.
//!
//! From page history M1.2 on, a room stamps its snapshots with the rev they
//! hold the log up to (`snapshot_rev`) and the content epoch they were taken
//! at. A stamped row is immutable (the `doc_snapshots_epoch` trigger skips an
//! update of one); a legacy row (no `snapshot_rev`) is promoted when a
//! stamped write lands on its seq.

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
    /// The rev the snapshot holds the log up to; `None` for a legacy one.
    pub snapshot_rev: Option<i64>,
    /// The content epoch it was taken at; `None` for a legacy one.
    pub epoch: Option<i32>,
}

/// Where a snapshot sits in the page's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotStamp {
    /// The seq of the row at `snapshot_rev` (or of the snapshot the room
    /// booted from), so 0.9.9 replicas' seq-based hydration keeps working.
    pub snapshot_seq: i64,
    pub snapshot_rev: Option<i64>,
    pub epoch: Option<i32>,
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
    /// The page's content epoch is no longer the one given (KH002).
    #[error("page history epoch changed; reload the page")]
    EpochChanged,
    /// The page does not exist (purged; 23503).
    #[error("page does not exist")]
    DocGone,
}

/// A snapshot write's error: KH002 and 23503 get their own variants.
fn write_error(e: sqlx::Error) -> SnapshotStoreError {
    match e.as_database_error().and_then(|d| d.code()).as_deref() {
        Some("KH002") => SnapshotStoreError::EpochChanged,
        Some("23503") => SnapshotStoreError::DocGone,
        _ => SnapshotStoreError::Sqlx(e),
    }
}

#[async_trait]
pub trait SnapshotStore: Send + Sync + 'static {
    /// Writes a snapshot at `stamp`. An existing legacy row at the same seq is
    /// replaced (promoted); an existing stamped row is left as it is.
    /// `EpochChanged` when `stamp.epoch` is not the page's, `DocGone` when the
    /// page does not exist.
    async fn insert_stamped(
        &self,
        doc_id: Uuid,
        stamp: SnapshotStamp,
        state_bytes: &[u8],
        state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError>;

    /// A legacy write: no rev, no epoch. Kept for tests; production code
    /// calls `insert_stamped`.
    async fn insert(
        &self,
        doc_id: Uuid,
        snapshot_seq: i64,
        state_bytes: &[u8],
        state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError> {
        let stamp = SnapshotStamp {
            snapshot_seq,
            snapshot_rev: None,
            epoch: None,
        };
        self.insert_stamped(doc_id, stamp, state_bytes, state_vector)
            .await
    }

    /// The snapshot to boot from: the highest `snapshot_rev`, else (only
    /// legacy ones) the highest `snapshot_seq`. On a database never migrated
    /// past 0.9.9 (`knot-server export-check` on a staging copy), the highest
    /// `snapshot_seq`, unstamped.
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

type SnapshotRow = (
    Uuid,
    i64,
    Vec<u8>,
    Vec<u8>,
    DateTime<Utc>,
    Option<i64>,
    Option<i32>,
);

fn doc_snapshot(r: SnapshotRow) -> DocSnapshot {
    DocSnapshot {
        doc_id: r.0,
        snapshot_seq: r.1,
        state_bytes: r.2,
        state_vector: r.3,
        created_at: r.4,
        snapshot_rev: r.5,
        epoch: r.6,
    }
}

#[async_trait]
impl SnapshotStore for PgSnapshotStore {
    async fn insert_stamped(
        &self,
        doc_id: Uuid,
        stamp: SnapshotStamp,
        state_bytes: &[u8],
        state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError> {
        // On a seq collision the trigger skips the update of a stamped row
        // and lets it promote a legacy one.
        sqlx::query(
            "INSERT INTO doc_snapshots
                 (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now(),
                 snapshot_rev = EXCLUDED.snapshot_rev,
                 epoch = EXCLUDED.epoch",
        )
        .bind(doc_id)
        .bind(stamp.snapshot_seq)
        .bind(state_bytes)
        .bind(state_vector)
        .bind(stamp.snapshot_rev)
        .bind(stamp.epoch)
        .execute(&self.pool)
        .await
        .map_err(write_error)?;
        Ok(())
    }

    async fn latest(&self, doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        let row = sqlx::query_as::<_, SnapshotRow>(
            "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at,
                    snapshot_rev, epoch
             FROM doc_snapshots WHERE doc_id = $1
             ORDER BY snapshot_rev DESC NULLS LAST, snapshot_seq DESC LIMIT 1",
        )
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await;
        let row = match row {
            // 42703: no `snapshot_rev` column, the schema predates page
            // history, so every snapshot is a legacy one.
            Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("42703") => {
                sqlx::query_as::<_, (Uuid, i64, Vec<u8>, Vec<u8>, DateTime<Utc>)>(
                    "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at
                     FROM doc_snapshots WHERE doc_id = $1 ORDER BY snapshot_seq DESC LIMIT 1",
                )
                .bind(doc_id)
                .fetch_optional(&self.pool)
                .await?
                .map(|(doc_id, seq, state, sv, created_at)| {
                    (doc_id, seq, state, sv, created_at, None, None)
                })
            }
            row => row?,
        };
        Ok(row.map(doc_snapshot))
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
        let row = sqlx::query_as::<_, SnapshotRow>(
            "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at,
                    snapshot_rev, epoch
             FROM doc_snapshots
             WHERE doc_id = $1 AND snapshot_seq = $2",
        )
        .bind(doc_id)
        .bind(snapshot_seq)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(doc_snapshot))
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
