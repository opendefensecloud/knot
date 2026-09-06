//! A stranded transaction must not hold locks forever.
//!
//! Seen in local e2e runs: a backend in `idle in transaction`, minutes old,
//! still holding ACCESS SHARE on `doc_updates` from `UpdatesStore::since`.
//! `e2e/support/reset.ts` then cannot get ACCESS EXCLUSIVE to TRUNCATE, times
//! out, and the suite fails somewhere unrelated-looking.
//!
//! What it is NOT: `tests/cancel_safety.rs` rules out query cancellation and
//! dropped `Transaction`s — sqlx cleans both up. What is left is a peer that
//! goes away without the socket closing (a process killed behind Docker's
//! userland TCP proxy is the reproducible-in-anger case), which leaves
//! Postgres holding a session it still believes is live.
//!
//! We cannot stop a peer from dying. We can stop a dead peer's transaction
//! from holding locks indefinitely, which is what these tests pin.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor};
use std::time::Duration;

/// Production connections must carry a bounded
/// `idle_in_transaction_session_timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn pooled_connections_bound_idle_transactions() {
    let url = knot_test_support::fresh_db().await.url;
    let pool = knot_storage::connect(&url, 5).await.unwrap();

    let setting: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_ne!(
        setting, "0",
        "connections have no idle_in_transaction_session_timeout, so a \
         stranded transaction holds its locks until the server is restarted"
    );
}

/// And the setting does what we are relying on it for: Postgres terminates
/// the session and releases its locks without anyone intervening.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_reclaims_a_session_stranded_in_a_transaction() {
    let db = knot_test_support::fresh_db().await;
    let name = db.url.rsplit('/').next().unwrap().to_string();

    // Same mechanism as production, with a timeout short enough to test.
    let opts: PgConnectOptions = db.url.parse().unwrap();
    let opts = opts.options([("idle_in_transaction_session_timeout", "1s")]);
    let mut victim = sqlx::PgConnection::connect_with(&opts).await.unwrap();

    let observer = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url)
        .await
        .unwrap();

    // Open a transaction, touch a table to take a lock, then go idle.
    victim.execute("BEGIN").await.unwrap();
    victim
        .execute("SELECT count(*) FROM doc_updates")
        .await
        .unwrap();

    let holding: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = $1 AND state = 'idle in transaction'",
    )
    .bind(&name)
    .fetch_one(&observer)
    .await
    .unwrap();
    assert_eq!(
        holding, 1,
        "precondition: the victim is idle in transaction"
    );

    tokio::time::sleep(Duration::from_millis(2500)).await;

    let still_holding: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = $1 AND state = 'idle in transaction'",
    )
    .bind(&name)
    .fetch_one(&observer)
    .await
    .unwrap();
    assert_eq!(
        still_holding, 0,
        "Postgres did not reclaim a session left idle in a transaction"
    );
}
