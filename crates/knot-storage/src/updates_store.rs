//! doc_updates persistence: append-only log of Y.Doc binary updates.
//!
//! Per spec §5.4, `seq` is a GLOBAL bigserial; per-doc monotonicity comes
//! from Postgres serialising sequence allocation. Replays use
//! `WHERE doc_id = $1 ORDER BY seq`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgPool};
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
    /// Insert a batch of updates atomically. Returns the assigned seqs in
    /// the same order as the input. The batch is one INSERT with a multi-row
    /// VALUES list so all rows share one round-trip.
    async fn insert_batch(
        &self,
        doc_id: Uuid,
        by_user_id: Option<Uuid>,
        updates: &[Vec<u8>],
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
        by_user_id: Option<Uuid>,
        updates: &[Vec<u8>],
    ) -> Result<Vec<i64>, UpdatesStoreError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        // Build "($1, $2, $3), ($1, $2, $4), ..." with shared doc_id +
        // by_user_id binds and one per-update bytea bind.
        //
        // The generated string contains only a fixed prefix and `$N` placeholder
        // tuples whose N is a loop counter, so the `AssertSqlSafe` below is
        // sound: no caller data reaches the SQL text, only the binds.
        let mut sql =
            String::from("INSERT INTO doc_updates (doc_id, by_user_id, update_bytes) VALUES ");
        for i in 0..updates.len() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("($1, $2, ${})", i + 3));
        }
        sql.push_str(" RETURNING seq");
        let mut q = sqlx::query_scalar::<_, i64>(AssertSqlSafe(sql))
            .bind(doc_id)
            .bind(by_user_id);
        for u in updates {
            q = q.bind(u);
        }
        let seqs = q.fetch_all(&self.pool).await?;
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
