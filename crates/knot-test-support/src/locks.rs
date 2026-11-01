//! Deterministic lock interleavings for the placement tests: wait until
//! sessions are blocked on a lock, and make Postgres abort a chosen
//! transaction to break a deadlock (SQLSTATE 40P01), so a test can check that
//! the operation runs its transaction once more.

use std::fmt::Debug;
use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

/// The first half of the top-level group's advisory lock key (P8), `"knot"`
/// in ASCII; the second half is `hashtext(workspace_id::text)`. The same key
/// as `knot_storage::placement` takes.
pub const TOP_LEVEL_LOCK: i32 = 1_802_399_604;

/// Waits until `n` sessions on `pool`'s database are waiting for a lock.
///
/// # Panics
///
/// When they don't within ten seconds.
pub async fn wait_for_lock_waits(pool: &PgPool, n: i64) {
    for _ in 0..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{n} sessions never waited for a lock together");
}

/// Runs `op`, which takes the top-level lock of `ws` first and later must
/// update `blocker`, a top-level row with a bad key, to respread the group.
/// A stand-in transaction holds `blocker` and waits for the top-level lock:
/// a deadlock. Postgres aborts `op` with 40P01, and `op` must succeed by
/// running its transaction once more. Returns what `op` returns.
///
/// Postgres checks a lock wait for a deadlock once, `deadlock_timeout` (1 s)
/// after the wait starts, and aborts the session whose check finds the
/// cycle. Two things make that `op`, even on a loaded machine:
///
/// - `op`'s wait for `blocker` starts last. A gate transaction holds the
///   top-level lock until `op` and then the stand-in queue for it. `op`,
///   first in the queue, gets it when the gate ends and goes on to wait for
///   `blocker`, with the stand-in already waiting. Without the gate, `op`
///   waited first, and on a loaded machine the stand-in could queue more
///   than a second later: `op`'s only check found no cycle yet, and the
///   stand-in's check aborted the stand-in.
/// - The stand-in's own check comes a minute after its wait starts, long
///   after `op`'s. Setting `deadlock_timeout` needs a superuser, which the
///   test databases' role is (CI and the dev stack).
///
/// # Panics
///
/// When Postgres aborts the stand-in instead, or `op` fails.
pub async fn deadlocks_once<T, E, F>(pool: &PgPool, ws: Uuid, blocker: Uuid, op: F) -> T
where
    T: Send + 'static,
    E: Debug + Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    const TAKE_TOP_LEVEL_LOCK: &str = "SELECT pg_advisory_xact_lock($1, hashtext($2::uuid::text))";
    let mut stand_in = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL deadlock_timeout = '1min'")
        .execute(&mut *stand_in)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM documents WHERE id = $1 FOR UPDATE")
        .bind(blocker)
        .execute(&mut *stand_in)
        .await
        .unwrap();
    let mut gate = pool.begin().await.unwrap();
    sqlx::query(TAKE_TOP_LEVEL_LOCK)
        .bind(TOP_LEVEL_LOCK)
        .bind(ws)
        .execute(&mut *gate)
        .await
        .unwrap();

    let op = tokio::spawn(op);
    wait_for_lock_waits(pool, 1).await;
    let stand_in = tokio::spawn(async move {
        let queued = sqlx::query(TAKE_TOP_LEVEL_LOCK)
            .bind(TOP_LEVEL_LOCK)
            .bind(ws)
            .execute(&mut *stand_in)
            .await;
        stand_in.rollback().await.unwrap();
        queued
    });
    wait_for_lock_waits(pool, 2).await;
    gate.rollback().await.unwrap();

    stand_in
        .await
        .unwrap()
        .expect("Postgres aborts the operation, not the stand-in");
    op.await.unwrap().expect("one retry after the deadlock")
}
