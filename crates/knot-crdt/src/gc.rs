//! Hourly GC of doc_snapshots.
//!
//! Snapshots are a boot cache, so they are thinned to the 3 newest per page,
//! plus the newest one boot trusts and those the history builder has still
//! to check; a page the current history builder has not built keeps what
//! `gc` keeps (the last 5, 1 a day for 30 days, and the trusted one;
//! `SnapshotStore::thin`). `doc_updates` are page history and are kept
//! forever: this module never deletes them (the database refuses it too).
//! Only one replica runs a pass at a time, under an advisory lock.
//!
//! This scans all docs that have at least one snapshot row. v0.1's
//! workload is small; a full scan is fine.

use std::sync::Arc;
use std::time::Duration;

use knot_storage::{SnapshotStore, ThinPolicy};
use sqlx::PgPool;

/// Advisory-lock key held for the length of one GC pass, so replicas do not
/// thin the same snapshots at once. "knotgc" plus a serial.
pub const GC_LOCK_KEY: i64 = 0x6b6e_6f74_6763_0001;

/// Snapshots kept per page the current history builder has built, newest
/// in boot order.
pub const KEEP_SNAPSHOTS: i64 = 3;
/// At most this many unprobed snapshot revs kept beside them (the lowest),
/// plus every unprobed one written before the page's genesis.
pub const KEEP_UNPROBED: i64 = 32;
/// On every other page: `gc`'s newest by seq ...
pub const LEGACY_KEEP_RECENT: i64 = 5;
/// ... and one a day for this many days.
pub const LEGACY_RETAIN_DAYS: i32 = 30;

/// The thinning policy for a process whose history builder is
/// `builder_version` (`knot_history::BUILDER_VERSION`).
pub fn policy(builder_version: i32) -> ThinPolicy {
    ThinPolicy {
        keep_newest: KEEP_SNAPSHOTS,
        keep_unprobed: KEEP_UNPROBED,
        builder_version,
        legacy_keep_recent: LEGACY_KEEP_RECENT,
        legacy_retain_days: LEGACY_RETAIN_DAYS,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GcRun {
    /// Another replica holds the lock; nothing was done.
    Skipped,
    Ran {
        docs: usize,
        snapshots_deleted: u64,
    },
}

pub fn spawn(pool: PgPool, snapshots: Arc<dyn SnapshotStore>, builder_version: i32) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            if let Err(e) = run_once(&pool, snapshots.as_ref(), builder_version).await {
                tracing::warn!(error=?e, "gc: run failed");
            }
        }
    });
}

pub async fn run_once(
    pool: &PgPool,
    snapshots: &dyn SnapshotStore,
    builder_version: i32,
) -> Result<GcRun, sqlx::Error> {
    // A session-level lock on a connection taken out of the pool: dropping
    // it closes the session, which releases the lock even if this run
    // fails part-way. A pooled connection would go back to the pool still
    // holding the lock and stop every later run, on every replica. The
    // success path still unlocks explicitly: the drop only closes the
    // socket, so the server would release the lock asynchronously.
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
    let policy = policy(builder_version);
    let mut snapshots_deleted = 0;
    for doc_id in &docs {
        match snapshots.thin(*doc_id, policy).await {
            Ok(n) => snapshots_deleted += n,
            Err(e) => tracing::warn!(error=?e, %doc_id, "gc snapshots failed"),
        }
    }
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(GC_LOCK_KEY)
        .execute(&mut lock_conn)
        .await
    {
        tracing::warn!(error=?e, "gc: releasing the lock failed");
    }
    drop(lock_conn);
    Ok(GcRun::Ran {
        docs: docs.len(),
        snapshots_deleted,
    })
}
