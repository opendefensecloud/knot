//! Test database helper — reuses the dev-compose Postgres at
//! `localhost:5432`. Each call to [`fresh_db`] creates a unique database
//! (`CREATE DATABASE`) and returns a connected, migrated pool. No
//! containers are spawned; running `make compose.up` before `cargo test`
//! is a prerequisite.
//!
//! Why this approach: an earlier iteration spawned a testcontainers
//! Postgres per call (then per binary via `OnceCell`). `OnceCell` only
//! dedupes within a single process — `cargo test` launches one process
//! per test binary, so the workspace's ~10 test binaries produced ~10
//! containers per run. With no cleanup, the host accumulated thousands.
//! Sharing the long-lived dev-compose container reduces this to zero
//! test-owned containers.
//!
//! **Cleanup**: databases are NOT dropped automatically when `TestDb`
//! goes out of scope (Drop with async work is awkward, and most tests
//! consume `.pool` directly which would conflict). Instead, leftover
//! `t_*` databases accumulate inside the dev container and are reclaimed
//! by `make db.cleanup` (one short query). Inside a single Postgres
//! instance these are cheap; thousands of empty databases use kilobytes
//! of storage and no idle resources.

use sqlx::{AssertSqlSafe, Executor, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

/// Connection string for the dev-compose Postgres. Override with the
/// `KNOT_TEST_DATABASE_URL` env var if you run Postgres on a different
/// port or host.
pub fn admin_url() -> String {
    std::env::var("KNOT_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://knot:knot@127.0.0.1:5432/knot".to_string())
}

/// A freshly-created, migrated database on the shared dev Postgres.
pub struct TestDb {
    /// Connection URL for the unique per-call database.
    /// Use this when something needs to open its own connection
    /// (e.g. `PgBus::connect(&db.url)` for LISTEN/NOTIFY).
    pub url: String,
    /// Pool already connected to `url` with all workspace migrations
    /// applied.
    pub pool: PgPool,
}

/// Returns the admin URL (same host/port/user as `admin_url`, but with
/// the database name forced to `postgres`).
fn parse_admin_db_url(url: &str) -> String {
    match url.rfind('/') {
        Some(i) => format!("{}/postgres", &url[..i]),
        None => url.to_string(),
    }
}

/// Create a fresh empty database on the dev Postgres and return its
/// connection URL. No migrations are applied.
pub async fn fresh_db_url() -> String {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&parse_admin_db_url(&admin_url()))
        .await
        .expect("admin connect (is `make compose.up` running?)");
    let name = format!("t_{}", Uuid::new_v4().simple());
    admin
        .execute(AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .await
        .expect("create database");
    drop(admin);
    // Replace the database name in the admin URL with the new one.
    let base = admin_url();
    match base.rfind('/') {
        Some(i) => format!("{}/{name}", &base[..i]),
        None => format!("{base}/{name}"),
    }
}

/// Create a fresh database on the dev Postgres, run all workspace
/// migrations against it, and return both the URL and a connected pool.
pub async fn fresh_db() -> TestDb {
    let url = fresh_db_url().await;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("pool connect");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("migrate");
    TestDb { url, pool }
}
