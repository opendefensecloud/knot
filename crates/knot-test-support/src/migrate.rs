//! Migrating a test database step by step, for tests that seed data under
//! an older schema (or run traffic against one) and then apply the rest.

use sqlx::PgPool;

/// The embedded migrations up to and including `last`.
pub fn migrator_up_to(last: i64) -> sqlx::migrate::Migrator {
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations.iter().any(|m| m.version == last),
        "migration {last} not embedded (cargo clean -p knot-test-support?)"
    );
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version <= last)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
}

/// Applies the embedded migrations up to and including `last`.
pub async fn migrate_up_to(pool: &PgPool, last: i64) {
    migrator_up_to(last)
        .run(pool)
        .await
        .unwrap_or_else(|e| panic!("migrate up to {last}: {e}"));
}

/// Applies every embedded migration not applied yet.
pub async fn migrate_all(pool: &PgPool) {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .expect("migrate");
}

/// A fresh database with no migrations applied, and a pool on it.
pub async fn bare_db(max_connections: u32) -> (String, PgPool) {
    let url = crate::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(&url)
        .await
        .expect("connect");
    (url, pool)
}
