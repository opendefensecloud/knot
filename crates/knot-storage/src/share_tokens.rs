//! Public share tokens — anonymous read-only access to a doc.

use async_trait::async_trait;
use base64::Engine;
use rand::RngCore;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ShareStoreError {
    #[error("not found")]
    NotFound,
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, ShareStoreError>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareToken {
    pub id: Uuid,
    pub token: String,
    pub workspace_id: Uuid,
    pub doc_id: Uuid,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait]
pub trait ShareTokenStore: Send + Sync {
    /// Create a new token. Generates 24 random bytes URL-safe-base64-encoded.
    async fn create(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        created_by: Uuid,
    ) -> Result<ShareToken>;

    /// All tokens for a doc that are not revoked, newest first.
    async fn list_active(&self, doc_id: Uuid) -> Result<Vec<ShareToken>>;

    /// Look up a token by its public string. Only returns Some when the
    /// token is alive: not revoked AND (no expiry OR expiry in the future).
    async fn find_alive(&self, token: &str) -> Result<Option<ShareToken>>;

    /// Mark a token revoked. Returns true if a row was updated (the token
    /// existed AND belonged to `doc_id`). Returns false when the token does
    /// not belong to the given doc (cross-doc IDOR guard). Idempotent on
    /// already-revoked tokens (returns false).
    async fn revoke(&self, id: Uuid, doc_id: Uuid) -> Result<bool>;
}

pub struct PgShareTokenStore {
    pool: PgPool,
}

impl PgShareTokenStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn fresh_token() -> String {
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[async_trait]
impl ShareTokenStore for PgShareTokenStore {
    async fn create(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        created_by: Uuid,
    ) -> Result<ShareToken> {
        let id = Uuid::new_v4();
        let token = fresh_token();
        sqlx::query(
            "INSERT INTO share_tokens (id, token, workspace_id, doc_id, expires_at, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(&token)
        .bind(workspace_id)
        .bind(doc_id)
        .bind(expires_at)
        .bind(created_by)
        .execute(&self.pool)
        .await?;

        // Fetch created_at + return the full row.
        let row: (chrono::DateTime<chrono::Utc>,) =
            sqlx::query_as("SELECT created_at FROM share_tokens WHERE id = $1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        Ok(ShareToken {
            id,
            token,
            workspace_id,
            doc_id,
            expires_at,
            revoked_at: None,
            created_by,
            created_at: row.0,
        })
    }

    async fn list_active(&self, doc_id: Uuid) -> Result<Vec<ShareToken>> {
        let rows: Vec<ShareTokenRow> = sqlx::query_as(
            "SELECT id, token, workspace_id, doc_id, expires_at, revoked_at, created_by, created_at \
             FROM share_tokens \
             WHERE doc_id = $1 AND revoked_at IS NULL \
             ORDER BY created_at DESC",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn find_alive(&self, token: &str) -> Result<Option<ShareToken>> {
        let row: Option<ShareTokenRow> = sqlx::query_as(
            "SELECT id, token, workspace_id, doc_id, expires_at, revoked_at, created_by, created_at \
             FROM share_tokens \
             WHERE token = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > NOW()) \
             LIMIT 1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    async fn revoke(&self, id: Uuid, doc_id: Uuid) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE share_tokens SET revoked_at = NOW() \
             WHERE id = $1 AND doc_id = $2 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(doc_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct ShareTokenRow {
    id: Uuid,
    token: String,
    workspace_id: Uuid,
    doc_id: Uuid,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    created_by: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<ShareTokenRow> for ShareToken {
    fn from(r: ShareTokenRow) -> Self {
        Self {
            id: r.id,
            token: r.token,
            workspace_id: r.workspace_id,
            doc_id: r.doc_id,
            expires_at: r.expires_at,
            revoked_at: r.revoked_at,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}
