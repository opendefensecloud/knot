//! Hourly GC of doc_snapshots.
//!
//! Snapshots are a boot cache, so they are thinned to "keep last 5 + 1/day
//! for 30 days". `doc_updates` are page history and are kept forever: this
//! module never deletes them (the database refuses it too). Only one
//! replica runs a pass at a time, under an advisory lock.
//!
//! This scans all docs that have at least one snapshot row. v0.1's
//! workload is small; a full scan is fine.

use std::sync::Arc;
use std::time::Duration;

use knot_storage::SnapshotStore;
use sqlx::PgPool;

/// Advisory-lock key held for the length of one GC pass, so replicas do not
/// thin the same snapshots at once. "knotgc" plus a serial.
pub const GC_LOCK_KEY: i64 = 0x6b6e_6f74_6763_0001;

#[derive(Debug, PartialEq, Eq)]
pub enum GcRun {
    /// Another replica holds the lock; nothing was done.
    Skipped,
    Ran {
        docs: usize,
        snapshots_deleted: u64,
    },
}

pub fn spawn(pool: PgPool, snapshots: Arc<dyn SnapshotStore>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            if let Err(e) = run_once(&pool, snapshots.as_ref()).await {
                tracing::warn!(error=?e, "gc: run failed");
            }
        }
    });
}

pub async fn run_once(pool: &PgPool, snapshots: &dyn SnapshotStore) -> Result<GcRun, sqlx::Error> {
    // A session-level lock on a connection taken out of the pool: dropping
    // it closes the session, which releases the lock even if this run
    // fails part-way. A pooled connection would go back to the pool still
    // holding the lock and stop every later run, on every replica.
    let mut lock_conn = pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(GC_LOCK_KEY)
        .fetch_one(&mut lock_conn)
        .await?;
    if !locked {
        return Ok(GcRun::Skipped);
    }
    let docs = sqlx::query_scalar::<_, uuid::Uuid>("SELECT DISTINCT doc_id FROM doc_snapshots")
        .fetch_all(pool)
        .await?;
    let mut snapshots_deleted = 0;
    for doc_id in &docs {
        match snapshots.gc(*doc_id, 5, 30).await {
            Ok(n) => snapshots_deleted += n,
            Err(e) => tracing::warn!(error=?e, %doc_id, "gc snapshots failed"),
        }
    }
    drop(lock_conn);
    Ok(GcRun::Ran {
        docs: docs.len(),
        snapshots_deleted,
    })
}
