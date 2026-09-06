//! Postgres connection pool + migration runner.

use std::time::Duration;

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use thiserror::Error;

pub type Pool = PgPool;

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("invalid connection string: {0}")]
    Url(String),
    #[error("connect: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// How long a session may sit IDLE inside an open transaction before Postgres
/// terminates it. A backstop, not the fix — see `after_release` below for the
/// actual defect. Kept because it costs nothing and bounds anything that still
/// escapes; it does NOT catch the bug below, whose connection is in constant
/// use and so never idles at all.
///
/// Counts only idle time between statements, never time executing one, so a
/// slow query or a long migration is not at risk — those sessions are
/// `active`. knot's transactions are a handful of statements with no client
/// round-trips in between, so 30s is far above anything legitimate.
const IDLE_IN_TRANSACTION_TIMEOUT: &str = "30s";

/// Open a Postgres pool and run pending migrations.
pub async fn connect(url: &str, max_conn: u32) -> Result<Pool, PoolError> {
    let opts: PgConnectOptions = url
        .parse()
        .map_err(|e: sqlx::Error| PoolError::Url(e.to_string()))?;

    let opts = opts
        // Names the session in `pg_stat_activity`, so the next time something
        // is holding a lock it is obvious whose connection it is.
        .application_name("knot-server")
        .options([(
            "idle_in_transaction_session_timeout",
            IDLE_IN_TRANSACTION_TIMEOUT,
        )]);

    let pool = PgPoolOptions::new()
        .max_connections(max_conn)
        .acquire_timeout(Duration::from_secs(10))
        // `pool.begin()` is not cancellation-safe. Drop the future after BEGIN
        // has reached Postgres but before `begin()` returns, and sqlx never
        // gets a `Transaction` to roll back and holds no record that one is
        // open — so the connection goes back to the pool looking clean. Every
        // later query handed that connection silently joins the orphaned
        // transaction, which never commits: it accumulates locks across
        // unrelated tables, pins a transaction id against vacuum, and stalls
        // any TRUNCATE or DDL until the process exits.
        //
        // axum drops a handler future exactly this way whenever a client
        // disconnects mid-request, which is ordinary browser behaviour.
        //
        // Caught with `log_statement=all`: one backend logged 12 BEGIN, 11
        // COMMIT and 0 ROLLBACK, and the statements after the unmatched BEGIN
        // were unrelated work for a dozen different documents.
        //
        // No call-site change can fix this — the cancelled task never receives
        // a `Transaction` to clean up — so it has to be caught here, on the
        // way back into the pool. ROLLBACK is a no-op when no transaction is
        // open, which is the overwhelmingly common case.
        //
        // `crates/knot-storage/tests/cancel_safety.rs` reproduces the leak and
        // pins this fix.
        .after_release(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "ROLLBACK").await?;
                Ok(true)
            })
        })
        .connect_with(opts)
        .await?;

    sqlx::migrate!("../../migrations").run(&pool).await?;

    // Background gauge poller: samples pool size + idle every 10 s.
    let pool_for_metrics = pool.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            metrics::gauge!("knot_db_pool_size").set(pool_for_metrics.size() as f64);
            metrics::gauge!("knot_db_pool_idle").set(pool_for_metrics.num_idle() as f64);
        }
    });

    Ok(pool)
}
