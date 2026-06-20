//! ACL invalidations outbox. Rows written in the same transaction as the
//! mutation; consumed by the listener in knot-docs.

use sqlx::PgConnection;
use uuid::Uuid;

pub async fn record_in_tx(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    doc_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO acl_invalidations (workspace_id, doc_id, reason)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace_id)
    .bind(doc_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    // Notify listeners. Payload = doc_id text so listener can target evictions.
    // pg_notify() takes the payload as a bound parameter, so no SQL string is
    // built from a value (defence-in-depth; doc_id is an internal Uuid).
    sqlx::query("SELECT pg_notify('acl_invalidate', $1)")
        .bind(doc_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(())
}
