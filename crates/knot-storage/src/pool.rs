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

/// Open a Postgres pool and run pending migrations.
pub async fn connect(url: &str, max_conn: u32) -> Result<Pool, PoolError> {
    let opts: PgConnectOptions = url
        .parse()
        .map_err(|e: sqlx::Error| PoolError::Url(e.to_string()))?;

    let pool = PgPoolOptions::new()
        .max_connections(max_conn)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await?;

    sqlx::migrate!("../../migrations").run(&pool).await?;

    Ok(pool)
}
