//! Per-document grants: explicit role for a principal on a doc, with
//! `inherit` controlling whether descendant docs see the grant too.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::types::JsonValue;
use sqlx::{PgConnection, PgPool};
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

/// One document of an ancestor chain or workspace tree, as the access rule
/// sees it: where it hangs and the two flags that change access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainNode {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub restricted: bool,
    pub archived: bool,
}

#[derive(Debug, Error)]
pub enum GrantStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid role: {0}")]
    InvalidRole(String),
    /// The change would leave a restricted doc without a `user:` owner
    /// grant, and so with nobody able to manage it.
    #[error("last owner of a restricted doc")]
    LastOwner,
    /// A `user:` principal who is not a member of the workspace. Their grant
    /// would give nothing, and would come back to life on a re-invite.
    #[error("principal is not a workspace member")]
    NotMember,
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
    /// Ancestor chain of `doc_id` ROOT FIRST (doc last), plus `principal`'s
    /// grants on it, inheriting or not. Empty chain if the doc is missing or
    /// not in `workspace_id`.
    async fn chain_for_principal(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
    ) -> Result<(Vec<ChainNode>, Vec<Grant>), GrantStoreError>;
    /// [`GrantStore::chain_for_principal`] for `user:<user_id>`, plus the
    /// user's workspace role (`None` when not a member), read in one
    /// snapshot: a decision drawn from both can't mix the states before and
    /// after a concurrent restriction, grant or role change.
    async fn chain_with_member_role(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
    ) -> Result<(Vec<ChainNode>, Vec<Grant>, Option<WorkspaceRole>), GrantStoreError>;
    /// Every grant of `principal` on docs of `workspace_id`.
    async fn list_for_principal(
        &self,
        workspace_id: Uuid,
        principal: &str,
    ) -> Result<Vec<Grant>, GrantStoreError>;
    /// Upsert `principal`'s grant on the doc, audited as `doc.grant` by
    /// `granted_by`. `LastOwner` when it downgrades the last owner grant of
    /// a restricted doc; `NotMember` when `principal` is a `user:` who is
    /// not a member of `workspace_id`. The membership check is ordered
    /// against `WorkspaceStore::remove_member`, so no grant outlives it.
    async fn put(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        role: WorkspaceRole,
        inherit: bool,
        granted_by: Uuid,
    ) -> Result<(), GrantStoreError>;
    /// [`GrantStore::put`] for the break-glass `knot-server admin grant`,
    /// which has no acting user: the row's `granted_by` is NULL, and the
    /// grant is audited as `admin.grant` (actor NULL, `data.break_glass`,
    /// the principal, role and inherit) in the same transaction.
    async fn put_break_glass(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        role: WorkspaceRole,
        inherit: bool,
    ) -> Result<(), GrantStoreError>;
    /// Remove `principal`'s grant on the doc. `LastOwner` when it is the
    /// last owner grant of a restricted doc.
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

/// A chain node joined with the principal's grant on it, if any. The grant
/// columns are all NULL when there is none.
type ChainRow = (
    Uuid,
    Option<Uuid>,
    bool,
    bool,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<DateTime<Utc>>,
    Option<Uuid>,
);

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

    async fn chain_for_principal(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
    ) -> Result<(Vec<ChainNode>, Vec<Grant>), GrantStoreError> {
        load_chain(&self.pool, workspace_id, doc_id, principal).await
    }

    async fn chain_with_member_role(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
    ) -> Result<(Vec<ChainNode>, Vec<Grant>, Option<WorkspaceRole>), GrantStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let (chain, user_grants) =
            load_chain(&mut *tx, workspace_id, doc_id, &format!("user:{user_id}")).await?;
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(workspace_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let role = role
            .map(|role| WorkspaceRole::parse(&role).ok_or(GrantStoreError::InvalidRole(role)))
            .transpose()?;
        Ok((chain, user_grants, role))
    }

    async fn list_for_principal(
        &self,
        workspace_id: Uuid,
        principal: &str,
    ) -> Result<Vec<Grant>, GrantStoreError> {
        let rows = sqlx::query_as::<_, GrantRow>(
            "SELECT g.doc_id, g.principal, g.role, g.inherit, g.granted_at, g.granted_by
             FROM document_grants g
             JOIN documents d ON d.id = g.doc_id
             WHERE d.workspace_id = $1 AND g.principal = $2",
        )
        .bind(workspace_id)
        .bind(principal)
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
        let mut tx = crate::begin(&self.pool).await?;
        let grant = NewGrant {
            workspace_id,
            doc_id,
            principal,
            role,
            inherit,
            granted_by: Some(granted_by),
        };
        upsert_in_tx(&mut tx, &grant).await?;
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(granted_by),
            "doc.grant",
            "doc",
            doc_id,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn put_break_glass(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        principal: &str,
        role: WorkspaceRole,
        inherit: bool,
    ) -> Result<(), GrantStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let grant = NewGrant {
            workspace_id,
            doc_id,
            principal,
            role,
            inherit,
            granted_by: None,
        };
        upsert_in_tx(&mut tx, &grant).await?;
        let audit_data = JsonValue::from_iter([
            ("break_glass", JsonValue::from(true)),
            ("principal", JsonValue::from(principal)),
            ("role", JsonValue::from(role.as_str())),
            ("inherit", JsonValue::from(inherit)),
        ]);
        audit::record_in_tx_with_data(
            &mut tx,
            workspace_id,
            None,
            "admin.grant",
            "doc",
            doc_id,
            &audit_data,
        )
        .await?;
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
        let mut tx = crate::begin(&self.pool).await?;
        refuse_last_owner_removal(&mut tx, doc_id, principal).await?;
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

/// The ancestor chain of `doc_id` ROOT FIRST, with `principal`'s grants on
/// it. The same walk as `list_inherited`, workspace filter and depth cap
/// included. `(doc_id, principal)` is the grants' primary key, so the LEFT
/// JOIN yields exactly one row per chain node.
async fn load_chain<'connection, Executor>(
    executor: Executor,
    workspace_id: Uuid,
    doc_id: Uuid,
    principal: &str,
) -> Result<(Vec<ChainNode>, Vec<Grant>), GrantStoreError>
where
    Executor: sqlx::PgExecutor<'connection>,
{
    let rows = sqlx::query_as::<_, ChainRow>(
        "WITH RECURSIVE chain AS (
             SELECT id, parent_id, restricted, archived_at IS NOT NULL AS archived,
                    0 AS depth
             FROM documents WHERE id = $2 AND workspace_id = $1
             UNION ALL
             SELECT d.id, d.parent_id, d.restricted, d.archived_at IS NOT NULL,
                    c.depth + 1
             FROM documents d JOIN chain c ON d.id = c.parent_id
             WHERE d.workspace_id = $1 AND c.depth < 10000
         )
         SELECT c.id, c.parent_id, c.restricted, c.archived,
                g.principal, g.role, g.inherit, g.granted_at, g.granted_by
         FROM chain c
         LEFT JOIN document_grants g ON g.doc_id = c.id AND g.principal = $3
         ORDER BY c.depth DESC",
    )
    .bind(workspace_id)
    .bind(doc_id)
    .bind(principal)
    .fetch_all(executor)
    .await?;
    let mut chain = Vec::with_capacity(rows.len());
    let mut principal_grants = Vec::new();
    for (
        id,
        parent_id,
        restricted,
        archived,
        grant_principal,
        role,
        inherit,
        granted_at,
        granted_by,
    ) in rows
    {
        chain.push(ChainNode {
            id,
            parent_id,
            restricted,
            archived,
        });
        if let (Some(grant_principal), Some(role), Some(inherit), Some(granted_at)) =
            (grant_principal, role, inherit, granted_at)
        {
            principal_grants.push(from_row((
                id,
                grant_principal,
                role,
                inherit,
                granted_at,
                granted_by,
            ))?);
        }
    }
    Ok((chain, principal_grants))
}

/// A grant as `put` and `put_break_glass` write it.
struct NewGrant<'principal> {
    workspace_id: Uuid,
    doc_id: Uuid,
    principal: &'principal str,
    role: WorkspaceRole,
    inherit: bool,
    /// `None` for the break-glass grant, which no user made.
    granted_by: Option<Uuid>,
}

/// Upsert `grant` and record its invalidation, after the membership check
/// and the last-owner guard. The caller audits it and commits.
async fn upsert_in_tx(tx: &mut PgConnection, grant: &NewGrant<'_>) -> Result<(), GrantStoreError> {
    require_member(tx, grant.workspace_id, grant.principal).await?;
    if grant.role != WorkspaceRole::Owner {
        refuse_last_owner_removal(tx, grant.doc_id, grant.principal).await?;
    }
    sqlx::query(
        "INSERT INTO document_grants (doc_id, principal, role, inherit, granted_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (doc_id, principal) DO UPDATE
         SET role = EXCLUDED.role, inherit = EXCLUDED.inherit,
             granted_at = now(), granted_by = EXCLUDED.granted_by",
    )
    .bind(grant.doc_id)
    .bind(grant.principal)
    .bind(grant.role.as_str())
    .bind(grant.inherit)
    .bind(grant.granted_by)
    .execute(&mut *tx)
    .await?;
    invalidations::record_in_tx(tx, grant.workspace_id, grant.doc_id, "grant-change").await?;
    Ok(())
}

/// `NotMember` unless a `user:` principal is a member of the workspace.
/// Other principals pass: they belong to no workspace, and ACL resolution
/// honours none of them anyway.
///
/// Locks the membership row `FOR KEY SHARE`, which orders this against
/// `WorkspaceStore::remove_member`'s delete of it: a removal that got there
/// first has committed when the lock is granted, and the row is gone; one
/// that comes later waits for this transaction, and its grant cleanup then
/// sees the grant written here. A role change does not wait.
async fn require_member(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    principal: &str,
) -> Result<(), GrantStoreError> {
    let Some(user_part) = principal.strip_prefix("user:") else {
        return Ok(());
    };
    let Ok(user_id) = Uuid::parse_str(user_part) else {
        return Err(GrantStoreError::NotMember);
    };
    let membership: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM workspace_members
          WHERE workspace_id = $1 AND user_id = $2
            FOR KEY SHARE",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if membership.is_none() {
        return Err(GrantStoreError::NotMember);
    }
    Ok(())
}

/// `LastOwner` when the doc is restricted and `principal` holds its only
/// `user:` owner grant, which the caller is about to remove or downgrade.
/// Only `user:` grants of current members of the doc's workspace count:
/// ACL resolution honours no others, so a grant left behind by a removed
/// member is nobody's way to manage the doc.
///
/// Locks the doc row first, so concurrent grant changes on the doc and
/// `DocStore::set_restricted` take turns: two owners can't each remove
/// the other's grant because both saw a second owner.
async fn refuse_last_owner_removal(
    tx: &mut sqlx::PgConnection,
    doc_id: Uuid,
    principal: &str,
) -> Result<(), GrantStoreError> {
    let doc_row: Option<(bool, Uuid)> =
        sqlx::query_as("SELECT restricted, workspace_id FROM documents WHERE id = $1 FOR UPDATE")
            .bind(doc_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((true, workspace_id)) = doc_row else {
        return Ok(());
    };
    let (removes_owner, other_owner_remains): (bool, bool) = sqlx::query_as(
        "WITH member_owners AS (
             SELECT g.principal FROM document_grants g
               JOIN workspace_members m
                 ON m.workspace_id = $3 AND g.principal = 'user:' || m.user_id::text
              WHERE g.doc_id = $1 AND g.role = 'owner'
         )
         SELECT EXISTS (SELECT 1 FROM member_owners WHERE principal = $2),
                EXISTS (SELECT 1 FROM member_owners WHERE principal <> $2)",
    )
    .bind(doc_id)
    .bind(principal)
    .bind(workspace_id)
    .fetch_one(&mut *tx)
    .await?;
    if removes_owner && !other_owner_remains {
        return Err(GrantStoreError::LastOwner);
    }
    Ok(())
}
