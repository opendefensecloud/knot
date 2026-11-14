//! What the optional quotas count (issue #88): pages and file bytes, in a
//! workspace and per creator. Every row counts until it is purged, archived
//! ones and templates included, so restoring never goes over a quota.
//! Board SVGs and CRDT history are not counted.
//!
//! Pool-only reads, one indexed aggregate each
//! (`20261112120000_quota_indexes`). The server checks them before a write;
//! concurrent writes can both pass, so a quota is best effort.

use sqlx::PgPool;
use uuid::Uuid;

/// Pages in `workspace_id`.
pub async fn pages_in_workspace(pool: &PgPool, workspace_id: Uuid) -> sqlx::Result<u64> {
    let pages: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE workspace_id = $1")
        .bind(workspace_id)
        .fetch_one(pool)
        .await?;
    Ok(non_negative(pages))
}

/// Pages `user_id` created in `workspace_id` (`documents.created_by`;
/// duplicated and imported pages are created by the user who copied them).
pub async fn pages_created_by(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
) -> sqlx::Result<u64> {
    let pages: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents WHERE workspace_id = $1 AND created_by = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(non_negative(pages))
}

/// Bytes of the files in `workspace_id`.
pub async fn blob_bytes_in_workspace(pool: &PgPool, workspace_id: Uuid) -> sqlx::Result<u64> {
    let bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(byte_size), 0)::bigint FROM blobs WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;
    Ok(non_negative(bytes))
}

/// Bytes of the files `user_id` stored in `workspace_id` (`blobs.created_by`:
/// uploaded, imported or copied by a duplicate).
pub async fn blob_bytes_created_by(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
) -> sqlx::Result<u64> {
    let bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(byte_size), 0)::bigint FROM blobs
          WHERE workspace_id = $1 AND created_by = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(non_negative(bytes))
}

/// A count or a sum of positive sizes is never negative.
fn non_negative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
