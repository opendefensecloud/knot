//! What sqlx does to a pooled connection when work is abandoned.
//!
//! These pin the behaviour that `pool_hygiene.rs`'s reasoning depends on.
//! They were written to answer a specific question — why backends were being
//! left in `idle in transaction` holding `UpdatesStore::since`'s ACCESS SHARE
//! on `doc_updates`, long enough to stall `e2e/support/reset.ts`'s TRUNCATE —
//! and the answer turned out to be "not any of this".
//!
//! Each test below is a hypothesis that was ruled out. They are kept because
//! they are cheap and because a future sqlx upgrade that regresses any of
//! them would reintroduce the leak silently: nothing else in the suite would
//! notice a connection going back to the pool mid-protocol.

use knot_storage::{
    DocStore, PgDocStore, PgUpdatesStore, PgUserStore, PgWorkspaceStore, UpdatesStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

fn db_name(url: &str) -> String {
    url.rsplit('/').next().unwrap_or_default().to_string()
}

async fn idle_in_transaction(observer: &sqlx::PgPool, db: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = $1 AND state = 'idle in transaction'",
    )
    .bind(db)
    .fetch_one(observer)
    .await
    .unwrap()
}

/// A separate pool, so that observing cannot clean up the connection under
/// test by reusing it.
async fn observer(url: &str) -> sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap()
}

/// Hypothesis 1: dropping a `since()` future mid-fetch stands its connection
/// up in an implicit transaction. It does not.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_fetch_does_not_strand_its_connection() {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();
    let name = db_name(&db.url);

    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "D", "m", u.id)
        .await
        .unwrap();

    let store = PgUpdatesStore::new(pool.clone());
    let blob = vec![7u8; 4096];
    for _ in 0..8 {
        let batch: Vec<Vec<u8>> = (0..250).map(|_| blob.clone()).collect();
        store
            .insert_batch(doc.id, Some(u.id), &batch)
            .await
            .unwrap();
    }

    let obs = observer(&db.url).await;
    assert_eq!(idle_in_transaction(&obs, &name).await, 0, "precondition");

    // `timeout` drops the query future — the same thing axum does to a
    // handler future when the client disconnects.
    let r = tokio::time::timeout(Duration::from_millis(3), store.since(doc.id, 0)).await;
    assert!(
        r.is_err(),
        "the query finished inside the timeout, so nothing was cancelled — \
         raise the payload or lower the timeout"
    );

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        idle_in_transaction(&obs, &name).await,
        0,
        "a cancelled fetch left a backend idle in transaction"
    );
}

/// Hypothesis 2: a `Transaction` dropped without commit sits open until the
/// connection is reused. It does not — sqlx sends the ROLLBACK promptly.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_transaction_rolls_back_promptly() {
    let db = knot_test_support::fresh_db().await;
    let name = db_name(&db.url);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url)
        .await
        .unwrap();
    let obs = observer(&db.url).await;

    {
        let tx = pool.begin().await.unwrap();
        drop(tx); // no commit, no explicit rollback
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert_eq!(
        idle_in_transaction(&obs, &name).await,
        0,
        "a dropped transaction was left open"
    );
}

/// Hypothesis 3: cancelling a query that is provably still executing strands
/// the connection. It does not.
///
/// Worth knowing what actually happens, because it is not what you would
/// guess: sqlx sends no CancelRequest, so the backend runs the abandoned
/// statement to completion. It settles into plain `idle`, not `idle in
/// transaction` — so the connection is reusable, it just was not free during
/// the intervening time.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_query_does_not_strand_its_connection() {
    let db = knot_test_support::fresh_db().await;
    let name = db_name(&db.url);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url)
        .await
        .unwrap();
    let obs = observer(&db.url).await;

    let q = sqlx::query_scalar::<_, String>("SELECT pg_sleep(3)::text").fetch_one(&pool);
    let r = tokio::time::timeout(Duration::from_millis(300), q).await;
    assert!(r.is_err(), "pg_sleep(3) should not finish inside 300ms");

    // Still executing: the statement outlives the future that asked for it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let running: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = $1 AND state = 'active' AND query LIKE '%pg_sleep%'
           AND pid <> pg_backend_pid()",
    )
    .bind(&name)
    .fetch_one(&obs)
    .await
    .unwrap();
    assert_eq!(running, 1, "sqlx is expected not to cancel the statement");

    // And once it finishes, the connection is clean rather than in a
    // transaction.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(
        idle_in_transaction(&obs, &name).await,
        0,
        "the abandoned statement left its connection in a transaction"
    );
}

/// Hypothesis 4: a `Transaction` dropped inside a CANCELLED future — the
/// shape axum produces when a client disconnects mid-request — leaves the
/// connection in the pool still inside that transaction.
///
/// This differs from `a_dropped_transaction_rolls_back_promptly` in one way
/// that turns out to matter: there the drop happens in a task that keeps
/// running afterwards, so sqlx gets polled and issues its ROLLBACK. Here the
/// whole future is discarded.
#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_dropped_by_cancellation_does_not_poison_the_pool() {
    let db = knot_test_support::fresh_db().await;
    let name = db_name(&db.url);
    // One connection, so the next acquire is guaranteed to be the same one.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url)
        .await
        .unwrap();
    let obs = observer(&db.url).await;

    let work = async {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT count(*) FROM doc_updates")
            .execute(&mut *tx)
            .await
            .unwrap();
        // Stand in for whatever the handler awaits next.
        tokio::time::sleep(Duration::from_secs(30)).await;
        tx.commit().await.unwrap();
    };
    let r = tokio::time::timeout(Duration::from_millis(300), work).await;
    assert!(r.is_err(), "the work should not have completed");

    tokio::time::sleep(Duration::from_millis(500)).await;

    let stranded = idle_in_transaction(&obs, &name).await;
    assert_eq!(
        stranded, 0,
        "a cancelled request left its connection inside an open transaction; \
         every later query handed this connection joins that transaction and \
         its locks are never released"
    );
}

/// Hypothesis 5, and the one that is true: `pool.begin()` is not
/// cancellation-safe.
///
/// If the future is dropped after BEGIN has reached Postgres but before
/// `begin()` returns, sqlx never gets a `Transaction` to roll back and has no
/// record that a transaction is open. The connection goes back to the pool
/// looking clean, and every later query handed that connection silently joins
/// the orphaned transaction — which then never commits.
///
/// Caught in the wild by turning on `log_statement=all`: one backend logged
/// 12 BEGIN, 11 COMMIT and 0 ROLLBACK, and the statements after the unmatched
/// BEGIN were unrelated work for a dozen different documents.
///
/// This is the characterisation half. If sqlx ever fixes this upstream, this
/// test fails and `knot_storage::begin` can be retired.
#[tokio::test(flavor = "multi_thread")]
async fn the_raw_pool_begin_is_not_cancellation_safe() {
    let db = knot_test_support::fresh_db().await;
    let name = db_name(&db.url);
    let pool = knot_storage::connect(&db.url, 1).await.unwrap();
    let obs = observer(&db.url).await;

    let mut cancelled = 0u32;
    for i in 0..3_000u64 {
        let d = Duration::from_micros(20 + (i % 400) * 5);
        match tokio::time::timeout(d, pool.begin()).await {
            Ok(tx) => tx.unwrap().rollback().await.unwrap(),
            Err(_) => {
                cancelled += 1;
                if idle_in_transaction(&obs, &name).await > 0 {
                    return; // reproduced — the wrapper below is still needed
                }
            }
        }
    }
    panic!(
        "cancelled {cancelled} raw begin() calls without stranding a \
         transaction. Either the window was never hit, or sqlx fixed this \
         upstream — in which case knot_storage::begin can be retired."
    );
}

/// And the guard: cancelling `knot_storage::begin` cannot poison the pool,
/// because the BEGIN runs on a detached task that always completes and always
/// drops its `Transaction`.
#[tokio::test(flavor = "multi_thread")]
async fn knot_begin_survives_cancellation() {
    let db = knot_test_support::fresh_db().await;
    let name = db_name(&db.url);
    let pool = knot_storage::connect(&db.url, 1).await.unwrap();
    let obs = observer(&db.url).await;

    // Check after EVERY cancellation. Checking only at the end hides the bug:
    // a later successful begin()/rollback() on the same pooled connection
    // rolls the orphaned transaction back too.
    let mut cancelled = 0u32;
    for i in 0..3_000u64 {
        let d = Duration::from_micros(20 + (i % 400) * 5);
        match tokio::time::timeout(d, knot_storage::begin(&pool)).await {
            Ok(tx) => tx.unwrap().rollback().await.unwrap(),
            Err(_) => {
                cancelled += 1;
                // The detached BEGIN may still be in flight; give its
                // Transaction a moment to drop and roll back.
                tokio::time::sleep(Duration::from_millis(2)).await;
                let stranded = idle_in_transaction(&obs, &name).await;
                assert_eq!(
                    stranded, 0,
                    "a cancelled knot_storage::begin left the pooled connection \
                     inside an open transaction (after {cancelled} cancellations)"
                );
            }
        }
    }
    assert!(
        cancelled > 0,
        "no begin() was actually cancelled, so this proved nothing — \
         widen the timeout sweep"
    );
    eprintln!("cancelled {cancelled} knot_storage::begin calls with no poisoning");
}
