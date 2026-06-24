//! Session rows — 32-byte primary key (`bytea`), TTL by `expires_at`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::net::IpAddr;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: Vec<u8>,
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub user_agent: Option<String>,
    pub ip: Option<IpAddr>,
}

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Create a new session. `id` is the at-rest session key — the auth layer
    /// passes the keyed HMAC of the cookie token, not the raw token.
    async fn create(
        &self,
        id: &[u8],
        user_id: Uuid,
        workspace_id: Uuid,
        expires_at: DateTime<Utc>,
        user_agent: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<Session, SessionStoreError>;

    /// Find a non-expired session by id.
    async fn find_active(&self, id: &[u8]) -> Result<Option<Session>, SessionStoreError>;

    /// Bump `last_seen_at = now()`; ignored if the row doesn't exist.
    async fn touch(&self, id: &[u8]) -> Result<(), SessionStoreError>;

    /// Delete a session row.
    async fn delete(&self, id: &[u8]) -> Result<(), SessionStoreError>;

    /// Delete every session belonging to a user (e.g. on password change).
    async fn delete_for_user(&self, user_id: Uuid) -> Result<(), SessionStoreError>;
}

#[derive(Clone)]
pub struct PgSessionStore {
    pool: PgPool,
}

impl PgSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

type SessionRow = (
    Vec<u8>,
    Uuid,
    Uuid,
    DateTime<Utc>,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<String>,
    Option<ipnetwork::IpNetwork>,
);

fn session_from_row(r: SessionRow) -> Session {
    Session {
        id: r.0,
        user_id: r.1,
        workspace_id: r.2,
        created_at: r.3,
        expires_at: r.4,
        last_seen_at: r.5,
        user_agent: r.6,
        ip: r.7.map(|net| net.ip()),
    }
}

#[async_trait]
impl SessionStore for PgSessionStore {
    async fn create(
        &self,
        id: &[u8],
        user_id: Uuid,
        workspace_id: Uuid,
        expires_at: DateTime<Utc>,
        user_agent: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<Session, SessionStoreError> {
        let ip_net: Option<ipnetwork::IpNetwork> = ip.map(ipnetwork::IpNetwork::from);
        let row = sqlx::query_as::<_, SessionRow>(
            "INSERT INTO sessions (id, user_id, workspace_id, expires_at, user_agent, ip)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id, user_id, workspace_id, created_at, expires_at, last_seen_at,
                       user_agent, ip",
        )
        .bind(id)
        .bind(user_id)
        .bind(workspace_id)
        .bind(expires_at)
        .bind(user_agent)
        .bind(ip_net)
        .fetch_one(&self.pool)
        .await?;
        Ok(session_from_row(row))
    }

    async fn find_active(&self, id: &[u8]) -> Result<Option<Session>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            "SELECT id, user_id, workspace_id, created_at, expires_at, last_seen_at,
                    user_agent, ip
             FROM sessions WHERE id = $1 AND expires_at > now()",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(session_from_row))
    }

    async fn touch(&self, id: &[u8]) -> Result<(), SessionStoreError> {
        sqlx::query("UPDATE sessions SET last_seen_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete(&self, id: &[u8]) -> Result<(), SessionStoreError> {
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_for_user(&self, user_id: Uuid) -> Result<(), SessionStoreError> {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
