//! Per-document grants: explicit role for a principal on a doc, with
//! `inherit` controlling whether descendant docs see the grant too.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::WorkspaceRole;
use crate::audit;
use crate::invalidations;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub doc_id: Uuid,
    pub principal: String, // "user:<uuid>" or "group:<name>"
    pub role: WorkspaceRole,
    pub inherit: bool,
    pub granted_at: DateTime<Utc>,
    pub granted_by: Option<Uuid>,
}

#[derive(Debug, Error)]
pub enum GrantStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid role: {0}")]
    InvalidRole(String),
}

#[async_trait]
pub trait GrantStore: Send + Sync + 'static {
    async fn list(&self, doc_id: Uuid) -> Result<Vec<Grant>, GrantStoreError>;
    /// List grants attached to any document in the parent chain of `doc_id`,
    /// in walk order (deepest first). Only `inherit=true` grants from
    /// ancestors are returned; the doc's own grants are returned regardless.
    async fn list_inherited(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
    ) -> Result<Vec<Grant>, GrantStoreError>;
    async fn put(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        role: WorkspaceRole,
        inherit: bool,
        granted_by: Uuid,
    ) -> Result<(), GrantStoreError>;
    async fn delete(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        actor: Uuid,
    ) -> Result<(), GrantStoreError>;
}

#[derive(Clone)]
pub struct PgGrantStore {
    pool: PgPool,
}

impl PgGrantStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

type GrantRow = (Uuid, String, String, bool, DateTime<Utc>, Option<Uuid>);
fn from_row(r: GrantRow) -> Result<Grant, GrantStoreError> {
    let role =
        WorkspaceRole::parse(&r.2).ok_or_else(|| GrantStoreError::InvalidRole(r.2.clone()))?;
    Ok(Grant {
        doc_id: r.0,
        principal: r.1,
        role,
        inherit: r.3,
        granted_at: r.4,
        granted_by: r.5,
    })
}

#[async_trait]
impl GrantStore for PgGrantStore {
    async fn list(&self, doc_id: Uuid) -> Result<Vec<Grant>, GrantStoreError> {
        let rows = sqlx::query_as::<_, GrantRow>(
            "SELECT doc_id, principal, role, inherit, granted_at, granted_by
             FROM document_grants WHERE doc_id = $1",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(from_row).collect()
    }

    async fn list_inherited(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
    ) -> Result<Vec<Grant>, GrantStoreError> {
        // Recursive CTE walks from doc_id up to root; selects grants on each.
        // For non-self levels, only inherit=true grants are returned.
        // The depth cap (< 10000) is a defense-in-depth guard: a reintroduced
        // cycle in the document tree would otherwise loop indefinitely in
        // Postgres (ACL-resolution DoS). The heal migration already prevents
        // cycles; this cap is belt-and-suspenders.
        let rows = sqlx::query_as::<_, GrantRow>(
            "WITH RECURSIVE chain AS (
                 SELECT id, parent_id, 0 AS depth
                 FROM documents WHERE id = $2 AND workspace_id = $1
                 UNION ALL
                 SELECT d.id, d.parent_id, c.depth + 1
                 FROM documents d JOIN chain c ON d.id = c.parent_id
                 WHERE d.workspace_id = $1 AND c.depth < 10000
             )
             SELECT g.doc_id, g.principal, g.role, g.inherit, g.granted_at, g.granted_by
             FROM document_grants g
             JOIN chain c ON g.doc_id = c.id
             WHERE c.depth = 0 OR g.inherit = true
             ORDER BY c.depth ASC",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(from_row).collect()
    }

    async fn put(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        role: WorkspaceRole,
        inherit: bool,
        granted_by: Uuid,
    ) -> Result<(), GrantStoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO document_grants (doc_id, principal, role, inherit, granted_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (doc_id, principal) DO UPDATE
             SET role = EXCLUDED.role, inherit = EXCLUDED.inherit,
                 granted_at = now(), granted_by = EXCLUDED.granted_by",
        )
        .bind(doc_id)
        .bind(principal)
        .bind(role.as_str())
        .bind(inherit)
        .bind(granted_by)
        .execute(&mut *tx)
        .await?;
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(granted_by),
            "doc.grant",
            "doc",
            doc_id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "grant-change").await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        actor: Uuid,
    ) -> Result<(), GrantStoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM document_grants WHERE doc_id = $1 AND principal = $2")
            .bind(doc_id)
            .bind(principal)
            .execute(&mut *tx)
            .await?;
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.grant.delete",
            "doc",
            doc_id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "grant-delete").await?;
        tx.commit().await?;
        Ok(())
    }
}
