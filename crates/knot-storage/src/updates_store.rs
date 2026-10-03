//! doc_updates persistence: append-only log of Y.Doc binary updates.
//!
//! Per spec §5.4, `seq` is a GLOBAL bigserial; per-doc monotonicity comes
//! from Postgres serialising sequence allocation. Replays use
//! `WHERE doc_id = $1 ORDER BY seq`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocUpdate {
    pub seq: i64,
    pub doc_id: Uuid,
    pub update_bytes: Vec<u8>,
    pub by_user_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum UpdatesStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[async_trait]
pub trait UpdatesStore: Send + Sync + 'static {
    /// Insert a batch of `(by_user_id, update_bytes)` rows atomically, each
    /// row stored with its OWN author: one batch can mix several people's
    /// edits (the room writer flushes whatever arrived in its window).
    /// Returns the assigned seqs in the same order as the input.
    async fn insert_batch(
        &self,
        doc_id: Uuid,
        updates: &[(Option<Uuid>, Vec<u8>)],
    ) -> Result<Vec<i64>, UpdatesStoreError>;

    /// Fetch updates with `seq > after_seq` for a doc, in seq order.
    async fn since(
        &self,
        doc_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError>;

    /// Highest seq for a doc, or 0 if none.
    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError>;

    /// Delete updates with seq <= cutoff (used by snapshot GC).
    async fn delete_up_to(&self, doc_id: Uuid, cutoff_seq: i64) -> Result<u64, UpdatesStoreError>;
}

#[derive(Clone)]
pub struct PgUpdatesStore {
    pool: PgPool,
}

impl PgUpdatesStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UpdatesStore for PgUpdatesStore {
    async fn insert_batch(
        &self,
        doc_id: Uuid,
        updates: &[(Option<Uuid>, Vec<u8>)],
    ) -> Result<Vec<i64>, UpdatesStoreError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let (authors, bytes): (Vec<Option<Uuid>>, Vec<&[u8]>) =
            updates.iter().map(|(by, b)| (*by, b.as_slice())).unzip();
        // One statement whatever the batch size: the rows travel as two
        // parallel arrays. `seq` comes from nextval() as rows are inserted,
        // and UNNEST … ORDER BY ord feeds them in input order, so ascending
        // seq IS input order — which the writer relies on to zip seqs back
        // onto its jobs.
        //
        // The `contrib` CTE credits every distinct author in the SAME
        // statement, so a content change and its byline entry commit or fail
        // together. A data-modifying CTE always runs to completion even
        // though the outer SELECT never reads it. Details:
        //   - DISTINCT: ON CONFLICT DO UPDATE may not touch a row twice in
        //     one statement, and a batch often holds many rows per author.
        //   - ORDER BY by_user_id: replicas flush the same doc concurrently;
        //     taking the row locks in one global order rules out deadlocks.
        //   - the WHERE throttles the refresh to once a minute — continuous
        //     typing flushes every 250 ms, and a minute-old "last edited" is
        //     as good as a fresh one for a byline.
        let seqs = sqlx::query_scalar::<_, i64>(
            "WITH ins AS (
                 INSERT INTO doc_updates (doc_id, by_user_id, update_bytes)
                 SELECT $1, u.by_user_id, u.update_bytes
                 FROM UNNEST($2::uuid[], $3::bytea[])
                      WITH ORDINALITY AS u(by_user_id, update_bytes, ord)
                 ORDER BY u.ord
                 RETURNING seq, by_user_id
             ),
             contrib AS (
                 INSERT INTO doc_contributors (doc_id, user_id)
                 SELECT DISTINCT $1::uuid, by_user_id
                 FROM ins
                 WHERE by_user_id IS NOT NULL
                 ORDER BY by_user_id
                 ON CONFLICT (doc_id, user_id) DO UPDATE
                     SET last_edited_at = now()
                     WHERE doc_contributors.last_edited_at < now() - interval '1 minute'
             )
             SELECT seq FROM ins ORDER BY seq",
        )
        .bind(doc_id)
        .bind(&authors)
        .bind(&bytes)
        .fetch_all(&self.pool)
        .await?;
        Ok(seqs)
    }

    async fn since(
        &self,
        doc_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        let rows = sqlx::query_as::<_, (i64, Uuid, Vec<u8>, Option<Uuid>, DateTime<Utc>)>(
            "SELECT seq, doc_id, update_bytes, by_user_id, created_at
             FROM doc_updates
             WHERE doc_id = $1 AND seq > $2
             ORDER BY seq",
        )
        .bind(doc_id)
        .bind(after_seq)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DocUpdate {
                seq: r.0,
                doc_id: r.1,
                update_bytes: r.2,
                by_user_id: r.3,
                created_at: r.4,
            })
            .collect())
    }

    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM doc_updates WHERE doc_id = $1")
                .bind(doc_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(v.unwrap_or(0))
    }

    async fn delete_up_to(&self, doc_id: Uuid, cutoff_seq: i64) -> Result<u64, UpdatesStoreError> {
        let r = sqlx::query("DELETE FROM doc_updates WHERE doc_id = $1 AND seq <= $2")
            .bind(doc_id)
            .bind(cutoff_seq)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}
