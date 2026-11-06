//! The hourly GC thins snapshots only: doc_updates are page history and are
//! never deleted, and only one replica runs a pass at a time.

use knot_crdt::GcRun;
use knot_storage::PgSnapshotStore;
use sqlx::{Connection, PgPool};
use uuid::Uuid;

/// The literal the plan fixes; the test pins the const to it so the two
/// cannot drift.
const GC_LOCK_KEY_FOR_TEST: i64 = 0x6b6e_6f74_6763_0001;

async fn seed_doc(pool: &PgPool, updates: usize, snapshots: usize) -> Uuid {
    let ws: Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (slug, name) VALUES ('default', 'W') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'Alice') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    for _ in 0..updates {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, created_at)
             VALUES ($1, '\\x01', now() - interval '40 days')",
        )
        .bind(doc)
        .execute(pool)
        .await
        .unwrap();
    }
    for seq in 1..=snapshots as i64 {
        sqlx::query(
            "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
             VALUES ($1, $2, '\\x01', '\\x00', now() - interval '40 days')",
        )
        .bind(doc)
        .bind(seq)
        .execute(pool)
        .await
        .unwrap();
    }
    doc
}

async fn count(pool: &PgPool, table: &str, doc: Uuid) -> i64 {
    let sql = match table {
        "doc_updates" => "SELECT count(*) FROM doc_updates WHERE doc_id = $1",
        "doc_snapshots" => "SELECT count(*) FROM doc_snapshots WHERE doc_id = $1",
        "doc_history_baselines" => "SELECT count(*) FROM doc_history_baselines WHERE doc_id = $1",
        other => panic!("unknown table {other}"),
    };
    sqlx::query_scalar(sql)
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_thins_snapshots_but_keeps_every_update() {
    let db = knot_test_support::fresh_db().await;
    let snapshots = PgSnapshotStore::new(db.pool.clone());
    let doc = seed_doc(&db.pool, 20, 8).await;

    let run = knot_crdt::gc_run_once(&db.pool, &snapshots).await.unwrap();
    assert_eq!(
        run,
        GcRun::Ran {
            docs: 1,
            snapshots_deleted: 3
        }
    ); // 8 - keep 5
    assert_eq!(count(&db.pool, "doc_snapshots", doc).await, 5);
    assert_eq!(count(&db.pool, "doc_updates", doc).await, 20);
    // Baselines are only the migration's copy; GC adds none.
    assert_eq!(count(&db.pool, "doc_history_baselines", doc).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_is_single_flight_across_replicas() {
    assert_eq!(knot_crdt::gc::GC_LOCK_KEY, GC_LOCK_KEY_FOR_TEST);
    let db = knot_test_support::fresh_db().await;
    let snapshots = PgSnapshotStore::new(db.pool.clone());
    let pool = &db.pool;

    // Another replica holds the lock on its own session.
    let mut other = pool.acquire().await.unwrap().detach();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(GC_LOCK_KEY_FOR_TEST)
        .fetch_one(&mut other)
        .await
        .unwrap();
    assert!(held);
    assert_eq!(
        knot_crdt::gc_run_once(pool, &snapshots).await.unwrap(),
        GcRun::Skipped
    );

    // Released, the next run goes ahead.
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(GC_LOCK_KEY_FOR_TEST)
        .execute(&mut other)
        .await
        .unwrap();
    assert!(matches!(
        knot_crdt::gc_run_once(pool, &snapshots).await.unwrap(),
        GcRun::Ran { .. }
    ));

    // And run_once released its own lock: another session can take it now.
    // The probe is opened outside the pool, so it cannot be the same session
    // (advisory locks are reentrant per session).
    let mut probe = sqlx::PgConnection::connect(&db.url).await.unwrap();
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(GC_LOCK_KEY_FOR_TEST)
        .fetch_one(&mut probe)
        .await
        .unwrap();
    assert!(free);
}
