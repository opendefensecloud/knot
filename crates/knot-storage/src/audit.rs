//! Best-effort audit_events writer. Failures are logged + swallowed.

use sqlx::types::JsonValue;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub async fn record(
    pool: &PgPool,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
) {
    let result = sqlx::query(
        "INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(workspace_id)
    .bind(actor)
    .bind(action)
    .bind(target_kind)
    .bind(target_id)
    .execute(pool)
    .await;
    if let Err(e) = result {
        tracing::warn!(error=?e, action, "audit write failed (best-effort)");
    }
}

/// Same as `record` but accepts an in-flight transaction so the audit row
/// is committed alongside its mutation.
pub async fn record_in_tx(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
) -> Result<(), sqlx::Error> {
    let no_data = JsonValue::Object(Default::default());
    record_in_tx_with_data(
        tx,
        workspace_id,
        actor,
        action,
        target_kind,
        target_id,
        &no_data,
    )
    .await
}

/// Same as `record_in_tx`, with details about the event in `data` (a JSON
/// object).
pub async fn record_in_tx_with_data(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
    data: &JsonValue,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id, data)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(workspace_id)
    .bind(actor)
    .bind(action)
    .bind(target_kind)
    .bind(target_id)
    .bind(data)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
