//! Process-wide shared Postgres container for integration tests.
//!
//! Why this exists: every test fixture used to spin up its own
//! `Postgres::default().start()` and then `std::mem::forget` the handle,
//! relying on testcontainers' ryuk reaper to clean up at process exit.
//! Ryuk is not always running on dev machines, so each test run leaked
//! one container per fixture call. Across 20 fixtures × N tests × M
//! `cargo test` runs, this accumulated thousands of leaked containers.
//!
//! Instead, every test binary now shares a single container via
//! [`fresh_db`], which creates a unique database per call. The container
//! itself still leaks once per test binary (statics never drop), but the
//! N→1 reduction makes that tractable — and ryuk, if present, will reap
//! the lone leftover.

use sqlx::{Executor, PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// Pin tests to the same major version compose/prod runs.
const POSTGRES_TAG: &str = "16-alpine";

static SHARED: OnceCell<SharedPg> = OnceCell::const_new();

struct SharedPg {
    _container: ContainerAsync<Postgres>,
    port: u16,
}

async fn shared() -> &'static SharedPg {
    SHARED
        .get_or_init(|| async {
            let container = Postgres::default()
                .with_tag(POSTGRES_TAG)
                .start()
                .await
                .expect("start shared postgres container");
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("get shared postgres port");
            SharedPg {
                _container: container,
                port,
            }
        })
        .await
}

/// A freshly-created, migrated database on the shared test Postgres.
pub struct TestDb {
    /// Connection URL for the unique per-call database.
    /// Use this when something needs to open its own connection
    /// (e.g. `PgBus::connect(&db.url)` for LISTEN/NOTIFY).
    pub url: String,
    /// Pool already connected to `url` with all workspace migrations
    /// applied.
    pub pool: PgPool,
}

/// Create a fresh empty database on the shared Postgres container and
/// return its connection URL. No migrations are applied.
///
/// Use this when the test itself needs to verify migration behavior
/// (otherwise prefer [`fresh_db`]).
pub async fn fresh_db_url() -> String {
    let port = shared().await.port;
    let admin_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("admin connect");
    let name = format!("t_{}", Uuid::new_v4().simple());
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create database");
    drop(admin);
    format!("postgres://postgres:postgres@127.0.0.1:{port}/{name}")
}

/// Create a fresh database on the shared Postgres container, run all
/// workspace migrations against it, and return both the URL and a
/// connected pool.
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
