//! ACL invalidations outbox. Rows written in the same transaction as the
//! mutation; consumed by the listener in knot-docs.
//!
//! A row names either one doc (`doc_id`), whose access changed for everyone
//! in its subtree, or no doc at all: the access of one member changed in
//! every doc of the workspace (a role change or a removal). The NOTIFY
//! payload is the doc id, or [`workspace_payload`] for the latter;
//! [`parse_payload`] reads both back.

use sqlx::PgConnection;
use uuid::Uuid;

/// The NOTIFY channel the listener subscribes to.
pub const CHANNEL: &str = "acl_invalidate";

/// Marks a workspace-wide payload, so it can't be mistaken for a doc id.
const WORKSPACE_PAYLOAD_PREFIX: &str = "workspace:";

/// What an `acl_invalidate` payload invalidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidated {
    /// The access to this doc and its subtree changed.
    Doc(Uuid),
    /// Someone's access changed anywhere in this workspace.
    Workspace(Uuid),
}

/// The NOTIFY payload of a workspace-wide invalidation.
pub fn workspace_payload(workspace_id: Uuid) -> String {
    format!("{WORKSPACE_PAYLOAD_PREFIX}{workspace_id}")
}

/// Read an `acl_invalidate` payload back; `None` when it is malformed.
pub fn parse_payload(payload: &str) -> Option<Invalidated> {
    match payload.strip_prefix(WORKSPACE_PAYLOAD_PREFIX) {
        Some(workspace_id) => workspace_id.parse().ok().map(Invalidated::Workspace),
        None => payload.parse().ok().map(Invalidated::Doc),
    }
}

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
    notify(tx, &doc_id.to_string()).await
}

/// Record that one member's access changed in every doc of `workspace_id`
/// (`reason`: `member-role`, `member-remove`). Every replica's listener
/// then evicts its whole ACL cache and has every open socket re-check its
/// access: unlike a doc's, this change can't be narrowed to a subtree.
pub async fn record_workspace_in_tx(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO acl_invalidations (workspace_id, doc_id, reason)
         VALUES ($1, NULL, $2)",
    )
    .bind(workspace_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    notify(tx, &workspace_payload(workspace_id)).await
}

async fn notify(tx: &mut PgConnection, payload: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    Ok(())
}
