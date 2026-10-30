//! Workspace + workspace_members CRUD.
//!
//! v0.1 has one workspace per knot deployment, but the schema is multi-tenant
//! ready. `get_singleton` is the v0.1 convenience for "the workspace".

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::invalidations;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: WorkspaceRole,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRole {
    Owner,
    Editor,
    Viewer,
}

impl WorkspaceRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid role: {0}")]
    InvalidRole(String),
}

#[async_trait]
pub trait WorkspaceStore: Send + Sync + 'static {
    async fn create(&self, slug: &str, name: &str) -> Result<Workspace, WorkspaceStoreError>;
    async fn get_singleton(&self) -> Result<Option<Workspace>, WorkspaceStoreError>;
    async fn add_member(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        role: WorkspaceRole,
    ) -> Result<(), WorkspaceStoreError>;
    async fn get_member_role(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<WorkspaceRole>, WorkspaceStoreError>;
    async fn list_members(&self, workspace_id: Uuid) -> Result<Vec<Member>, WorkspaceStoreError>;
    /// Change the member's role, recording a workspace-wide `member-role`
    /// invalidation in the same transaction.
    async fn update_role(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        role: WorkspaceRole,
    ) -> Result<(), WorkspaceStoreError>;
    /// Remove the membership and, in the same transaction, every `user:`
    /// grant the member holds on this workspace's docs (the only owner grant
    /// of a restricted doc included), recording a `member-remove`
    /// invalidation per affected doc and one workspace-wide.
    async fn remove_member(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), WorkspaceStoreError>;
    async fn count_owners(&self, workspace_id: Uuid) -> Result<i64, WorkspaceStoreError>;
}

#[derive(Clone)]
pub struct PgWorkspaceStore {
    pool: PgPool,
}

impl PgWorkspaceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WorkspaceStore for PgWorkspaceStore {
    async fn create(&self, slug: &str, name: &str) -> Result<Workspace, WorkspaceStoreError> {
        let row = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
            "INSERT INTO workspaces (slug, name) VALUES ($1, $2)
             RETURNING id, slug, name, created_at",
        )
        .bind(slug)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(Workspace {
            id: row.0,
            slug: row.1,
            name: row.2,
            created_at: row.3,
        })
    }

    async fn get_singleton(&self) -> Result<Option<Workspace>, WorkspaceStoreError> {
        let row = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
            "SELECT id, slug, name, created_at FROM workspaces ORDER BY created_at LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Workspace {
            id: r.0,
            slug: r.1,
            name: r.2,
            created_at: r.3,
        }))
    }

    async fn add_member(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        role: WorkspaceRole,
    ) -> Result<(), WorkspaceStoreError> {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, user_id, role)
             VALUES ($1, $2, $3)",
        )
        .bind(workspace_id)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_member_role(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<WorkspaceRole>, WorkspaceStoreError> {
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT role FROM workspace_members
             WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(workspace_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => WorkspaceRole::parse(&r.0)
                .ok_or_else(|| WorkspaceStoreError::InvalidRole(r.0))
                .map(Some),
        }
    }

    async fn list_members(&self, workspace_id: Uuid) -> Result<Vec<Member>, WorkspaceStoreError> {
        let rows = sqlx::query_as::<_, (Uuid, String, String, String, DateTime<Utc>)>(
            "SELECT wm.user_id, u.email::text, u.display_name, wm.role, wm.added_at
             FROM workspace_members wm
             JOIN users u ON u.id = wm.user_id
             WHERE wm.workspace_id = $1
             ORDER BY wm.added_at",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(uid, email, name, role, added)| {
                Ok(Member {
                    user_id: uid,
                    email,
                    display_name: name,
                    role: WorkspaceRole::parse(&role)
                        .ok_or_else(|| WorkspaceStoreError::InvalidRole(role))?,
                    added_at: added,
                })
            })
            .collect()
    }

    async fn update_role(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        role: WorkspaceRole,
    ) -> Result<(), WorkspaceStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        sqlx::query(
            "UPDATE workspace_members SET role = $3
             WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(workspace_id)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;
        // The workspace role is the member's base role in every doc outside
        // a restricted subtree: their cached access and open sockets go
        // stale everywhere at once.
        invalidations::record_workspace_in_tx(&mut tx, workspace_id, "member-role").await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_member(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), WorkspaceStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
            .bind(workspace_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        // Grants never outlive the membership: a re-invite or an OIDC
        // re-provision must not revive access to restricted pages.
        let ungranted_doc_ids: Vec<Uuid> = sqlx::query_scalar(
            "DELETE FROM document_grants g
             USING documents d
             WHERE g.doc_id = d.id AND d.workspace_id = $1 AND g.principal = $2
             RETURNING g.doc_id",
        )
        .bind(workspace_id)
        .bind(format!("user:{user_id}"))
        .fetch_all(&mut *tx)
        .await?;
        for doc_id in ungranted_doc_ids {
            invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "member-remove").await?;
        }
        // The membership itself gave access to every unrestricted doc, which
        // no doc-scoped invalidation names (a member without grants has
        // none): drop it everywhere, and close their open sockets.
        invalidations::record_workspace_in_tx(&mut tx, workspace_id, "member-remove").await?;
        tx.commit().await?;
        Ok(())
    }

    async fn count_owners(&self, workspace_id: Uuid) -> Result<i64, WorkspaceStoreError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM workspace_members
             WHERE workspace_id = $1 AND role = 'owner'",
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }
}
