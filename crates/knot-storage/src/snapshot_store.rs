//! doc_snapshots persistence. One row per snapshot. Per spec §5.4:
//! `(doc_id, snapshot_seq)` is the PK; `state_bytes` is the Y.Doc encoded
//! state at that seq; `state_vector` lets us compute diff fetches cheaply.
//!
//! From page history M1.2 on, a room stamps its snapshots with the rev they
//! hold the log up to (`snapshot_rev`) and the content epoch they were taken
//! at. A stamped row is immutable (the `doc_snapshots_epoch` trigger skips an
//! update of one); a legacy row (no `snapshot_rev`) is promoted when a
//! stamped write lands on its seq.
//!
//! From the pending-encoding fix on, a room also records which log rows
//! hold the content its doc kept pending (`pending_revs`), since the
//! snapshot's bytes leave pending content out. A stamped row without them
//! that was written after that migration is an older release's
//! (`DocSnapshot::older_writer`).

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
    /// The ascending revs of the log rows whose bytes, applied after the
    /// snapshot's, re-park what the room held pending; `Some(vec![])` when
    /// nothing was pending, `None` for a snapshot written before the fix or
    /// by an older pod during the rollout.
    pub pending_revs: Option<Vec<i64>>,
    /// A stamped snapshot without `pending_revs` written after the
    /// `pending_revs` migration: an older pod's, which may lack content
    /// parked below the snapshot its room booted from (`older_writer_sql`).
    pub older_writer: bool,
}

/// The `pending_revs` migration, whose `installed_on` tells an older pod's
/// snapshots from those written before the upgrade.
const PENDING_REVS_MIGRATION: i64 = 20261114140000;

/// The SQL expression for `DocSnapshot::older_writer` of the `doc_snapshots`
/// row aliased `alias`: stamped, no `pending_revs`, and created at or after
/// the `pending_revs` migration's `installed_on` (Ruling 6). Every column is
/// qualified, so a statement that also reads a table with columns of the
/// same names (`doc_history_checkpoints`) cannot bind it to the wrong one.
/// `false` on a database where that migration has no row.
///
/// `alias` is spliced into SQL: pass a constant.
pub fn older_writer_sql(alias: &str) -> String {
    format!(
        "coalesce({alias}.pending_revs IS NULL AND {alias}.snapshot_rev IS NOT NULL \
         AND {alias}.created_at >= (SELECT _sqlx_migrations.installed_on FROM _sqlx_migrations \
         WHERE _sqlx_migrations.version = {PENDING_REVS_MIGRATION}), false)"
    )
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
    /// Writes a snapshot at `stamp`, naming the rows that hold its pending
    /// content (`pending_revs`). An existing legacy row at the same seq is
    /// replaced (promoted); an existing stamped row is left as it is.
    /// Returns whether a row was written: `false` when a stamped row already
    /// held the seq. `EpochChanged` when `stamp.epoch` is not the page's,
    /// `DocGone` when the page does not exist.
    async fn insert_stamped(
        &self,
        doc_id: Uuid,
        stamp: SnapshotStamp,
        state_bytes: &[u8],
        state_vector: &[u8],
        pending_revs: Option<&[i64]>,
    ) -> Result<bool, SnapshotStoreError>;

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
        self.insert_stamped(doc_id, stamp, state_bytes, state_vector, None)
            .await
            .map(drop)
    }

    /// The snapshot to boot from: the highest `snapshot_rev`, else (only
    /// legacy ones) the highest `snapshot_seq`. On a database never migrated
    /// past 0.9.9 (`knot-server export-check` on a staging copy), the highest
    /// `snapshot_seq`, unstamped.
    async fn latest(&self, doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError>;

    /// The snapshot `latest` would return among those an older pod did not
    /// write (`DocSnapshot::older_writer` false), for when `latest` returns
    /// one it did. `None` when there is none, and on a schema without
    /// `pending_revs` (nothing is flagged there). Stores that serve no
    /// snapshots return `None`.
    async fn latest_trusted(
        &self,
        _doc_id: Uuid,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        Ok(None)
    }

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

    /// `latest` on a schema without `pending_revs`: M1.2's query, and on a
    /// schema without `snapshot_rev` either (never migrated past 0.9.9), the
    /// highest `snapshot_seq`, unstamped.
    async fn latest_before_pending_revs(
        &self,
        doc_id: Uuid,
    ) -> Result<Option<SnapshotRow>, SnapshotStoreError> {
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                i64,
                Vec<u8>,
                Vec<u8>,
                DateTime<Utc>,
                Option<i64>,
                Option<i32>,
            ),
        >(
            "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at,
                    snapshot_rev, epoch
             FROM doc_snapshots WHERE doc_id = $1
             ORDER BY snapshot_rev DESC NULLS LAST, snapshot_seq DESC LIMIT 1",
        )
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await;
        let row = match row {
            // 42703 again: no `snapshot_rev` column, the schema predates page
            // history, so every snapshot is a legacy one.
            Err(e) if undefined_column(&e) => {
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
        Ok(row.map(|(doc_id, seq, state, sv, created_at, rev, epoch)| {
            (doc_id, seq, state, sv, created_at, rev, epoch, None, false)
        }))
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
    Option<Vec<i64>>,
    bool,
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
        pending_revs: r.7,
        older_writer: r.8,
    }
}

/// The columns of a `SnapshotRow`, from `doc_snapshots s`.
fn snapshot_cols() -> String {
    format!(
        "s.doc_id, s.snapshot_seq, s.state_bytes, s.state_vector, s.created_at, \
         s.snapshot_rev, s.epoch, s.pending_revs, {} AS older_writer",
        older_writer_sql("s")
    )
}

/// Whether `e` is 42703 (undefined column): the schema predates a column.
fn undefined_column(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("42703")
}

#[async_trait]
impl SnapshotStore for PgSnapshotStore {
    async fn insert_stamped(
        &self,
        doc_id: Uuid,
        stamp: SnapshotStamp,
        state_bytes: &[u8],
        state_vector: &[u8],
        pending_revs: Option<&[i64]>,
    ) -> Result<bool, SnapshotStoreError> {
        // On a seq collision the trigger skips the update of a stamped row
        // (no row affected) and lets it promote a legacy one.
        let r = sqlx::query(
            "INSERT INTO doc_snapshots
                 (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch,
                  pending_revs)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now(),
                 snapshot_rev = EXCLUDED.snapshot_rev,
                 epoch = EXCLUDED.epoch,
                 pending_revs = EXCLUDED.pending_revs",
        )
        .bind(doc_id)
        .bind(stamp.snapshot_seq)
        .bind(state_bytes)
        .bind(state_vector)
        .bind(stamp.snapshot_rev)
        .bind(stamp.epoch)
        .bind(pending_revs)
        .execute(&self.pool)
        .await
        .map_err(write_error)?;
        Ok(r.rows_affected() == 1)
    }

    async fn latest(&self, doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        let row = sqlx::query_as::<_, SnapshotRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM doc_snapshots s WHERE s.doc_id = $1
             ORDER BY s.snapshot_rev DESC NULLS LAST, s.snapshot_seq DESC LIMIT 1",
            snapshot_cols()
        )))
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await;
        let row = match row {
            // 42703: no `pending_revs` column, the schema predates the
            // pending-encoding fix (`knot-server export-check` on a staging
            // copy of 0.9.13): no snapshot names its pending rows, and none
            // is an older writer's.
            Err(e) if undefined_column(&e) => self.latest_before_pending_revs(doc_id).await?,
            row => row?,
        };
        Ok(row.map(doc_snapshot))
    }

    async fn latest_trusted(
        &self,
        doc_id: Uuid,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        let row = sqlx::query_as::<_, SnapshotRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM doc_snapshots s WHERE s.doc_id = $1 AND NOT ({})
             ORDER BY s.snapshot_rev DESC NULLS LAST, s.snapshot_seq DESC LIMIT 1",
            snapshot_cols(),
            older_writer_sql("s")
        )))
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await;
        match row {
            // No `pending_revs` column: nothing is flagged, so nothing
            // needs replacing.
            Err(e) if undefined_column(&e) => Ok(None),
            row => Ok(row?.map(doc_snapshot)),
        }
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
        let row = sqlx::query_as::<_, SnapshotRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM doc_snapshots s WHERE s.doc_id = $1 AND s.snapshot_seq = $2",
            snapshot_cols()
        )))
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
        // Keeps the newest `keep_recent`, the newest of each of the last
        // `retain_days` days, and the snapshot `latest_trusted` returns
        // (Ruling 6), which a run of older pods' snapshots would otherwise
        // push out of the first two.
        let r = sqlx::query(sqlx::AssertSqlSafe(format!(
            "WITH recent AS (
                 SELECT s.snapshot_seq FROM doc_snapshots s
                 WHERE s.doc_id = $1 ORDER BY s.snapshot_seq DESC LIMIT $2
             ),
             per_day AS (
                 SELECT DISTINCT ON (date_trunc('day', s.created_at)) s.snapshot_seq
                 FROM doc_snapshots s
                 WHERE s.doc_id = $1 AND s.created_at >= now() - ($3 || ' days')::interval
                 ORDER BY date_trunc('day', s.created_at), s.created_at DESC
             ),
             trusted AS (
                 SELECT s.snapshot_seq FROM doc_snapshots s
                 WHERE s.doc_id = $1 AND NOT ({})
                 ORDER BY s.snapshot_rev DESC NULLS LAST, s.snapshot_seq DESC LIMIT 1
             )
             DELETE FROM doc_snapshots
             WHERE doc_id = $1
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM recent)
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM per_day)
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM trusted)",
            older_writer_sql("s")
        )))
        .bind(doc_id)
        .bind(keep_recent)
        .bind(retain_days)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::older_writer_sql;

    /// Every `doc_snapshots` column the expression names is qualified with
    /// the alias, so a statement that also reads `doc_history_checkpoints`
    /// (which has `pending_revs` and `created_at` too) cannot bind it to
    /// the wrong table.
    #[test]
    fn older_writer_sql_qualifies_its_columns() {
        for alias in ["s", "snap"] {
            let sql = older_writer_sql(alias);
            for column in ["pending_revs", "snapshot_rev", "created_at"] {
                let found: Vec<usize> = sql.match_indices(column).map(|(i, _)| i).collect();
                assert!(!found.is_empty(), "{column} missing from {sql}");
                let qualified = format!("{alias}.");
                for i in found {
                    assert!(
                        sql[..i].ends_with(&qualified),
                        "unqualified {column} at {i} in {sql}"
                    );
                }
            }
            assert!(sql.starts_with("coalesce("), "{sql}");
            assert!(sql.ends_with(", false)"), "{sql}");
        }
    }
}
