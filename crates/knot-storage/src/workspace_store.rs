//! Workspace + workspace_members CRUD.
//!
//! v0.1 has one workspace per knot deployment, but the schema is multi-tenant
//! ready. `get_singleton` is the v0.1 convenience for "the workspace".

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
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
}
